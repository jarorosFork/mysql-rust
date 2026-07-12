//! Executes parsed statements against the storage layer.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::query::parser::{
    ColumnDef, CompareOp, Condition, Expr, SelectItem, ShowStatement, Statement,
};
use crate::storage::{ColumnSchema, ColumnType, Storage, Value};
use crate::{Error, Result};

/// A single row in a result set: one typed value per column.
pub type Row = Vec<Value>;

/// The outcome of executing a statement. For `CREATE TABLE` / `INSERT` /
/// transaction control, `columns` is empty and `rows_affected` carries the
/// count; a `SELECT` always has at least one projected column. Values stay
/// typed (not pre-stringified) so the connection can encode them in either
/// the text or the binary protocol and report accurate column type codes.
#[derive(Debug, Default)]
pub struct QueryResult {
    pub columns: Vec<ColumnSchema>,
    pub rows: Vec<Row>,
    pub rows_affected: u64,
}

impl QueryResult {
    /// The projected column names, in order — a convenience for callers
    /// (and tests) that don't care about the types.
    pub fn column_names(&self) -> Vec<String> {
        self.columns.iter().map(|c| c.name.clone()).collect()
    }
}

/// MySQL's default `wait_timeout` (8 hours, in seconds) — reported to clients
/// that read `@@wait_timeout` on connect. This server doesn't actually reap
/// idle connections yet, so this is a compatibility value, not an enforced one.
const DEFAULT_WAIT_TIMEOUT: u64 = 28_800;

/// Server system variables surfaced to `SELECT @@...`. Built from [`Config`]
/// (see `server::connection`); covers the handful a standard driver reads on
/// connect (`@@max_allowed_packet`, `@@wait_timeout`) plus the version string.
///
/// [`Config`]: crate::config::Config
#[derive(Debug, Clone)]
pub struct SystemVariables {
    pub version: String,
    pub max_allowed_packet: u64,
    pub wait_timeout: u64,
}

impl SystemVariables {
    /// Build from the configured version string and `max_allowed_packet`,
    /// defaulting `wait_timeout` to MySQL's 8-hour default.
    pub fn new(version: impl Into<String>, max_allowed_packet: u64) -> Self {
        SystemVariables {
            version: version.into(),
            max_allowed_packet,
            wait_timeout: DEFAULT_WAIT_TIMEOUT,
        }
    }
}

/// Runs statements against a [`Storage`] backend.
pub struct Executor<'a> {
    storage: &'a dyn Storage,
    vars: &'a SystemVariables,
}

impl<'a> Executor<'a> {
    pub fn new(storage: &'a dyn Storage, vars: &'a SystemVariables) -> Self {
        Executor { storage, vars }
    }

    pub fn execute(&self, statement: Statement) -> Result<QueryResult> {
        match statement {
            Statement::CreateTable { table, columns } => self.execute_create_table(&table, columns),
            Statement::Insert {
                table,
                columns,
                rows,
            } => self.execute_insert(&table, columns, rows),
            Statement::Select {
                projection,
                from,
                selection,
            } => self.execute_select(projection, from, selection),
            // Transaction control is handled by `Connection` before a
            // statement ever reaches an `Executor` (it needs to switch
            // which `Storage` implementation subsequent statements use —
            // see `server::connection::Connection::execute_sql`). Reaching
            // here means that dispatch was skipped somehow.
            Statement::Begin | Statement::Commit | Statement::Rollback => Err(Error::Execution(
                "transaction control statements must be handled by the connection layer"
                    .to_string(),
            )),
            // Client/session boilerplate we accept and acknowledge with an OK
            // (see the parser): session settings and database selection are
            // not modelled, so they have no effect but must not error.
            Statement::Set | Statement::Use => Ok(QueryResult::default()),
            Statement::Show(show) => self.execute_show(show),
        }
    }

    /// Execute a `SHOW` — enough introspection that GUI clients don't error.
    /// Unmodelled forms return an empty result set.
    fn execute_show(&self, show: ShowStatement) -> Result<QueryResult> {
        let text_col = |name: &str| ColumnSchema {
            name: name.to_string(),
            column_type: ColumnType::Varchar,
        };
        match show {
            ShowStatement::Databases => Ok(QueryResult {
                columns: vec![text_col("Database")],
                rows: Vec::new(),
                rows_affected: 0,
            }),
            ShowStatement::Tables => {
                let mut names = self.storage.tables()?;
                names.sort();
                Ok(QueryResult {
                    columns: vec![text_col("Tables_in_mysql_rust")],
                    rows: names.into_iter().map(|t| vec![Value::Varchar(t)]).collect(),
                    rows_affected: 0,
                })
            }
            ShowStatement::Warnings => Ok(QueryResult {
                columns: vec![text_col("Level"), text_col("Code"), text_col("Message")],
                rows: Vec::new(),
                rows_affected: 0,
            }),
            ShowStatement::Variables { like } => {
                let rows = self
                    .known_variables()
                    .into_iter()
                    .filter(|(name, _)| match &like {
                        Some(pattern) => like_matches(pattern, name),
                        None => true,
                    })
                    .map(|(name, value)| vec![Value::Varchar(name.to_string()), value])
                    .collect();
                Ok(QueryResult {
                    columns: vec![text_col("Variable_name"), text_col("Value")],
                    rows,
                    rows_affected: 0,
                })
            }
            ShowStatement::Other => Ok(QueryResult::default()),
        }
    }

    fn execute_create_table(&self, table: &str, columns: Vec<ColumnDef>) -> Result<QueryResult> {
        let mut primary_key = None;
        let mut schema_columns = Vec::with_capacity(columns.len());
        for col in columns {
            let column_type = ColumnType::from_name(&col.type_name).ok_or_else(|| {
                Error::Execution(format!(
                    "Unknown column type '{}' for column '{}'",
                    col.type_name, col.name
                ))
            })?;
            if col.is_primary_key {
                if primary_key.is_some() {
                    return Err(Error::Execution(
                        "multiple primary key columns are not supported".to_string(),
                    ));
                }
                primary_key = Some(col.name.clone());
            }
            schema_columns.push(ColumnSchema {
                name: col.name,
                column_type,
            });
        }

        self.storage
            .create_table(table, schema_columns, primary_key)?;
        Ok(QueryResult::default())
    }

    fn execute_insert(
        &self,
        table: &str,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Expr>>,
    ) -> Result<QueryResult> {
        let schema = self.storage.table_schema(table)?;

        let mut affected = 0u64;
        for row in rows {
            let ordered_exprs = match &columns {
                Some(cols) => reorder_exprs(&schema.columns, cols, row)?,
                None => row,
            };

            if ordered_exprs.len() != schema.columns.len() {
                return Err(Error::Execution(format!(
                    "Column count doesn't match value count: table '{table}' has {} column(s), got {}",
                    schema.columns.len(),
                    ordered_exprs.len()
                )));
            }

            let mut values = Vec::with_capacity(ordered_exprs.len());
            for (expr, col) in ordered_exprs.iter().zip(schema.columns.iter()) {
                let value = coerce(expr, col.column_type, &col.name)?;
                if value == Value::Null && schema.primary_key.as_deref() == Some(col.name.as_str())
                {
                    return Err(Error::Execution(format!(
                        "Column '{}' cannot be NULL (primary key)",
                        col.name
                    )));
                }
                values.push(value);
            }

            self.storage.insert_row(table, values)?;
            affected += 1;
        }

        Ok(QueryResult {
            rows_affected: affected,
            ..QueryResult::default()
        })
    }

    fn execute_select(
        &self,
        projection: Vec<SelectItem>,
        from: Option<String>,
        selection: Option<Condition>,
    ) -> Result<QueryResult> {
        match from {
            None => self.execute_select_without_table(projection),
            Some(table) => self.execute_select_from_table(&table, projection, selection),
        }
    }

    /// `SELECT <expr-list>` with no `FROM` — literals, `NULL`, and system
    /// variables only.
    fn execute_select_without_table(&self, projection: Vec<SelectItem>) -> Result<QueryResult> {
        let mut columns = Vec::with_capacity(projection.len());
        let mut values = Vec::with_capacity(projection.len());

        for item in projection {
            let (expr, alias) = match item {
                SelectItem::Wildcard => {
                    return Err(Error::Execution(
                        "SELECT * requires a FROM clause".to_string(),
                    ));
                }
                SelectItem::Expr(expr, alias) => (expr, alias),
            };

            let (default_name, value) = match expr {
                Expr::Integer(n) => (n.to_string(), Value::Int(n)),
                Expr::String(s) => (s.clone(), Value::Varchar(s)),
                Expr::Null => ("NULL".to_string(), Value::Null),
                Expr::SystemVariable(name) => (format!("@@{name}"), self.system_variable(&name)),
                Expr::Function(name, args) => {
                    (format!("{name}()"), self.evaluate_function(&name, &args))
                }
                // A bare column with no FROM clause is an error, as in MySQL.
                Expr::Column(name) => {
                    return Err(Error::Execution(format!(
                        "Unknown column '{name}' in 'field list'"
                    )));
                }
                Expr::Placeholder(_) => {
                    return Err(Error::Execution(
                        "unbound '?' parameter reached the executor".to_string(),
                    ));
                }
            };
            // Numeric values (e.g. @@max_allowed_packet) are reported as an INT
            // column so clients read them as numbers, not strings.
            let column_type = match &value {
                Value::Int(_) => ColumnType::Int,
                _ => ColumnType::Varchar,
            };
            columns.push(ColumnSchema {
                name: alias.unwrap_or(default_name),
                column_type,
            });
            values.push(value);
        }

        Ok(QueryResult {
            columns,
            rows: vec![values],
            rows_affected: 0,
        })
    }

    /// Resolve a `@@name` system variable to a typed value. Scope prefixes
    /// (`@@session.`, `@@global.`, `@@local.`) are accepted and stripped.
    /// Unknown variables resolve to `NULL` rather than erroring, so a client's
    /// connect-time batch of `@@…` reads never fails on one we didn't model.
    fn system_variable(&self, name: &str) -> Value {
        let lower = name.to_ascii_lowercase();
        let bare = lower
            .strip_prefix("session.")
            .or_else(|| lower.strip_prefix("global."))
            .or_else(|| lower.strip_prefix("local."))
            .unwrap_or(lower.as_str());
        self.known_variables()
            .into_iter()
            .find(|(n, _)| *n == bare)
            .map(|(_, v)| v)
            .unwrap_or(Value::Null)
    }

    /// The system variables this server reports, with plausible values — the
    /// set a standard client (JDBC/GUI) reads on connect. Backs both
    /// `@@variable` reads and `SHOW VARIABLES`.
    fn known_variables(&self) -> Vec<(&'static str, Value)> {
        // Clamp to i64 defensively so a huge configured value can't wrap.
        let as_int = |v: u64| Value::Int(v.min(i64::MAX as u64) as i64);
        let s = |v: &str| Value::Varchar(v.to_string());
        vec![
            ("version", Value::Varchar(self.vars.version.clone())),
            ("version_comment", s("mysql-rust")),
            ("max_allowed_packet", as_int(self.vars.max_allowed_packet)),
            ("wait_timeout", as_int(self.vars.wait_timeout)),
            ("interactive_timeout", as_int(self.vars.wait_timeout)),
            ("net_write_timeout", Value::Int(60)),
            ("net_read_timeout", Value::Int(30)),
            ("autocommit", Value::Int(1)),
            ("auto_increment_increment", Value::Int(1)),
            ("character_set_client", s("utf8mb4")),
            ("character_set_connection", s("utf8mb4")),
            ("character_set_results", s("utf8mb4")),
            ("character_set_server", s("utf8mb4")),
            ("collation_server", s("utf8mb4_general_ci")),
            ("collation_connection", s("utf8mb4_general_ci")),
            ("init_connect", s("")),
            ("license", s("MIT OR Apache-2.0")),
            ("lower_case_table_names", Value::Int(0)),
            ("performance_schema", Value::Int(0)),
            ("query_cache_size", Value::Int(0)),
            ("query_cache_type", s("OFF")),
            ("have_query_cache", s("NO")),
            ("sql_mode", s("")),
            ("system_time_zone", s("UTC")),
            ("time_zone", s("SYSTEM")),
            ("transaction_isolation", s("READ-COMMITTED")),
            ("tx_isolation", s("READ-COMMITTED")),
            ("transaction_read_only", Value::Int(0)),
            // TCP-only server: there is no unix socket to report.
            ("socket", Value::Null),
        ]
    }

    /// Evaluate a small set of no-argument informational functions clients use
    /// (`DATABASE()`, `VERSION()`, ...). Unknown functions resolve to `NULL`.
    fn evaluate_function(&self, name: &str, _args: &[Expr]) -> Value {
        match name.to_ascii_lowercase().as_str() {
            // Schemaless server: no current database.
            "database" | "schema" => Value::Null,
            "version" => Value::Varchar(self.vars.version.clone()),
            "connection_id" => Value::Int(1),
            "last_insert_id" => Value::Int(0),
            _ => Value::Null,
        }
    }

    fn execute_select_from_table(
        &self,
        table: &str,
        projection: Vec<SelectItem>,
        selection: Option<Condition>,
    ) -> Result<QueryResult> {
        let schema = self.storage.table_schema(table)?;
        let selected_indices = resolve_projection(&schema.columns, &projection)?;

        let matching_rows: Vec<Vec<Value>> = match &selection {
            None => self.storage.scan(table)?,
            Some(cond) => {
                let col_idx = column_index(&schema.columns, &cond.column)?;
                let column_type = schema.columns[col_idx].column_type;
                let expected = coerce(&cond.value, column_type, &cond.column)?;

                let is_pk_equality = cond.op == CompareOp::Eq
                    && schema.primary_key.as_deref() == Some(cond.column.as_str());
                if is_pk_equality {
                    // Indexed point lookup instead of a full scan.
                    self.storage
                        .lookup_by_primary_key(table, &expected)?
                        .into_iter()
                        .collect()
                } else {
                    self.storage
                        .scan(table)?
                        .into_iter()
                        .filter(|row| compare_values(&row[col_idx], cond.op, &expected))
                        .collect()
                }
            }
        };

        let columns = selected_indices
            .iter()
            .map(|&i| schema.columns[i].clone())
            .collect();
        let rows = matching_rows
            .into_iter()
            .map(|row| selected_indices.iter().map(|&i| row[i].clone()).collect())
            .collect();

        Ok(QueryResult {
            columns,
            rows,
            rows_affected: 0,
        })
    }
}

fn resolve_projection(
    table_columns: &[ColumnSchema],
    projection: &[SelectItem],
) -> Result<Vec<usize>> {
    if let [SelectItem::Wildcard] = projection {
        return Ok((0..table_columns.len()).collect());
    }

    let mut indices = Vec::with_capacity(projection.len());
    for item in projection {
        match item {
            SelectItem::Wildcard => {
                return Err(Error::Execution(
                    "'*' cannot be combined with other selected columns".to_string(),
                ));
            }
            SelectItem::Expr(Expr::Column(name), _) => {
                indices.push(column_index(table_columns, name)?)
            }
            SelectItem::Expr(_, _) => {
                return Err(Error::Unsupported(
                    "literal expressions in a SELECT list alongside a FROM clause",
                ));
            }
        }
    }
    Ok(indices)
}

/// A minimal MySQL `LIKE` matcher for `SHOW ... LIKE '<pattern>'`: `%` matches
/// any run of characters, `_` matches one. Case-insensitive, which is enough
/// for the variable-name lookups clients use.
fn like_matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let t: Vec<char> = text.to_ascii_lowercase().chars().collect();
    // Classic dynamic-programming wildcard match.
    let (np, nt) = (p.len(), t.len());
    let mut dp = vec![vec![false; nt + 1]; np + 1];
    dp[0][0] = true;
    for i in 1..=np {
        if p[i - 1] == '%' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=np {
        for j in 1..=nt {
            dp[i][j] = match p[i - 1] {
                '%' => dp[i - 1][j] || dp[i][j - 1],
                '_' => dp[i - 1][j - 1],
                c => dp[i - 1][j - 1] && c == t[j - 1],
            };
        }
    }
    dp[np][nt]
}

fn column_index(table_columns: &[ColumnSchema], name: &str) -> Result<usize> {
    table_columns
        .iter()
        .position(|c| c.name == name)
        .ok_or_else(|| Error::Execution(format!("Unknown column '{name}'")))
}

/// Build a row of expressions in the table's actual column order from an
/// `INSERT`'s explicit `(col, ...) VALUES (...)` list.
fn reorder_exprs(
    table_columns: &[ColumnSchema],
    insert_columns: &[String],
    values: Vec<Expr>,
) -> Result<Vec<Expr>> {
    if insert_columns.len() != values.len() {
        return Err(Error::Execution(format!(
            "Column count doesn't match value count: {} column(s) named, {} value(s) given",
            insert_columns.len(),
            values.len()
        )));
    }

    for col in insert_columns {
        if !table_columns.iter().any(|c| &c.name == col) {
            return Err(Error::Execution(format!(
                "Unknown column '{col}' in field list"
            )));
        }
    }

    let mut named: HashMap<&str, Expr> = HashMap::new();
    for (col, expr) in insert_columns.iter().zip(values) {
        named.insert(col.as_str(), expr);
    }

    table_columns
        .iter()
        .map(|col| {
            named.remove(col.name.as_str()).ok_or_else(|| {
                Error::Execution(format!(
                    "Column '{}' has no default value and was not given a value",
                    col.name
                ))
            })
        })
        .collect()
}

/// Coerce a parsed literal into a typed storage [`Value`] for `column`,
/// following MySQL's permissive-but-checked conversions: a numeric string
/// into an INT column is parsed, an integer into a VARCHAR column is
/// stringified, and `NULL` is always allowed at this stage (primary-key
/// not-null is enforced by the caller).
fn coerce(expr: &Expr, column_type: ColumnType, column_name: &str) -> Result<Value> {
    match (expr, column_type) {
        (Expr::Null, _) => Ok(Value::Null),
        (Expr::Integer(n), ColumnType::Int) => Ok(Value::Int(*n)),
        (Expr::Integer(n), ColumnType::Varchar) => Ok(Value::Varchar(n.to_string())),
        (Expr::String(s), ColumnType::Varchar) => Ok(Value::Varchar(s.clone())),
        (Expr::String(s), ColumnType::Int) => s.parse::<i64>().map(Value::Int).map_err(|_| {
            Error::Execution(format!(
                "Incorrect integer value: '{s}' for column '{column_name}'"
            ))
        }),
        (Expr::SystemVariable(_) | Expr::Column(_) | Expr::Function(..), _) => {
            Err(Error::Execution(format!(
                "a literal value is required for column '{column_name}'"
            )))
        }
        // Placeholders are always replaced with literals by
        // `parser::bind_parameters` before execution; one reaching here means
        // a prepared statement was executed without binding its parameters.
        (Expr::Placeholder(_), _) => Err(Error::Execution(
            "unbound '?' parameter reached the executor".to_string(),
        )),
    }
}

/// Compare two typed values. SQL three-valued logic: any comparison
/// involving `NULL` is never true (not even `NULL = NULL`).
fn compare_values(actual: &Value, op: CompareOp, expected: &Value) -> bool {
    let ordering = match (actual, expected) {
        (Value::Null, _) | (_, Value::Null) => return false,
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        (Value::Varchar(a), Value::Varchar(b)) => a.cmp(b),
        // Mixed Int/Varchar: compare by display text (best-effort; real
        // MySQL has more nuanced coercion rules than this subset needs).
        (a, b) => a
            .to_display_string()
            .unwrap_or_default()
            .cmp(&b.to_display_string().unwrap_or_default()),
    };
    match op {
        CompareOp::Eq => ordering == Ordering::Equal,
        CompareOp::NotEq => ordering != Ordering::Equal,
        CompareOp::Lt => ordering == Ordering::Less,
        CompareOp::Gt => ordering == Ordering::Greater,
        CompareOp::Le => ordering != Ordering::Greater,
        CompareOp::Ge => ordering != Ordering::Less,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::parser::parse;
    use crate::storage::InMemoryStorage;

    fn test_vars() -> SystemVariables {
        SystemVariables::new("8.0.0-mysql-rust-test", 64 * 1024 * 1024)
    }

    fn run(storage: &InMemoryStorage, sql: &str) -> Result<QueryResult> {
        let vars = test_vars();
        Executor::new(storage, &vars).execute(parse(sql)?)
    }

    fn int(n: i64) -> Value {
        Value::Int(n)
    }

    fn vc(s: &str) -> Value {
        Value::Varchar(s.to_string())
    }

    #[test]
    fn select_1_returns_a_single_row() {
        let storage = InMemoryStorage::new();
        let result = run(&storage, "SELECT 1").expect("execute");
        assert_eq!(result.column_names(), vec!["1"]);
        assert_eq!(result.rows, vec![vec![int(1)]]);
        // The literal 1 is reported as an integer column, not a string.
        assert_eq!(result.columns[0].column_type, ColumnType::Int);
    }

    #[test]
    fn select_1_is_case_and_whitespace_insensitive() {
        let storage = InMemoryStorage::new();
        for sql in ["select 1", "  SELECT 1  ", "SELECT 1;", "Select 1 ;"] {
            let result =
                run(&storage, sql).unwrap_or_else(|e| panic!("execute({sql:?}) failed: {e}"));
            assert_eq!(result.rows, vec![vec![int(1)]]);
        }
    }

    #[test]
    fn select_null_literal_is_a_null_value_not_the_text_null() {
        let storage = InMemoryStorage::new();
        let result = run(&storage, "SELECT NULL").expect("execute");
        assert_eq!(result.column_names(), vec!["NULL"]); // display header only
        assert_eq!(result.rows, vec![vec![Value::Null]]);
    }

    #[test]
    fn select_version_returns_configured_server_version() {
        let storage = InMemoryStorage::new();
        let result = run(&storage, "SELECT @@version").expect("execute");
        assert_eq!(result.column_names(), vec!["@@version"]);
        assert_eq!(result.rows, vec![vec![vc("8.0.0-mysql-rust-test")]]);
    }

    #[test]
    fn unknown_system_variable_resolves_to_null() {
        // Lenient by design: an unknown @@var yields NULL, not an error, so a
        // client's connect-time batch of variable reads never fails on one we
        // don't model.
        let storage = InMemoryStorage::new();
        let result = run(&storage, "SELECT @@bogus").expect("execute");
        assert_eq!(result.rows, vec![vec![Value::Null]]);
    }

    #[test]
    fn scope_qualified_and_aliased_system_variables() {
        let storage = InMemoryStorage::new();
        let result = run(&storage, "SELECT @@session.max_allowed_packet AS m").expect("execute");
        assert_eq!(result.column_names(), vec!["m"]);
        assert_eq!(result.rows, vec![vec![int(64 * 1024 * 1024)]]);
    }

    #[test]
    fn set_use_and_show_are_accepted() {
        let storage = InMemoryStorage::new();
        run(&storage, "SET NAMES utf8mb4").expect("SET");
        run(&storage, "SET @@session.autocommit = 1").expect("SET session");
        run(&storage, "USE mydb").expect("USE");
        run(&storage, "CREATE TABLE t (a INT)").expect("create");
        let tables = run(&storage, "SHOW TABLES").expect("SHOW TABLES");
        assert_eq!(tables.rows, vec![vec![vc("t")]]);
        let warnings = run(&storage, "SHOW WARNINGS").expect("SHOW WARNINGS");
        assert!(warnings.rows.is_empty());
        assert_eq!(warnings.column_names(), vec!["Level", "Code", "Message"]);
    }

    #[test]
    fn show_variables_like_filters() {
        let storage = InMemoryStorage::new();
        let result = run(&storage, "SHOW VARIABLES LIKE 'max_allowed%'").expect("SHOW VARIABLES");
        assert_eq!(result.column_names(), vec!["Variable_name", "Value"]);
        assert_eq!(
            result.rows,
            vec![vec![vc("max_allowed_packet"), int(64 * 1024 * 1024)]]
        );
    }

    #[test]
    fn database_function_is_null_without_a_schema() {
        let storage = InMemoryStorage::new();
        let result = run(&storage, "SELECT DATABASE()").expect("execute");
        assert_eq!(result.rows, vec![vec![Value::Null]]);
    }

    #[test]
    fn connect_time_system_variables_resolve_with_types() {
        // The exact multi-variable query mysql_async issues on connect.
        let storage = InMemoryStorage::new();
        let result = run(
            &storage,
            "SELECT @@max_allowed_packet,@@wait_timeout,@@socket",
        )
        .expect("execute");
        assert_eq!(
            result.column_names(),
            vec!["@@max_allowed_packet", "@@wait_timeout", "@@socket"]
        );
        // max_allowed_packet and wait_timeout are numeric; socket is NULL.
        assert_eq!(result.columns[0].column_type, ColumnType::Int);
        assert_eq!(result.columns[1].column_type, ColumnType::Int);
        assert_eq!(
            result.rows,
            vec![vec![int(64 * 1024 * 1024), int(28_800), Value::Null]]
        );
    }

    #[test]
    fn create_table_then_select_star_is_empty() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT, b VARCHAR)").expect("create");
        let result = run(&storage, "SELECT * FROM t").expect("select");
        assert_eq!(result.column_names(), vec!["a", "b"]);
        // Column types are reported per the schema: INT then VARCHAR.
        assert_eq!(result.columns[0].column_type, ColumnType::Int);
        assert_eq!(result.columns[1].column_type, ColumnType::Varchar);
        assert!(result.rows.is_empty());
    }

    #[test]
    fn create_table_rejects_duplicate() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT)").expect("create");
        assert!(matches!(
            run(&storage, "CREATE TABLE t (a INT)"),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn create_table_rejects_unknown_type() {
        let storage = InMemoryStorage::new();
        assert!(matches!(
            run(&storage, "CREATE TABLE t (a BOGUS)"),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn create_table_rejects_two_primary_keys() {
        let storage = InMemoryStorage::new();
        assert!(matches!(
            run(
                &storage,
                "CREATE TABLE t (a INT PRIMARY KEY, b INT PRIMARY KEY)"
            ),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn insert_then_select_returns_the_row() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT, b VARCHAR)").expect("create");
        let inserted =
            run(&storage, "INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y')").expect("insert");
        assert_eq!(inserted.rows_affected, 2);

        let result = run(&storage, "SELECT * FROM t").expect("select");
        assert_eq!(
            result.rows,
            vec![vec![int(1), vc("x")], vec![int(2), vc("y")]]
        );
    }

    #[test]
    fn insert_without_explicit_columns_uses_table_order() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT, b VARCHAR)").expect("create");
        run(&storage, "INSERT INTO t VALUES (1, 'x')").expect("insert");
        let result = run(&storage, "SELECT * FROM t").expect("select");
        assert_eq!(result.rows, vec![vec![int(1), vc("x")]]);
    }

    #[test]
    fn insert_with_reordered_columns_places_values_correctly() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT, b VARCHAR)").expect("create");
        run(&storage, "INSERT INTO t (b, a) VALUES ('x', 1)").expect("insert");
        let result = run(&storage, "SELECT a, b FROM t").expect("select");
        assert_eq!(result.rows, vec![vec![int(1), vc("x")]]);
    }

    #[test]
    fn insert_rejects_unknown_column() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT)").expect("create");
        assert!(matches!(
            run(&storage, "INSERT INTO t (bogus) VALUES (1)"),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn insert_rejects_missing_columns_in_partial_list() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT, b VARCHAR)").expect("create");
        assert!(matches!(
            run(&storage, "INSERT INTO t (a) VALUES (1)"),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn insert_into_missing_table_errors() {
        let storage = InMemoryStorage::new();
        assert!(matches!(
            run(&storage, "INSERT INTO missing VALUES (1)"),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn insert_null_into_ordinary_column_is_allowed() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT, b VARCHAR)").expect("create");
        run(&storage, "INSERT INTO t VALUES (1, NULL)").expect("insert");
        let result = run(&storage, "SELECT * FROM t").expect("select");
        assert_eq!(result.rows, vec![vec![int(1), Value::Null]]);
    }

    #[test]
    fn insert_null_into_primary_key_is_rejected() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (id INT PRIMARY KEY)").expect("create");
        assert!(matches!(
            run(&storage, "INSERT INTO t VALUES (NULL)"),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn insert_coerces_numeric_string_into_int_column() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT)").expect("create");
        run(&storage, "INSERT INTO t VALUES ('42')").expect("insert");
        let result = run(&storage, "SELECT * FROM t").expect("select");
        assert_eq!(result.rows, vec![vec![int(42)]]);
    }

    #[test]
    fn insert_coerces_integer_into_varchar_column() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a VARCHAR)").expect("create");
        run(&storage, "INSERT INTO t VALUES (42)").expect("insert");
        let result = run(&storage, "SELECT * FROM t").expect("select");
        assert_eq!(result.rows, vec![vec![vc("42")]]);
    }

    #[test]
    fn insert_rejects_non_numeric_string_into_int_column() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT)").expect("create");
        assert!(matches!(
            run(&storage, "INSERT INTO t VALUES ('not-a-number')"),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn insert_rejects_duplicate_primary_key() {
        let storage = InMemoryStorage::new();
        run(
            &storage,
            "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR)",
        )
        .expect("create");
        run(&storage, "INSERT INTO t VALUES (1, 'alice')").expect("insert");
        assert!(matches!(
            run(&storage, "INSERT INTO t VALUES (1, 'bob')"),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn select_with_projection_and_where_equality() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (id INT, name VARCHAR)").expect("create");
        run(
            &storage,
            "INSERT INTO t (id, name) VALUES (1, 'alice'), (2, 'bob')",
        )
        .expect("insert");

        let result = run(&storage, "SELECT name FROM t WHERE id = 2").expect("select");
        assert_eq!(result.column_names(), vec!["name"]);
        assert_eq!(result.rows, vec![vec![vc("bob")]]);
    }

    #[test]
    fn where_equality_on_primary_key_uses_indexed_lookup_and_is_correct() {
        let storage = InMemoryStorage::new();
        run(
            &storage,
            "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR)",
        )
        .expect("create");
        run(
            &storage,
            "INSERT INTO t VALUES (1, 'alice'), (2, 'bob'), (3, 'carol')",
        )
        .expect("insert");

        let result = run(&storage, "SELECT name FROM t WHERE id = 2").expect("select");
        assert_eq!(result.rows, vec![vec![vc("bob")]]);

        let miss = run(&storage, "SELECT name FROM t WHERE id = 999").expect("select");
        assert!(miss.rows.is_empty());
    }

    #[test]
    fn where_compares_numerically_not_lexically() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (id INT)").expect("create");
        for v in ["1", "2", "9", "10", "20"] {
            run(&storage, &format!("INSERT INTO t VALUES ({v})")).expect("insert");
        }

        // Lexically, "10" and "20" < "9"; numerically they must sort after it.
        let result = run(&storage, "SELECT id FROM t WHERE id > 9").expect("select");
        let mut got: Vec<i64> = result
            .rows
            .into_iter()
            .map(|r| match &r[0] {
                Value::Int(n) => *n,
                other => panic!("expected an integer, got {other:?}"),
            })
            .collect();
        got.sort();
        assert_eq!(got, vec![10, 20]);
    }

    #[test]
    fn where_null_column_never_matches_even_equality() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT, b VARCHAR)").expect("create");
        run(&storage, "INSERT INTO t VALUES (1, NULL)").expect("insert");

        // A NULL column value can't satisfy any comparison, even against NULL.
        assert!(run(&storage, "SELECT * FROM t WHERE b = NULL")
            .expect("select")
            .rows
            .is_empty());
    }

    #[test]
    fn select_from_missing_table_errors() {
        let storage = InMemoryStorage::new();
        assert!(matches!(
            run(&storage, "SELECT * FROM missing"),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn select_unknown_column_errors() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT)").expect("create");
        assert!(matches!(
            run(&storage, "SELECT bogus FROM t"),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn select_unknown_column_in_where_errors() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT)").expect("create");
        assert!(matches!(
            run(&storage, "SELECT a FROM t WHERE bogus = 1"),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn wildcard_mixed_with_column_errors() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT)").expect("create");
        assert!(matches!(
            run(&storage, "SELECT *, a FROM t"),
            Err(Error::Execution(_))
        ));
    }
}
