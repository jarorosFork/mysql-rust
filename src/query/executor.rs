//! Executes parsed statements against the storage layer.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::query::parser::{
    ColumnDef, ColumnRef, CompareOp, Condition, Expr, FromClause, JoinType, OrderByItem,
    SelectItem, ShowStatement, Statement,
};
use crate::storage::{format_decimal, ColumnSchema, ColumnType, Storage, TableSchema, Value};
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

    pub async fn execute(&self, statement: Statement) -> Result<QueryResult> {
        match statement {
            Statement::CreateTable { table, columns } => {
                self.execute_create_table(&table, columns).await
            }
            Statement::Insert {
                table,
                columns,
                rows,
            } => self.execute_insert(&table, columns, rows).await,
            Statement::Select {
                projection,
                from,
                selection,
                group_by,
                order_by,
                limit,
                offset,
            } => self.execute_select(
                projection, from, selection, &group_by, &order_by, limit, offset,
            ),
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
            Statement::CreateDatabase {
                name,
                if_not_exists,
            } => {
                self.storage.create_database(&name, if_not_exists)?;
                Ok(QueryResult::default())
            }
            Statement::DropDatabase { name, if_exists } => {
                self.storage.drop_database(&name, if_exists)?;
                Ok(QueryResult::default())
            }
        }
    }

    /// Execute a `SHOW` — enough introspection that GUI clients don't error.
    /// Unmodelled forms return an empty result set.
    fn execute_show(&self, show: ShowStatement) -> Result<QueryResult> {
        let text_col = |name: &str| ColumnSchema {
            name: name.to_string(),
            column_type: ColumnType::Varchar,
            nullable: true,
            auto_increment: false,
        };
        let int_col = |name: &str| ColumnSchema {
            name: name.to_string(),
            column_type: ColumnType::Int,
            nullable: true,
            auto_increment: false,
        };
        match show {
            ShowStatement::Databases => {
                let mut names = self.storage.databases()?;
                names.sort();
                Ok(QueryResult {
                    columns: vec![text_col("Database")],
                    rows: names.into_iter().map(|d| vec![Value::Varchar(d)]).collect(),
                    rows_affected: 0,
                })
            }
            // A single row describing the one charset/collation this server
            // supports (`utf8mb4`/`utf8mb4_general_ci`, matching the handshake
            // — see `protocol::handshake::DEFAULT_CHARACTER_SET`). Real MySQL
            // lists many; a GUI client populating a charset/collation picker
            // (e.g. a "create database" dialog) just needs a non-empty,
            // correctly-shaped result to pick a default from — an empty result
            // is what previously made such dialogs fail with a client-side
            // null-pointer error before any SQL was even sent.
            ShowStatement::CharacterSet => Ok(QueryResult {
                columns: vec![
                    text_col("Charset"),
                    text_col("Description"),
                    text_col("Default collation"),
                    int_col("Maxlen"),
                ],
                rows: vec![vec![
                    Value::Varchar("utf8mb4".to_string()),
                    Value::Varchar("UTF-8 Unicode".to_string()),
                    Value::Varchar("utf8mb4_general_ci".to_string()),
                    Value::Int(4),
                ]],
                rows_affected: 0,
            }),
            ShowStatement::Collation => Ok(QueryResult {
                columns: vec![
                    text_col("Collation"),
                    text_col("Charset"),
                    int_col("Id"),
                    text_col("Default"),
                    text_col("Compiled"),
                    int_col("Sortlen"),
                ],
                rows: vec![vec![
                    Value::Varchar("utf8mb4_general_ci".to_string()),
                    Value::Varchar("utf8mb4".to_string()),
                    Value::Int(45),
                    Value::Varchar("Yes".to_string()),
                    Value::Varchar("Yes".to_string()),
                    Value::Int(1),
                ]],
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

    async fn execute_create_table(
        &self,
        table: &str,
        columns: Vec<ColumnDef>,
    ) -> Result<QueryResult> {
        let mut primary_key = None;
        let mut auto_increment_column = None;
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
            if col.auto_increment {
                if auto_increment_column.is_some() {
                    return Err(Error::Unsupported(
                        "more than one AUTO_INCREMENT column per table",
                    ));
                }
                auto_increment_column = Some(col.name.clone());
            }
            schema_columns.push(ColumnSchema {
                name: col.name,
                column_type,
                // PRIMARY KEY implies NOT NULL, regardless of how the column
                // was actually declared — matches standard SQL.
                nullable: !col.is_primary_key && col.nullable,
                auto_increment: col.auto_increment,
            });
        }

        // This engine has exactly one index — the primary key — so an
        // AUTO_INCREMENT column (which real MySQL requires to be indexed)
        // must be it.
        if let Some(name) = &auto_increment_column {
            if primary_key.as_deref() != Some(name.as_str()) {
                return Err(Error::Unsupported(
                    "AUTO_INCREMENT on a column that isn't the PRIMARY KEY",
                ));
            }
        }

        self.storage
            .create_table(table, schema_columns, primary_key)
            .await?;
        Ok(QueryResult::default())
    }

    async fn execute_insert(
        &self,
        table: &str,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Expr>>,
    ) -> Result<QueryResult> {
        let schema = self.storage.table_schema(table)?;

        // Coerce every row's values *before* touching storage at all, then
        // insert the whole statement as one atomic batch
        // (PERFORMANCE_DURABILITY_PLAN.md D2, via `Storage::insert_rows`).
        // This also fixes a subtler pre-existing gap than D2's headline
        // crash case: previously, row 1 could already be applied by the
        // time row 3 failed to coerce, so even a plain (no crash involved)
        // multi-row INSERT with a bad value partway through used to leave
        // a partial result. A single statement is now genuinely all-or-
        // nothing.
        let mut batch = Vec::with_capacity(rows.len());
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
                let mut value = coerce(expr, col.column_type, &col.name)?;
                // A NULL AUTO_INCREMENT value (explicit, or via reorder_exprs
                // defaulting an omitted column) gets the next sequence value
                // instead of being subject to the NOT NULL check below.
                // Reserving it here (rather than deferring to the batch
                // insert) matches real MySQL/InnoDB: AUTO_INCREMENT isn't
                // transactional, so a value is never reused even if this
                // statement as a whole later fails.
                if value == Value::Null && col.auto_increment {
                    value = Value::Int(self.storage.next_auto_increment(table)?);
                }
                if value == Value::Null && !col.nullable {
                    return Err(Error::Execution(format!(
                        "Column '{}' cannot be NULL",
                        col.name
                    )));
                }
                values.push(value);
            }

            batch.push((table.to_string(), values));
        }

        let affected = batch.len() as u64;
        self.storage.insert_rows(batch).await?;

        Ok(QueryResult {
            rows_affected: affected,
            ..QueryResult::default()
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_select(
        &self,
        projection: Vec<SelectItem>,
        from: Option<FromClause>,
        selection: Option<Box<Condition>>,
        group_by: &[ColumnRef],
        order_by: &[OrderByItem],
        limit: Option<u64>,
        offset: Option<u64>,
    ) -> Result<QueryResult> {
        match from {
            None => self.execute_select_without_table(projection, limit, offset),
            Some(from_clause) => {
                let (scope, rows) = self.resolve_from(&from_clause, &selection)?;
                if is_aggregate_query(&projection, group_by) {
                    execute_aggregate(&scope, rows, &projection, group_by, order_by, limit, offset)
                } else {
                    execute_projected(&scope, rows, &projection, order_by, limit, offset)
                }
            }
        }
    }

    /// Resolve a `FROM` clause (one table, or several joined) into a column
    /// [`Scope`] and its `WHERE`-filtered rows. A single, unjoined table
    /// keeps the indexed `WHERE pk = literal` fast path (see
    /// `Storage::lookup_by_primary_key`, used inside `scan_and_filter`); a
    /// `JOIN`ed query has no equivalent index once rows are combined, so its
    /// `WHERE` is a plain linear filter applied after the join (see
    /// `apply_where`).
    fn resolve_from(
        &self,
        from: &FromClause,
        selection: &Option<Box<Condition>>,
    ) -> Result<(Scope, Vec<Vec<Value>>)> {
        if from.joins.is_empty() {
            let schema = self.storage.table_schema(&from.table.name)?;
            let qualifier = from.table.qualifier();
            let scope = Scope::single(qualifier, &schema.columns);
            let rows = self.scan_and_filter(&from.table.name, qualifier, &schema, selection)?;
            Ok((scope, rows))
        } else {
            let (scope, rows) = self.resolve_joins(from)?;
            let rows = apply_where(rows, &scope, selection)?;
            Ok((scope, rows))
        }
    }

    /// Evaluate a `FROM` clause's `JOIN`s into a combined [`Scope`] and its
    /// (not yet `WHERE`-filtered) rows, via a sequence of hash joins — each
    /// subsequent table is joined onto the rows accumulated so far. `ON` is
    /// restricted to one column-to-column equality (this engine's `WHERE`/
    /// `ON` have never supported AND/OR-chaining a predicate — see
    /// [`Condition`]); `NULL` never matches on either side, matching SQL's
    /// three-valued logic (the same rule `compare_values` already applies to
    /// `WHERE`). `ON`'s two sides aren't fixed to "old table" / "new table"
    /// by syntax position (`ON a.x = b.y` is exactly as valid as
    /// `ON b.y = a.x`), so both orders are tried.
    fn resolve_joins(&self, from: &FromClause) -> Result<(Scope, Vec<Vec<Value>>)> {
        let base_schema = self.storage.table_schema(&from.table.name)?;
        let mut scope = Scope::single(from.table.qualifier(), &base_schema.columns);
        let mut rows = self.storage.scan(&from.table.name)?;

        for join in &from.joins {
            let join_schema = self.storage.table_schema(&join.table.name)?;
            let join_qualifier = join.table.qualifier();
            let new_table_scope = Scope::single(join_qualifier, &join_schema.columns);

            let (accumulated_idx, new_idx) = match (
                scope.try_resolve(&join.left),
                new_table_scope.try_resolve(&join.right),
            ) {
                (Some(l), Some(r)) => (l, r),
                _ => match (
                    scope.try_resolve(&join.right),
                    new_table_scope.try_resolve(&join.left),
                ) {
                    (Some(l), Some(r)) => (l, r),
                    _ => {
                        return Err(Error::Execution(format!(
                            "JOIN ... ON must compare a column already in scope with a column of '{join_qualifier}'"
                        )))
                    }
                },
            };

            let new_rows = self.storage.scan(&join.table.name)?;
            let right_width = join_schema.columns.len();
            rows = hash_join(
                rows,
                accumulated_idx,
                new_rows,
                new_idx,
                right_width,
                join.join_type,
            );
            scope.push_table(join_qualifier, &join_schema.columns);
        }

        Ok((scope, rows))
    }

    /// `SELECT <expr-list>` with no `FROM` — literals, `NULL`, and system
    /// variables only. Always exactly one row unless `LIMIT`/`OFFSET` drop
    /// it (`ORDER BY` is a no-op here — there is only one row to order).
    fn execute_select_without_table(
        &self,
        projection: Vec<SelectItem>,
        limit: Option<u64>,
        offset: Option<u64>,
    ) -> Result<QueryResult> {
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
                Expr::Decimal(unscaled, scale) => (
                    format_decimal(unscaled, scale),
                    Value::Decimal(unscaled, scale),
                ),
                Expr::String(s) => (s.clone(), Value::Varchar(s)),
                Expr::Null => ("NULL".to_string(), Value::Null),
                Expr::SystemVariable(name) => (format!("@@{name}"), self.system_variable(&name)),
                Expr::Function(name, args) => {
                    (format!("{name}()"), self.evaluate_function(&name, &args))
                }
                // A bare column with no FROM clause is an error, as in MySQL.
                Expr::Column(col_ref) => {
                    let name = match &col_ref.table {
                        Some(t) => format!("{t}.{}", col_ref.column),
                        None => col_ref.column.clone(),
                    };
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
                Value::Decimal(_, scale) => ColumnType::Decimal(*scale),
                Value::Date(_) => ColumnType::Date,
                Value::Varchar(_) | Value::Null => ColumnType::Varchar,
            };
            columns.push(ColumnSchema {
                name: alias.unwrap_or(default_name),
                column_type,
                nullable: true,
                auto_increment: false,
            });
            values.push(value);
        }

        // Exactly one row exists to page through: OFFSET >= 1 or LIMIT 0
        // drops it, anything else keeps it.
        let dropped = offset.is_some_and(|o| o >= 1) || limit == Some(0);
        let rows = if dropped { Vec::new() } else { vec![values] };

        Ok(QueryResult {
            columns,
            rows,
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

    /// Scan `table` and apply an optional `WHERE` filter, returning full
    /// (pre-projection) matching rows. The no-`JOIN` `SELECT` path (see
    /// `resolve_from`). Uses the indexed primary-key lookup for
    /// `col = value` on the primary key; `Storage::scan_filtered`
    /// (PERFORMANCE_DURABILITY_PLAN.md P1) otherwise, which clones only the
    /// rows that actually match instead of the whole table.
    fn scan_and_filter(
        &self,
        table: &str,
        qualifier: &str,
        schema: &TableSchema,
        selection: &Option<Box<Condition>>,
    ) -> Result<Vec<Vec<Value>>> {
        match selection {
            None => self.storage.scan(table),
            Some(cond) => {
                let col_idx =
                    resolve_single_table_column(&schema.columns, qualifier, &cond.column)?;
                let column_type = schema.columns[col_idx].column_type;
                let expected = coerce(&cond.value, column_type, &cond.column.column)?;

                let is_pk_equality = cond.op == CompareOp::Eq
                    && schema.primary_key.as_deref() == Some(schema.columns[col_idx].name.as_str());
                if is_pk_equality {
                    Ok(self
                        .storage
                        .lookup_by_primary_key(table, &expected)?
                        .into_iter()
                        .collect())
                } else {
                    let op = cond.op;
                    self.storage.scan_filtered(table, &mut |row| {
                        compare_values(&row[col_idx], op, &expected)
                    })
                }
            }
        }
    }
}

/// The columns visible while resolving a name in a `SELECT`'s `WHERE`/
/// `GROUP BY`/`ORDER BY`/projection — either one table's own schema, or
/// several tables' schemas concatenated by a `JOIN`, each column tagged with
/// the qualifier (table name, or its `AS` alias) it can be addressed by. A
/// qualified reference (`t.col`) must match both; an unqualified one must be
/// unambiguous among every table in scope — matching MySQL's own "Column
/// 'x' in field list is ambiguous" rule.
struct Scope {
    qualifiers: Vec<String>,
    columns: Vec<ColumnSchema>,
}

impl Scope {
    fn single(qualifier: &str, columns: &[ColumnSchema]) -> Self {
        Scope {
            qualifiers: vec![qualifier.to_string(); columns.len()],
            columns: columns.to_vec(),
        }
    }

    /// Extend the scope with another table's columns — used to add each
    /// `JOIN`ed table onto the accumulated scope, in `FROM`/`JOIN` order
    /// (matching how rows are concatenated by `hash_join`).
    fn push_table(&mut self, qualifier: &str, columns: &[ColumnSchema]) {
        self.qualifiers
            .extend(std::iter::repeat_n(qualifier.to_string(), columns.len()));
        self.columns.extend_from_slice(columns);
    }

    /// Resolve without producing an error message: `None` for both "not
    /// found" and "ambiguous" — used where only a yes/no answer is needed
    /// (see `Executor::resolve_joins`'s ON-side detection). [`resolve`]
    /// builds on this to also report *why* a reference failed.
    ///
    /// [`resolve`]: Scope::resolve
    fn try_resolve(&self, col_ref: &ColumnRef) -> Option<usize> {
        match &col_ref.table {
            Some(q) => self
                .qualifiers
                .iter()
                .zip(self.columns.iter())
                .position(|(cq, c)| cq == q && c.name == col_ref.column),
            None => {
                let mut matches = self
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.name == col_ref.column);
                let first = matches.next()?;
                if matches.next().is_some() {
                    None
                } else {
                    Some(first.0)
                }
            }
        }
    }

    fn resolve(&self, col_ref: &ColumnRef) -> Result<usize> {
        self.try_resolve(col_ref)
            .ok_or_else(|| match &col_ref.table {
                Some(q) => Error::Execution(format!("Unknown column '{q}.{}'", col_ref.column)),
                None => {
                    let ambiguous = self
                        .columns
                        .iter()
                        .filter(|c| c.name == col_ref.column)
                        .count()
                        > 1;
                    if ambiguous {
                        Error::Execution(format!(
                            "Column '{}' in field list is ambiguous",
                            col_ref.column
                        ))
                    } else {
                        Error::Execution(format!("Unknown column '{}'", col_ref.column))
                    }
                }
            })
    }
}

/// Resolve a (possibly qualified) column reference against a single table's
/// own schema — the no-`JOIN` `SELECT` path. A qualifier, if present, must
/// equal the table's own name or its `FROM ... AS` alias (`qualifier`); an
/// unqualified reference always just names one of the table's columns, same
/// as before `JOIN` existed.
fn resolve_single_table_column(
    columns: &[ColumnSchema],
    qualifier: &str,
    col_ref: &ColumnRef,
) -> Result<usize> {
    if let Some(q) = &col_ref.table {
        if q != qualifier {
            return Err(Error::Execution(format!(
                "Unknown column '{q}.{}'",
                col_ref.column
            )));
        }
    }
    column_index(columns, &col_ref.column)
}

/// Join `right_rows` onto `left_rows` by equality between
/// `left_rows[..][left_idx]` and `right_rows[..][right_idx]`, implemented as
/// a hash join: index the newly-joined table's rows by their `ON`-column
/// value, then probe once per accumulated row. `NULL` is never a join key on
/// either side (`NULL = NULL` is never true in SQL), matching
/// `compare_values`'s existing `WHERE`-clause rule. A `LEFT` join keeps
/// every accumulated row even with no match, padding the new table's
/// columns with `NULL`; an `INNER` join drops it.
fn hash_join(
    left_rows: Vec<Vec<Value>>,
    left_idx: usize,
    right_rows: Vec<Vec<Value>>,
    right_idx: usize,
    right_width: usize,
    join_type: JoinType,
) -> Vec<Vec<Value>> {
    let mut index: HashMap<Value, Vec<usize>> = HashMap::new();
    for (i, row) in right_rows.iter().enumerate() {
        if row[right_idx] == Value::Null {
            continue;
        }
        index.entry(row[right_idx].clone()).or_default().push(i);
    }

    let mut output = Vec::with_capacity(left_rows.len());
    for mut left_row in left_rows {
        let key = left_row[left_idx].clone();
        let match_indices = if key == Value::Null {
            None
        } else {
            index.get(&key)
        };
        match match_indices {
            Some(indices) => {
                for &i in indices {
                    let mut combined = left_row.clone();
                    combined.extend(right_rows[i].iter().cloned());
                    output.push(combined);
                }
            }
            None if join_type == JoinType::Left => {
                left_row.extend(std::iter::repeat_n(Value::Null, right_width));
                output.push(left_row);
            }
            None => {}
        }
    }
    output
}

/// Apply an already-resolved `WHERE` filter to already-materialized rows —
/// the `JOIN` path's counterpart to `Executor::scan_and_filter`'s non-primary-key
/// branch (a `JOIN`ed result has no index to speed up an equality lookup).
fn apply_where(
    rows: Vec<Vec<Value>>,
    scope: &Scope,
    selection: &Option<Box<Condition>>,
) -> Result<Vec<Vec<Value>>> {
    let Some(cond) = selection else {
        return Ok(rows);
    };
    let idx = scope.resolve(&cond.column)?;
    let column_type = scope.columns[idx].column_type;
    let expected = coerce(&cond.value, column_type, &cond.column.column)?;
    Ok(rows
        .into_iter()
        .filter(|row| compare_values(&row[idx], cond.op, &expected))
        .collect())
}

/// The non-aggregate tail of a `SELECT ... FROM ...`: resolve the
/// projection against `scope`, sort (if `ORDER BY`), then paginate. Shared
/// by the single-table and `JOIN`ed paths — by this point both have already
/// produced a [`Scope`] and its `WHERE`-filtered rows, and there's nothing
/// left that differs between them.
fn execute_projected(
    scope: &Scope,
    mut rows: Vec<Vec<Value>>,
    projection: &[SelectItem],
    order_by: &[OrderByItem],
    limit: Option<u64>,
    offset: Option<u64>,
) -> Result<QueryResult> {
    let selected_indices = resolve_projection(scope, projection)?;

    // Sort before projecting: ORDER BY may name a column that isn't in the
    // SELECT list, so this needs the full (pre-projection) row.
    if !order_by.is_empty() {
        let sort_keys = order_by
            .iter()
            .map(|item| Ok((scope.resolve(&item.column)?, item.descending)))
            .collect::<Result<Vec<(usize, bool)>>>()?;
        rows.sort_by(|a, b| {
            for &(idx, descending) in &sort_keys {
                let ordering = value_ordering(&a[idx], &b[idx]);
                let ordering = if descending {
                    ordering.reverse()
                } else {
                    ordering
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            Ordering::Equal
        });
    }

    // OFFSET then LIMIT, applied after ordering/filtering — matching real
    // MySQL's evaluation order.
    let paged_rows: Vec<Vec<Value>> = rows
        .into_iter()
        .skip(offset.unwrap_or(0) as usize)
        .take(limit.unwrap_or(u64::MAX) as usize)
        .collect();

    let columns = selected_indices
        .iter()
        .map(|&i| scope.columns[i].clone())
        .collect();
    let rows = paged_rows
        .into_iter()
        .map(|row| selected_indices.iter().map(|&i| row[i].clone()).collect())
        .collect();

    Ok(QueryResult {
        columns,
        rows,
        rows_affected: 0,
    })
}

/// `SELECT` with `GROUP BY` and/or an aggregate function
/// (`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`) in the projection: the aggregate tail of
/// a `SELECT ... FROM ...`, mirroring [`execute_projected`] — shared by the
/// single-table and `JOIN`ed paths once both have produced a [`Scope`] and
/// its `WHERE`-filtered rows. Groups the rows by `group_by`'s columns'
/// values — or, if `group_by` is empty, treats the whole filtered set as one
/// group, so e.g. `SELECT COUNT(*) FROM t` always returns exactly one row,
/// even for an empty table (zero *groups* only happens with an explicit,
/// non-empty `GROUP BY` and zero matching rows). Every bare column in the
/// projection must be one of `group_by`'s columns — standard SQL; MySQL's
/// own `ONLY_FULL_GROUP_BY` default enforces the same rule.
#[allow(clippy::too_many_arguments)]
fn execute_aggregate(
    scope: &Scope,
    matching_rows: Vec<Vec<Value>>,
    projection: &[SelectItem],
    group_by: &[ColumnRef],
    order_by: &[OrderByItem],
    limit: Option<u64>,
    offset: Option<u64>,
) -> Result<QueryResult> {
    let group_by_indices = group_by
        .iter()
        .map(|col_ref| scope.resolve(col_ref))
        .collect::<Result<Vec<usize>>>()?;

    if projection
        .iter()
        .any(|item| matches!(item, SelectItem::Wildcard))
    {
        return Err(Error::Unsupported("'*' in a GROUP BY / aggregate query"));
    }

    // Resolve each projection item once (not once per group), and build the
    // output column schema right away — independent of any group's actual
    // data, so a zero-group result (e.g. GROUP BY on an empty table) still
    // reports the right columns, just no rows.
    let mut resolved = Vec::with_capacity(projection.len());
    let mut output_columns = Vec::with_capacity(projection.len());
    for item in projection {
        let SelectItem::Expr(expr, alias) = item else {
            unreachable!("wildcard already rejected above");
        };
        let (item_plan, default_name, column_type) = match expr {
            Expr::Column(col_ref) => {
                let idx = scope.resolve(col_ref)?;
                let pos = group_by_indices.iter().position(|&gi| gi == idx).ok_or_else(|| {
                    Error::Execution(format!(
                        "Column '{}' must appear in GROUP BY or be used inside an aggregate function",
                        col_ref.column
                    ))
                })?;
                (
                    ResolvedAggregateItem::GroupColumn(pos),
                    scope.columns[idx].name.clone(),
                    scope.columns[idx].column_type,
                )
            }
            Expr::Function(name, args) if is_aggregate_function_name(name) => {
                let arg_col = resolve_aggregate_arg(args, scope)?;
                let column_type = aggregate_result_type(name, arg_col.map(|(_, t)| t))?;
                let default_name = match arg_col {
                    None => format!("{name}(*)"),
                    Some((idx, _)) => format!("{name}({})", scope.columns[idx].name),
                };
                (
                    ResolvedAggregateItem::Aggregate(name.clone(), arg_col),
                    default_name,
                    column_type,
                )
            }
            _ => {
                return Err(Error::Execution(
                    "only columns and aggregate functions are allowed in a GROUP BY / aggregate SELECT list"
                        .to_string(),
                ))
            }
        };
        resolved.push(item_plan);
        output_columns.push(ColumnSchema {
            name: alias.clone().unwrap_or(default_name),
            column_type,
            nullable: true,
            auto_increment: false,
        });
    }

    let mut groups: HashMap<Vec<Value>, Vec<Vec<Value>>> = HashMap::new();
    if group_by_indices.is_empty() {
        groups.insert(Vec::new(), matching_rows);
    } else {
        for row in matching_rows {
            let key: Vec<Value> = group_by_indices.iter().map(|&i| row[i].clone()).collect();
            groups.entry(key).or_default().push(row);
        }
    }

    // Deterministic group order (by key) so results are stable even before
    // any ORDER BY is applied.
    let mut keys: Vec<Vec<Value>> = groups.keys().cloned().collect();
    keys.sort_by(|a, b| {
        a.iter()
            .zip(b.iter())
            .map(|(av, bv)| value_ordering(av, bv))
            .find(|ord| *ord != Ordering::Equal)
            .unwrap_or(Ordering::Equal)
    });

    let mut output_rows = Vec::with_capacity(keys.len());
    for key in &keys {
        let group_rows = &groups[key];
        let mut row = Vec::with_capacity(resolved.len());
        for item_plan in &resolved {
            let value = match item_plan {
                ResolvedAggregateItem::GroupColumn(pos) => key[*pos].clone(),
                ResolvedAggregateItem::Aggregate(name, arg_col) => {
                    evaluate_aggregate(name, *arg_col, group_rows)?
                }
            };
            row.push(value);
        }
        output_rows.push(row);
    }

    // ORDER BY resolves against the OUTPUT columns (by name/alias), not the
    // source table(s) — an aggregate query's ORDER BY commonly names an
    // aggregate's own alias (`ORDER BY total DESC`), which may not even
    // exist as a column anywhere. The output has no qualifiers of its own,
    // so only the bare column name is meaningful here.
    if !order_by.is_empty() {
        let sort_keys = order_by
            .iter()
            .map(|item| {
                let idx = output_columns
                    .iter()
                    .position(|c| c.name == item.column.column)
                    .ok_or_else(|| {
                        Error::Execution(format!("Unknown column '{}'", item.column.column))
                    })?;
                Ok((idx, item.descending))
            })
            .collect::<Result<Vec<(usize, bool)>>>()?;
        output_rows.sort_by(|a, b| {
            sort_keys
                .iter()
                .map(|&(idx, descending)| {
                    let ord = value_ordering(&a[idx], &b[idx]);
                    if descending {
                        ord.reverse()
                    } else {
                        ord
                    }
                })
                .find(|ord| *ord != Ordering::Equal)
                .unwrap_or(Ordering::Equal)
        });
    }

    let paged_rows: Vec<Vec<Value>> = output_rows
        .into_iter()
        .skip(offset.unwrap_or(0) as usize)
        .take(limit.unwrap_or(u64::MAX) as usize)
        .collect();

    Ok(QueryResult {
        columns: output_columns,
        rows: paged_rows,
        rows_affected: 0,
    })
}

/// A projection item's meaning once resolved against `GROUP BY` and the
/// source schema — computed once per query, then reused for every group.
enum ResolvedAggregateItem {
    /// A `GROUP BY` column: its position within `group_by` (and the group
    /// key, which is built in that same order).
    GroupColumn(usize),
    /// An aggregate function call: its name and, for anything but
    /// `COUNT(*)`, the source column's index and type.
    Aggregate(String, Option<(usize, ColumnType)>),
}

fn is_aggregate_function_name(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
    )
}

/// Whether a `SELECT` needs the aggregate execution path: a non-empty
/// `GROUP BY`, or any aggregate function call in the projection.
fn is_aggregate_query(projection: &[SelectItem], group_by: &[ColumnRef]) -> bool {
    !group_by.is_empty()
        || projection.iter().any(|item| {
            matches!(item, SelectItem::Expr(Expr::Function(name, _), _) if is_aggregate_function_name(name))
        })
}

/// Resolve an aggregate function's argument to a source column, if any.
/// `[]` (i.e. `COUNT(*)`) resolves to `None`; a single bare-column argument
/// resolves to that column's index and type; anything else (a literal, a
/// nested call, more than one argument) is `Unsupported` — this engine has
/// no arithmetic operators, so an aggregate can only ever operate directly
/// on a stored column.
fn resolve_aggregate_arg(args: &[Expr], scope: &Scope) -> Result<Option<(usize, ColumnType)>> {
    match args {
        [] => Ok(None),
        [Expr::Column(col_ref)] => {
            let idx = scope.resolve(col_ref)?;
            Ok(Some((idx, scope.columns[idx].column_type)))
        }
        _ => Err(Error::Unsupported(
            "aggregate functions only support a single column argument (or none, for COUNT(*))",
        )),
    }
}

/// The result type of an aggregate function, given its argument's column
/// type (`None` for `COUNT(*)`) — used to build the output column schema
/// independent of any actual group's data (see `execute_aggregate`).
fn aggregate_result_type(name: &str, arg_type: Option<ColumnType>) -> Result<ColumnType> {
    match name.to_ascii_uppercase().as_str() {
        "COUNT" => Ok(ColumnType::Int),
        "SUM" => match arg_type {
            Some(ColumnType::Int) => Ok(ColumnType::Int),
            Some(ColumnType::Decimal(scale)) => Ok(ColumnType::Decimal(scale)),
            _ => Err(Error::Execution(
                "SUM() requires a numeric column".to_string(),
            )),
        },
        "AVG" => match arg_type {
            Some(ColumnType::Int) => Ok(ColumnType::Decimal(4)),
            Some(ColumnType::Decimal(scale)) => {
                Ok(ColumnType::Decimal(scale.saturating_add(4).min(30)))
            }
            _ => Err(Error::Execution(
                "AVG() requires a numeric column".to_string(),
            )),
        },
        "MIN" | "MAX" => {
            arg_type.ok_or_else(|| Error::Execution(format!("{name}() requires a column argument")))
        }
        other => unreachable!("caller already checked is_aggregate_function_name: {other}"),
    }
}

/// Evaluate one aggregate function call over `group_rows`. `arg_col` is the
/// source column's index and type (`None` for `COUNT(*)`) — see
/// `resolve_aggregate_arg`. `SUM`/`AVG`/`MIN`/`MAX` skip `NULL` values;
/// `SUM`/`AVG` return `NULL` (not `0`) if every value was `NULL`, matching
/// standard SQL. Checked arithmetic throughout: an absurd magnitude is a
/// clean `Error::Execution`, never an overflow panic.
fn evaluate_aggregate(
    name: &str,
    arg_col: Option<(usize, ColumnType)>,
    group_rows: &[Vec<Value>],
) -> Result<Value> {
    match name.to_ascii_uppercase().as_str() {
        "COUNT" => {
            let count = match arg_col {
                None => group_rows.len(),
                Some((idx, _)) => group_rows.iter().filter(|r| r[idx] != Value::Null).count(),
            };
            Ok(Value::Int(count as i64))
        }
        "SUM" => {
            let (idx, col_type) = arg_col
                .ok_or_else(|| Error::Execution("SUM() requires a column argument".to_string()))?;
            let overflow = || Error::Execution("SUM value out of range".to_string());
            match col_type {
                ColumnType::Int => {
                    let mut sum: i64 = 0;
                    let mut any = false;
                    for row in group_rows {
                        if let Value::Int(n) = row[idx] {
                            any = true;
                            sum = sum.checked_add(n).ok_or_else(overflow)?;
                        }
                    }
                    Ok(if any { Value::Int(sum) } else { Value::Null })
                }
                ColumnType::Decimal(scale) => {
                    let mut sum: i64 = 0;
                    let mut any = false;
                    for row in group_rows {
                        if let Value::Decimal(unscaled, _) = row[idx] {
                            any = true;
                            sum = sum.checked_add(unscaled).ok_or_else(overflow)?;
                        }
                    }
                    Ok(if any {
                        Value::Decimal(sum, scale)
                    } else {
                        Value::Null
                    })
                }
                _ => Err(Error::Execution(
                    "SUM() requires a numeric column".to_string(),
                )),
            }
        }
        "AVG" => {
            let (idx, col_type) = arg_col
                .ok_or_else(|| Error::Execution("AVG() requires a column argument".to_string()))?;
            let source_scale = match col_type {
                ColumnType::Int => 0u8,
                ColumnType::Decimal(s) => s,
                _ => {
                    return Err(Error::Execution(
                        "AVG() requires a numeric column".to_string(),
                    ))
                }
            };
            let avg_scale = source_scale.saturating_add(4).min(30);
            let overflow = || Error::Execution("AVG value out of range".to_string());

            let mut sum: i64 = 0;
            let mut count: i64 = 0;
            for row in group_rows {
                let n = match &row[idx] {
                    Value::Int(n) => Some(*n),
                    Value::Decimal(u, _) => Some(*u),
                    _ => None,
                };
                if let Some(n) = n {
                    sum = sum.checked_add(n).ok_or_else(overflow)?;
                    count += 1;
                }
            }
            if count == 0 {
                return Ok(Value::Null);
            }
            let factor = 10i64
                .checked_pow(u32::from(avg_scale - source_scale))
                .ok_or_else(overflow)?;
            let scaled_sum = sum.checked_mul(factor).ok_or_else(overflow)?;
            let magnitude = scaled_sum.unsigned_abs();
            let count_u = count as u64;
            let rounded = (magnitude + count_u / 2) / count_u;
            let rounded = i64::try_from(rounded).map_err(|_| overflow())?;
            let avg_unscaled = if scaled_sum < 0 { -rounded } else { rounded };
            Ok(Value::Decimal(avg_unscaled, avg_scale))
        }
        "MIN" | "MAX" => {
            let (idx, _) = arg_col
                .ok_or_else(|| Error::Execution(format!("{name}() requires a column argument")))?;
            let want_min = name.eq_ignore_ascii_case("MIN");
            let mut best: Option<Value> = None;
            for row in group_rows {
                let v = &row[idx];
                if matches!(v, Value::Null) {
                    continue;
                }
                best = Some(match best {
                    None => v.clone(),
                    Some(current) => {
                        let ord = value_ordering(v, &current);
                        let keep_new = (want_min && ord == Ordering::Less)
                            || (!want_min && ord == Ordering::Greater);
                        if keep_new {
                            v.clone()
                        } else {
                            current
                        }
                    }
                });
            }
            Ok(best.unwrap_or(Value::Null))
        }
        other => unreachable!("caller already checked is_aggregate_function_name: {other}"),
    }
}

fn resolve_projection(scope: &Scope, projection: &[SelectItem]) -> Result<Vec<usize>> {
    if let [SelectItem::Wildcard] = projection {
        return Ok((0..scope.columns.len()).collect());
    }

    let mut indices = Vec::with_capacity(projection.len());
    for item in projection {
        match item {
            SelectItem::Wildcard => {
                return Err(Error::Execution(
                    "'*' cannot be combined with other selected columns".to_string(),
                ));
            }
            SelectItem::Expr(Expr::Column(col_ref), _) => indices.push(scope.resolve(col_ref)?),
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
        .map(|col| match named.remove(col.name.as_str()) {
            Some(expr) => Ok(expr),
            // An omitted AUTO_INCREMENT column defaults to NULL, which
            // execute_insert then substitutes with the next sequence value —
            // matching real MySQL, where naming an explicit column list
            // without the auto-increment column is the normal way to insert.
            None if col.auto_increment => Ok(Expr::Null),
            None => Err(Error::Execution(format!(
                "Column '{}' has no default value and was not given a value",
                col.name
            ))),
        })
        .collect()
}

/// Coerce a parsed literal into a typed storage [`Value`] for `column`,
/// following MySQL's permissive-but-checked conversions: a numeric string
/// into an INT column is parsed, an integer into a VARCHAR column is
/// stringified, and `NULL` is always allowed at this stage (primary-key
/// not-null is enforced by the caller). Every `Decimal` value that reaches
/// storage is rescaled to `column`'s own declared scale here, so any two
/// values ever compared/hashed for one column already share a scale — see
/// `storage::Value::Decimal`.
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
        (Expr::Integer(n), ColumnType::Decimal(scale)) => {
            rescale_decimal(*n, 0, scale, column_name).map(|u| Value::Decimal(u, scale))
        }
        (Expr::Decimal(unscaled, lit_scale), ColumnType::Decimal(scale)) => {
            rescale_decimal(*unscaled, *lit_scale, scale, column_name)
                .map(|u| Value::Decimal(u, scale))
        }
        (Expr::Decimal(unscaled, lit_scale), ColumnType::Int) => {
            rescale_decimal(*unscaled, *lit_scale, 0, column_name).map(Value::Int)
        }
        (Expr::Decimal(unscaled, lit_scale), ColumnType::Varchar) => {
            Ok(Value::Varchar(format_decimal(*unscaled, *lit_scale)))
        }
        (Expr::String(s), ColumnType::Decimal(scale)) => {
            let (unscaled, lit_scale) = parse_decimal_literal(s, column_name)?;
            rescale_decimal(unscaled, lit_scale, scale, column_name)
                .map(|u| Value::Decimal(u, scale))
        }
        (Expr::String(s), ColumnType::Date) => parse_date_literal(s, column_name).map(Value::Date),
        (Expr::Integer(_) | Expr::Decimal(..), ColumnType::Date) => Err(Error::Execution(format!(
            "Incorrect date value for column '{column_name}': expected a 'YYYY-MM-DD' string"
        ))),
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

/// Convert a fixed-point value from `from_scale` to `to_scale` (widening
/// multiplies; narrowing rounds half-away-from-zero), with checked
/// arithmetic throughout so an absurd scale/magnitude combination is a clean
/// `Error::Execution`, never an overflow panic.
fn rescale_decimal(unscaled: i64, from_scale: u8, to_scale: u8, column_name: &str) -> Result<i64> {
    let out_of_range = || {
        Error::Execution(format!(
            "decimal value out of range for column '{column_name}'"
        ))
    };
    if to_scale >= from_scale {
        let factor = 10i64
            .checked_pow(u32::from(to_scale - from_scale))
            .ok_or_else(out_of_range)?;
        unscaled.checked_mul(factor).ok_or_else(out_of_range)
    } else {
        let divisor = 10u64
            .checked_pow(u32::from(from_scale - to_scale))
            .ok_or_else(out_of_range)?;
        let magnitude = unscaled.unsigned_abs();
        let rounded = (magnitude + divisor / 2) / divisor;
        let rounded = i64::try_from(rounded).map_err(|_| out_of_range())?;
        Ok(if unscaled < 0 { -rounded } else { rounded })
    }
}

/// Parse a numeric string like `"123.45"`, `"-5"`, or `".5"` into
/// `(unscaled, scale)` at the scale as written (not yet rescaled to any
/// column). Used when a decimal value arrives as text (a quoted SQL string
/// literal, or a prepared-statement string parameter).
fn parse_decimal_literal(s: &str, column_name: &str) -> Result<(i64, u8)> {
    let invalid = || {
        Error::Execution(format!(
            "Incorrect decimal value: '{s}' for column '{column_name}'"
        ))
    };
    let (negative, rest) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (int_part, frac_part) = match rest.split_once('.') {
        Some((i, f)) => (i, f),
        None => (rest, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(invalid());
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(invalid());
    }
    if frac_part.len() > u8::MAX as usize {
        return Err(invalid());
    }
    let magnitude: i64 = format!("{int_part}{frac_part}")
        .parse()
        .map_err(|_| invalid())?;
    Ok((
        if negative { -magnitude } else { magnitude },
        frac_part.len() as u8,
    ))
}

/// Validate a `'YYYY-MM-DD'` date literal: exactly that shape, month `01`-`12`,
/// day `01`-`31`. No calendar-correctness check beyond that (e.g. `2024-02-30`
/// is accepted) — this server does no date arithmetic that would need it
/// (see ROADMAP.md Phase 11's cut list), so it isn't worth the complexity.
fn parse_date_literal(s: &str, column_name: &str) -> Result<String> {
    let invalid = || {
        Error::Execution(format!(
            "Incorrect date value: '{s}' for column '{column_name}'"
        ))
    };
    let bytes = s.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(invalid());
    }
    let digits = |range: std::ops::Range<usize>| -> Result<u32> {
        s.get(range)
            .filter(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|part| part.parse::<u32>().ok())
            .ok_or_else(invalid)
    };
    let _year = digits(0..4)?; // any 4-digit year is accepted; no range limit
    let month = digits(5..7)?;
    let day = digits(8..10)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(invalid());
    }
    Ok(s.to_string())
}

/// Order two values for `ORDER BY` sorting. Unlike `compare_values` (a
/// WHERE-clause filter), this needs a definite answer even when one side is
/// `NULL` — MySQL sorts `NULL` first, as the least value, in ascending order.
fn value_ordering(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        (Value::Varchar(a), Value::Varchar(b)) => a.cmp(b),
        // `Date` stores canonical zero-padded "YYYY-MM-DD" text, which the
        // fallback's string comparison below already orders chronologically
        // — no dedicated arm needed there. `Decimal` is the opposite: its
        // *text* form ("10.20" vs "9.50") does NOT sort the way the numbers
        // do, so it needs a real numeric comparison, normalizing to a common
        // scale first (within one column `coerce` already guarantees a
        // shared scale, but this stays correct even if that ever isn't true).
        (Value::Decimal(a_unscaled, a_scale), Value::Decimal(b_unscaled, b_scale)) => {
            let (a_cmp, b_cmp) = match a_scale.cmp(b_scale) {
                Ordering::Equal => (*a_unscaled, *b_unscaled),
                Ordering::Less => (scale_up(*a_unscaled, b_scale - a_scale), *b_unscaled),
                Ordering::Greater => (*a_unscaled, scale_up(*b_unscaled, a_scale - b_scale)),
            };
            a_cmp.cmp(&b_cmp)
        }
        // Mixed types (incl. Date/Varchar, or Decimal against anything else):
        // compare by display text (best-effort; real MySQL has more nuanced
        // coercion rules than this subset needs).
        (a, b) => a
            .to_display_string()
            .unwrap_or_default()
            .cmp(&b.to_display_string().unwrap_or_default()),
    }
}

/// Multiply `unscaled` by `10^extra_scale`, saturating instead of
/// overflowing — used only to bring two *differently*-scaled decimals to a
/// common scale for comparison (`checked_pow`/`saturating_mul` so an
/// absurd scale difference saturates rather than panicking; `value_ordering`
/// returns a plain `Ordering`, so there's no `Result` to propagate an error
/// through here — a client-reachable path must never panic regardless).
fn scale_up(unscaled: i64, extra_scale: u8) -> i64 {
    match 10i64.checked_pow(u32::from(extra_scale)) {
        Some(factor) => unscaled.saturating_mul(factor),
        None if unscaled < 0 => i64::MIN,
        None => i64::MAX,
    }
}

/// Compare two typed values. SQL three-valued logic: any comparison
/// involving `NULL` is never true (not even `NULL = NULL`) — distinct from
/// `value_ordering`'s `ORDER BY` sorting, where NULL needs a definite
/// position rather than "no match".
fn compare_values(actual: &Value, op: CompareOp, expected: &Value) -> bool {
    if matches!(actual, Value::Null) || matches!(expected, Value::Null) {
        return false;
    }
    let ordering = value_ordering(actual, expected);
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

    /// Drive a future to completion on a throwaway current-thread runtime.
    /// `Executor::execute` became `async` in PERFORMANCE_DURABILITY_PLAN.md
    /// PD-2 (it now genuinely awaits the log-writer thread's ack for
    /// mutating statements), but the ~150 tests below call it through
    /// `run()` alone — wrapping just this one chokepoint keeps every test
    /// function itself synchronous rather than converting each one to
    /// `#[tokio::test] async fn` and adding `.await` at every call site.
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build a test runtime")
            .block_on(fut)
    }

    fn run(storage: &InMemoryStorage, sql: &str) -> Result<QueryResult> {
        let vars = test_vars();
        block_on(Executor::new(storage, &vars).execute(parse(sql)?))
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
    fn create_drop_and_show_databases_round_trip() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE DATABASE mydb").expect("CREATE DATABASE");
        // Duplicate without IF NOT EXISTS errors.
        assert!(matches!(
            run(&storage, "CREATE DATABASE mydb"),
            Err(Error::Execution(_))
        ));
        // IF NOT EXISTS makes the duplicate a no-op.
        run(&storage, "CREATE DATABASE IF NOT EXISTS mydb").expect("idempotent create");

        let shown = run(&storage, "SHOW DATABASES").expect("SHOW DATABASES");
        assert_eq!(shown.rows, vec![vec![vc("mydb")]]);

        run(&storage, "DROP DATABASE mydb").expect("DROP DATABASE");
        let shown = run(&storage, "SHOW DATABASES").expect("SHOW DATABASES");
        assert!(shown.rows.is_empty());

        // Dropping again without IF EXISTS errors; with it, a silent no-op.
        assert!(matches!(
            run(&storage, "DROP DATABASE mydb"),
            Err(Error::Execution(_))
        ));
        run(&storage, "DROP DATABASE IF EXISTS mydb").expect("idempotent drop");
    }

    #[test]
    fn show_character_set_and_collation_are_non_empty() {
        // The specific regression this guards: DBeaver's "create database"
        // dialog reads these to populate a charset/collation picker, and an
        // empty result (the old ShowStatement::Other fallback) made its own
        // client-side code null-pointer before ever sending more SQL.
        let storage = InMemoryStorage::new();

        let charsets = run(&storage, "SHOW CHARACTER SET").expect("SHOW CHARACTER SET");
        assert!(!charsets.rows.is_empty());
        assert_eq!(
            charsets.column_names(),
            vec!["Charset", "Description", "Default collation", "Maxlen"]
        );
        assert_eq!(charsets.rows[0][0], vc("utf8mb4"));

        let collations = run(&storage, "SHOW COLLATION").expect("SHOW COLLATION");
        assert!(!collations.rows.is_empty());
        assert_eq!(collations.rows[0][0], vc("utf8mb4_general_ci"));
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
    fn insert_null_into_explicit_not_null_column_is_rejected() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT NOT NULL)").expect("create");
        assert!(matches!(
            run(&storage, "INSERT INTO t VALUES (NULL)"),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn insert_null_into_a_nullable_column_is_allowed() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT NULL)").expect("create");
        run(&storage, "INSERT INTO t VALUES (NULL)").expect("insert");
        let result = run(&storage, "SELECT * FROM t").expect("select");
        assert_eq!(result.rows, vec![vec![Value::Null]]);
    }

    #[test]
    fn auto_increment_assigns_sequential_ids_when_omitted() {
        let storage = InMemoryStorage::new();
        run(
            &storage,
            "CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR)",
        )
        .expect("create");
        run(&storage, "INSERT INTO t (name) VALUES ('alice')").expect("insert 1");
        run(&storage, "INSERT INTO t (name) VALUES ('bob')").expect("insert 2");
        let result = run(&storage, "SELECT id, name FROM t").expect("select");
        assert_eq!(
            result.rows,
            vec![vec![int(1), vc("alice")], vec![int(2), vc("bob")],]
        );
    }

    #[test]
    fn auto_increment_assigns_when_value_is_explicitly_null() {
        let storage = InMemoryStorage::new();
        run(
            &storage,
            "CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR)",
        )
        .expect("create");
        run(&storage, "INSERT INTO t VALUES (NULL, 'alice')").expect("insert");
        let result = run(&storage, "SELECT id FROM t").expect("select");
        assert_eq!(result.rows, vec![vec![int(1)]]);
    }

    #[test]
    fn auto_increment_continues_after_an_explicit_higher_value() {
        let storage = InMemoryStorage::new();
        run(
            &storage,
            "CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY)",
        )
        .expect("create");
        run(&storage, "INSERT INTO t VALUES (100)").expect("explicit value");
        run(&storage, "INSERT INTO t VALUES (NULL)").expect("auto-assigned");
        let result = run(&storage, "SELECT id FROM t").expect("select");
        let mut ids: Vec<i64> = result
            .rows
            .into_iter()
            .map(|r| match r[0] {
                Value::Int(n) => n,
                ref other => panic!("expected an int, got {other:?}"),
            })
            .collect();
        ids.sort();
        assert_eq!(ids, vec![100, 101]);
    }

    #[test]
    fn auto_increment_on_a_non_primary_key_column_is_unsupported() {
        let storage = InMemoryStorage::new();
        assert!(matches!(
            run(
                &storage,
                "CREATE TABLE t (id INT PRIMARY KEY, seq INT AUTO_INCREMENT)"
            ),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn more_than_one_auto_increment_column_is_unsupported() {
        let storage = InMemoryStorage::new();
        assert!(matches!(
            run(
                &storage,
                "CREATE TABLE t (a INT AUTO_INCREMENT PRIMARY KEY, b INT AUTO_INCREMENT)"
            ),
            Err(Error::Unsupported(_))
        ));
    }

    // ---- BOOLEAN, DECIMAL, DATE (Phase 11) ----

    #[test]
    fn boolean_column_stores_true_false_as_one_and_zero() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (flag BOOLEAN)").expect("create");
        run(&storage, "INSERT INTO t VALUES (TRUE), (FALSE)").expect("insert");
        let result = run(&storage, "SELECT flag FROM t").expect("select");
        assert_eq!(result.rows, vec![vec![int(1)], vec![int(0)]]);
        // BOOLEAN is a pure INT alias, exactly like real MySQL — not its own
        // physical type.
        assert_eq!(result.columns[0].column_type, ColumnType::Int);
    }

    #[test]
    fn decimal_literal_inserted_and_selected_round_trips_exactly() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (price DECIMAL(10,2))").expect("create");
        run(&storage, "INSERT INTO t VALUES (19.99)").expect("insert");
        let result = run(&storage, "SELECT price FROM t").expect("select");
        assert_eq!(result.rows, vec![vec![Value::Decimal(1999, 2)]]);
        assert_eq!(
            result.rows[0][0].to_display_string(),
            Some("19.99".to_string())
        );
    }

    #[test]
    fn decimal_values_are_rescaled_to_the_columns_declared_scale() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (price DECIMAL(10,2))").expect("create");
        // An integer, a decimal with fewer fractional digits, and a decimal
        // with MORE (rounded down to the column's scale) all normalize to
        // the same (unscaled, scale) representation.
        run(&storage, "INSERT INTO t VALUES (5)").expect("insert int");
        run(&storage, "INSERT INTO t VALUES (5.5)").expect("insert coarser scale");
        run(&storage, "INSERT INTO t VALUES (5.999)").expect("insert finer scale, rounds");
        let result = run(&storage, "SELECT price FROM t").expect("select");
        assert_eq!(
            result.rows,
            vec![
                vec![Value::Decimal(500, 2)],
                vec![Value::Decimal(550, 2)],
                vec![Value::Decimal(600, 2)], // 5.999 rounds to 6.00 at scale 2
            ]
        );
    }

    #[test]
    fn decimal_comparison_and_ordering_are_numeric_not_lexical() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (price DECIMAL(10,2))").expect("create");
        for v in ["9.50", "10.20", "1.00"] {
            run(&storage, &format!("INSERT INTO t VALUES ({v})")).expect("insert");
        }
        // Lexically "10.20" < "1.00" < "9.50"; numerically 1.00 < 9.50 < 10.20.
        let result = run(&storage, "SELECT price FROM t ORDER BY price").expect("select");
        assert_eq!(
            result.rows,
            vec![
                vec![Value::Decimal(100, 2)],
                vec![Value::Decimal(950, 2)],
                vec![Value::Decimal(1020, 2)],
            ]
        );
        let matched = run(&storage, "SELECT price FROM t WHERE price > 5.00").expect("select");
        assert_eq!(matched.rows.len(), 2); // 9.50 and 10.20, not 1.00
    }

    #[test]
    fn decimal_into_int_column_rounds() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT)").expect("create");
        run(&storage, "INSERT INTO t VALUES (2.6)").expect("insert");
        let result = run(&storage, "SELECT a FROM t").expect("select");
        assert_eq!(result.rows, vec![vec![int(3)]]);
    }

    #[test]
    fn decimal_into_varchar_column_stringifies() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a VARCHAR)").expect("create");
        run(&storage, "INSERT INTO t VALUES (3.50)").expect("insert");
        let result = run(&storage, "SELECT a FROM t").expect("select");
        assert_eq!(result.rows, vec![vec![vc("3.50")]]);
    }

    #[test]
    fn date_literal_inserted_and_selected() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (d DATE)").expect("create");
        run(&storage, "INSERT INTO t VALUES ('2024-01-15')").expect("insert");
        let result = run(&storage, "SELECT d FROM t").expect("select");
        assert_eq!(
            result.rows,
            vec![vec![Value::Date("2024-01-15".to_string())]]
        );
    }

    #[test]
    fn date_orders_chronologically() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (d DATE)").expect("create");
        for d in ["2024-03-01", "2023-12-25", "2024-01-01"] {
            run(&storage, &format!("INSERT INTO t VALUES ('{d}')")).expect("insert");
        }
        let result = run(&storage, "SELECT d FROM t ORDER BY d").expect("select");
        assert_eq!(
            result.rows,
            vec![
                vec![Value::Date("2023-12-25".to_string())],
                vec![Value::Date("2024-01-01".to_string())],
                vec![Value::Date("2024-03-01".to_string())],
            ]
        );
    }

    #[test]
    fn malformed_date_literal_is_rejected() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (d DATE)").expect("create");
        for bad in [
            "not-a-date",
            "2024-13-01",
            "2024-01-32",
            "2024/01/15",
            "2024-1-1",
        ] {
            assert!(
                matches!(
                    run(&storage, &format!("INSERT INTO t VALUES ('{bad}')")),
                    Err(Error::Execution(_))
                ),
                "expected '{bad}' to be rejected"
            );
        }
    }

    #[test]
    fn non_string_literal_into_date_column_is_rejected() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (d DATE)").expect("create");
        assert!(matches!(
            run(&storage, "INSERT INTO t VALUES (20240115)"),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn malformed_decimal_literal_is_rejected() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (price DECIMAL(10,2))").expect("create");
        assert!(matches!(
            run(&storage, "INSERT INTO t VALUES ('not-a-number')"),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn decimal_default_scale_is_zero() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a DECIMAL, b DECIMAL(5))").expect("create");
        run(&storage, "INSERT INTO t VALUES (3.7, 3.7)").expect("insert");
        let result = run(&storage, "SELECT a, b FROM t").expect("select");
        // Both round to scale 0: 3.7 -> 4.
        assert_eq!(
            result.rows,
            vec![vec![Value::Decimal(4, 0), Value::Decimal(4, 0)]]
        );
    }

    // ---- GROUP BY / aggregate functions (Phase 11) ----

    fn seed_sales_table(storage: &InMemoryStorage) {
        run(
            storage,
            "CREATE TABLE sales (id INT PRIMARY KEY, category VARCHAR, amount DECIMAL(10,2))",
        )
        .expect("create");
        for (id, category, amount) in [
            (1, "fruit", "10.00"),
            (2, "fruit", "5.50"),
            (3, "veg", "3.25"),
            (4, "veg", "7.75"),
            (5, "veg", "1.00"),
        ] {
            run(
                storage,
                &format!("INSERT INTO sales VALUES ({id}, '{category}', {amount})"),
            )
            .expect("insert");
        }
    }

    #[test]
    fn count_star_counts_all_rows_including_null_columns() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT, b VARCHAR)").expect("create");
        run(&storage, "INSERT INTO t VALUES (1, NULL)").expect("insert");
        run(&storage, "INSERT INTO t VALUES (2, 'x')").expect("insert");
        let result = run(&storage, "SELECT COUNT(*) FROM t").expect("select");
        assert_eq!(result.rows, vec![vec![int(2)]]);
        assert_eq!(result.columns[0].column_type, ColumnType::Int);
    }

    #[test]
    fn count_column_skips_null_values() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT, b VARCHAR)").expect("create");
        run(&storage, "INSERT INTO t VALUES (1, NULL)").expect("insert");
        run(&storage, "INSERT INTO t VALUES (2, 'x')").expect("insert");
        let result = run(&storage, "SELECT COUNT(b) FROM t").expect("select");
        assert_eq!(result.rows, vec![vec![int(1)]]);
    }

    #[test]
    fn count_on_an_empty_table_is_zero_not_no_rows() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT)").expect("create");
        let result = run(&storage, "SELECT COUNT(*) FROM t").expect("select");
        // No GROUP BY: always exactly one output row, even for zero source
        // rows — distinct from an explicit GROUP BY with zero matches.
        assert_eq!(result.rows, vec![vec![int(0)]]);
    }

    #[test]
    fn sum_int_and_decimal_columns() {
        let storage = InMemoryStorage::new();
        seed_sales_table(&storage);
        let result = run(&storage, "SELECT SUM(amount) FROM sales").expect("select");
        // 10.00 + 5.50 + 3.25 + 7.75 + 1.00 = 27.50
        assert_eq!(result.rows, vec![vec![Value::Decimal(2750, 2)]]);
    }

    #[test]
    fn sum_of_all_null_is_null_not_zero() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT)").expect("create");
        run(&storage, "INSERT INTO t VALUES (NULL)").expect("insert");
        run(&storage, "INSERT INTO t VALUES (NULL)").expect("insert");
        let result = run(&storage, "SELECT SUM(a) FROM t").expect("select");
        assert_eq!(result.rows, vec![vec![Value::Null]]);
    }

    #[test]
    fn avg_int_column_returns_decimal() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT)").expect("create");
        for v in [1, 2, 4] {
            run(&storage, &format!("INSERT INTO t VALUES ({v})")).expect("insert");
        }
        let result = run(&storage, "SELECT AVG(a) FROM t").expect("select");
        // (1+2+4)/3 = 2.3333... at scale 4 (0 + 4).
        assert_eq!(result.rows, vec![vec![Value::Decimal(23333, 4)]]);
        assert_eq!(result.columns[0].column_type, ColumnType::Decimal(4));
    }

    #[test]
    fn avg_decimal_column() {
        let storage = InMemoryStorage::new();
        seed_sales_table(&storage);
        let result = run(&storage, "SELECT AVG(amount) FROM sales").expect("select");
        // 27.50 / 5 = 5.5000 at scale 6 (2 + 4).
        assert_eq!(result.rows, vec![vec![Value::Decimal(5500000, 6)]]);
    }

    #[test]
    fn avg_of_zero_rows_is_null() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT)").expect("create");
        let result = run(&storage, "SELECT AVG(a) FROM t").expect("select");
        assert_eq!(result.rows, vec![vec![Value::Null]]);
    }

    #[test]
    fn min_and_max_skip_nulls_and_work_on_any_comparable_type() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT, b VARCHAR, c DATE)").expect("create");
        run(&storage, "INSERT INTO t VALUES (5, 'banana', '2024-06-01')").expect("insert");
        run(&storage, "INSERT INTO t VALUES (NULL, NULL, NULL)").expect("insert");
        run(&storage, "INSERT INTO t VALUES (1, 'apple', '2024-01-01')").expect("insert");
        let result = run(&storage, "SELECT MIN(a), MAX(a), MIN(b), MAX(c) FROM t").expect("select");
        assert_eq!(
            result.rows,
            vec![vec![
                int(1),
                int(5),
                vc("apple"),
                Value::Date("2024-06-01".to_string()),
            ]]
        );
    }

    #[test]
    fn sum_on_a_non_numeric_column_is_rejected() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a VARCHAR)").expect("create");
        assert!(matches!(
            run(&storage, "SELECT SUM(a) FROM t"),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn group_by_partitions_and_aggregates_per_group() {
        let storage = InMemoryStorage::new();
        seed_sales_table(&storage);
        let result = run(
            &storage,
            "SELECT category, COUNT(*), SUM(amount) FROM sales GROUP BY category",
        )
        .expect("select");
        // Deterministic (sorted-by-key) group order: "fruit" before "veg".
        assert_eq!(
            result.rows,
            vec![
                vec![vc("fruit"), int(2), Value::Decimal(1550, 2)], // 10.00+5.50
                vec![vc("veg"), int(3), Value::Decimal(1200, 2)],   // 3.25+7.75+1.00
            ]
        );
        assert_eq!(
            result.column_names(),
            vec!["category", "COUNT(*)", "SUM(amount)"]
        );
    }

    #[test]
    fn group_by_aggregate_column_aliases_are_used_as_output_names() {
        let storage = InMemoryStorage::new();
        seed_sales_table(&storage);
        let result = run(
            &storage,
            "SELECT category, COUNT(*) AS total FROM sales GROUP BY category",
        )
        .expect("select");
        assert_eq!(result.column_names(), vec!["category", "total"]);
    }

    #[test]
    fn group_by_where_filters_before_grouping() {
        let storage = InMemoryStorage::new();
        seed_sales_table(&storage);
        let result = run(
            &storage,
            "SELECT category, COUNT(*) FROM sales WHERE amount > 4.00 GROUP BY category",
        )
        .expect("select");
        // Only rows with amount > 4.00: fruit(10.00, 5.50) both qualify;
        // veg only 7.75 does (3.25 and 1.00 don't).
        assert_eq!(
            result.rows,
            vec![vec![vc("fruit"), int(2)], vec![vc("veg"), int(1)]]
        );
    }

    #[test]
    fn group_by_order_by_resolves_against_the_aggregate_alias() {
        let storage = InMemoryStorage::new();
        seed_sales_table(&storage);
        let result = run(
            &storage,
            "SELECT category, COUNT(*) AS total FROM sales GROUP BY category ORDER BY total DESC",
        )
        .expect("select");
        assert_eq!(
            result.rows,
            vec![vec![vc("veg"), int(3)], vec![vc("fruit"), int(2)],]
        );
    }

    #[test]
    fn group_by_with_limit() {
        let storage = InMemoryStorage::new();
        seed_sales_table(&storage);
        let result = run(
            &storage,
            "SELECT category, COUNT(*) AS total FROM sales GROUP BY category \
             ORDER BY total DESC LIMIT 1",
        )
        .expect("select");
        assert_eq!(result.rows, vec![vec![vc("veg"), int(3)]]);
    }

    #[test]
    fn group_by_on_an_empty_table_yields_zero_groups() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (category VARCHAR, a INT)").expect("create");
        let result = run(
            &storage,
            "SELECT category, COUNT(*) FROM t GROUP BY category",
        )
        .expect("select");
        // Unlike a plain (no-GROUP-BY) aggregate, an explicit GROUP BY with
        // zero matching rows produces zero *rows* — but still the right
        // *columns*.
        assert!(result.rows.is_empty());
        assert_eq!(result.column_names(), vec!["category", "COUNT(*)"]);
    }

    #[test]
    fn non_grouped_bare_column_in_a_group_by_query_is_rejected() {
        let storage = InMemoryStorage::new();
        seed_sales_table(&storage);
        // `id` is neither aggregated nor in GROUP BY — standard SQL rejects
        // this (MySQL's own ONLY_FULL_GROUP_BY default does too).
        assert!(matches!(
            run(&storage, "SELECT id, category FROM sales GROUP BY category"),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn wildcard_in_a_group_by_query_is_unsupported() {
        let storage = InMemoryStorage::new();
        seed_sales_table(&storage);
        assert!(matches!(
            run(&storage, "SELECT * FROM sales GROUP BY category"),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn aggregate_with_more_than_one_argument_is_unsupported() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT, b INT)").expect("create");
        assert!(matches!(
            run(&storage, "SELECT COUNT(a, b) FROM t"),
            Err(Error::Unsupported(_))
        ));
    }

    // ---- JOIN (Phase 11) ----

    /// `customers`: Ada(1), Grace(2), Alan(3) — Alan has no orders, so he's
    /// the row a `LEFT JOIN` must keep (NULL-padded) and an `INNER JOIN`
    /// must drop. `orders`: two for Ada (100, 101), one for Grace (102) —
    /// Ada's two rows exercise one-to-many fan-out. Both tables have an
    /// `id` column, deliberately, so an unqualified `SELECT id` is
    /// ambiguous once they're joined.
    fn seed_orders_and_customers(storage: &InMemoryStorage) {
        run(
            storage,
            "CREATE TABLE customers (id INT PRIMARY KEY, name VARCHAR)",
        )
        .expect("create customers");
        run(
            storage,
            "CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT, total DECIMAL(10,2))",
        )
        .expect("create orders");
        for (id, name) in [(1, "Ada"), (2, "Grace"), (3, "Alan")] {
            run(
                storage,
                &format!("INSERT INTO customers VALUES ({id}, '{name}')"),
            )
            .expect("insert customer");
        }
        for (id, customer_id, total) in [(100, 1, "9.99"), (101, 1, "5.00"), (102, 2, "20.00")] {
            run(
                storage,
                &format!("INSERT INTO orders VALUES ({id}, {customer_id}, {total})"),
            )
            .expect("insert order");
        }
    }

    #[test]
    fn inner_join_returns_only_matching_rows() {
        let storage = InMemoryStorage::new();
        seed_orders_and_customers(&storage);
        let result = run(
            &storage,
            "SELECT c.name, o.total FROM customers c JOIN orders o \
             ON c.id = o.customer_id ORDER BY o.id",
        )
        .expect("select");
        assert_eq!(result.column_names(), vec!["name", "total"]);
        assert_eq!(
            result.rows,
            vec![
                vec![vc("Ada"), Value::Decimal(999, 2)],
                vec![vc("Ada"), Value::Decimal(500, 2)],
                vec![vc("Grace"), Value::Decimal(2000, 2)],
            ]
        );
    }

    #[test]
    fn left_join_pads_unmatched_rows_with_null() {
        let storage = InMemoryStorage::new();
        seed_orders_and_customers(&storage);
        let result = run(
            &storage,
            "SELECT c.name, o.total FROM customers c LEFT JOIN orders o \
             ON c.id = o.customer_id ORDER BY c.id, o.id",
        )
        .expect("select");
        assert_eq!(
            result.rows,
            vec![
                vec![vc("Ada"), Value::Decimal(999, 2)],
                vec![vc("Ada"), Value::Decimal(500, 2)],
                vec![vc("Grace"), Value::Decimal(2000, 2)],
                vec![vc("Alan"), Value::Null],
            ]
        );
    }

    #[test]
    fn join_where_filters_after_the_join() {
        let storage = InMemoryStorage::new();
        seed_orders_and_customers(&storage);
        let result = run(
            &storage,
            "SELECT c.name FROM customers c JOIN orders o \
             ON c.id = o.customer_id WHERE o.total > 6.00",
        )
        .expect("select");
        assert_eq!(result.rows, vec![vec![vc("Ada")], vec![vc("Grace")]]);
    }

    #[test]
    fn join_unqualified_column_resolves_when_unambiguous() {
        let storage = InMemoryStorage::new();
        seed_orders_and_customers(&storage);
        // No alias given, so each table's own name is its qualifier; `name`/
        // `total`/`customer_id` each exist on only one side, so referencing
        // them unqualified is fine even though `id` alone would be ambiguous.
        let result = run(
            &storage,
            "SELECT name, total FROM customers JOIN orders \
             ON customers.id = orders.customer_id WHERE customer_id = 1 ORDER BY total",
        )
        .expect("select");
        assert_eq!(
            result.rows,
            vec![
                vec![vc("Ada"), Value::Decimal(500, 2)],
                vec![vc("Ada"), Value::Decimal(999, 2)],
            ]
        );
    }

    #[test]
    fn join_unqualified_ambiguous_column_is_an_error() {
        let storage = InMemoryStorage::new();
        seed_orders_and_customers(&storage);
        assert!(matches!(
            run(
                &storage,
                "SELECT id FROM customers c JOIN orders o ON c.id = o.customer_id"
            ),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn join_on_referencing_an_unknown_column_is_a_clear_error() {
        let storage = InMemoryStorage::new();
        seed_orders_and_customers(&storage);
        assert!(matches!(
            run(
                &storage,
                "SELECT * FROM customers c JOIN orders o ON c.nonexistent = o.customer_id"
            ),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn join_wildcard_projects_columns_from_every_table_in_order() {
        let storage = InMemoryStorage::new();
        seed_orders_and_customers(&storage);
        let result = run(
            &storage,
            "SELECT * FROM customers c JOIN orders o ON c.id = o.customer_id WHERE o.id = 100",
        )
        .expect("select");
        // Both tables have an `id` column — a wildcard still reports both,
        // unqualified, in FROM order, same as real MySQL's wire output.
        assert_eq!(
            result.column_names(),
            vec!["id", "name", "id", "customer_id", "total"]
        );
        assert_eq!(
            result.rows,
            vec![vec![
                int(1),
                vc("Ada"),
                int(100),
                int(1),
                Value::Decimal(999, 2)
            ]]
        );
    }

    #[test]
    fn group_by_over_a_join() {
        let storage = InMemoryStorage::new();
        seed_orders_and_customers(&storage);
        let result = run(
            &storage,
            "SELECT c.name, COUNT(*), SUM(o.total) FROM customers c JOIN orders o \
             ON c.id = o.customer_id GROUP BY c.name ORDER BY c.name",
        )
        .expect("select");
        assert_eq!(
            result.rows,
            vec![
                vec![vc("Ada"), int(2), Value::Decimal(1499, 2)],
                vec![vc("Grace"), int(1), Value::Decimal(2000, 2)],
            ]
        );
    }

    #[test]
    fn chained_join_across_three_tables() {
        let storage = InMemoryStorage::new();
        run(
            &storage,
            "CREATE TABLE a (id INT PRIMARY KEY, label VARCHAR)",
        )
        .expect("create a");
        run(
            &storage,
            "CREATE TABLE b (id INT PRIMARY KEY, a_id INT, label VARCHAR)",
        )
        .expect("create b");
        run(
            &storage,
            "CREATE TABLE c (id INT PRIMARY KEY, b_id INT, label VARCHAR)",
        )
        .expect("create c");
        run(&storage, "INSERT INTO a VALUES (1, 'a1')").expect("insert a");
        run(&storage, "INSERT INTO b VALUES (10, 1, 'b1')").expect("insert b");
        run(&storage, "INSERT INTO c VALUES (100, 10, 'c1')").expect("insert c");

        let result = run(
            &storage,
            "SELECT a.label, b.label, c.label FROM a \
             JOIN b ON a.id = b.a_id \
             JOIN c ON b.id = c.b_id",
        )
        .expect("select");
        assert_eq!(result.rows, vec![vec![vc("a1"), vc("b1"), vc("c1")]]);
    }

    #[test]
    fn null_join_key_never_matches_even_for_inner_join() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE a (id INT PRIMARY KEY, x INT)").expect("create a");
        run(&storage, "CREATE TABLE b (id INT PRIMARY KEY, x INT)").expect("create b");
        run(&storage, "INSERT INTO a VALUES (1, NULL)").expect("insert a");
        run(&storage, "INSERT INTO b VALUES (1, NULL)").expect("insert b");
        let result = run(&storage, "SELECT * FROM a JOIN b ON a.x = b.x").expect("select");
        assert_eq!(result.rows, Vec::<Vec<Value>>::new());
    }

    #[test]
    fn join_on_condition_works_with_either_side_order() {
        let storage = InMemoryStorage::new();
        seed_orders_and_customers(&storage);
        let a = run(
            &storage,
            "SELECT COUNT(*) FROM customers c JOIN orders o ON c.id = o.customer_id",
        )
        .expect("select a");
        let b = run(
            &storage,
            "SELECT COUNT(*) FROM customers c JOIN orders o ON o.customer_id = c.id",
        )
        .expect("select b");
        assert_eq!(a.rows, b.rows);
        assert_eq!(a.rows, vec![vec![int(3)]]);
    }

    #[test]
    fn join_with_order_by_and_limit() {
        let storage = InMemoryStorage::new();
        seed_orders_and_customers(&storage);
        let result = run(
            &storage,
            "SELECT o.id FROM customers c JOIN orders o \
             ON c.id = o.customer_id ORDER BY o.total DESC LIMIT 1",
        )
        .expect("select");
        assert_eq!(result.rows, vec![vec![int(102)]]);
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
    fn multi_row_insert_rejects_a_duplicate_key_within_the_same_statement() {
        let storage = InMemoryStorage::new();
        run(
            &storage,
            "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR)",
        )
        .expect("create");
        assert!(matches!(
            run(
                &storage,
                "INSERT INTO t VALUES (1, 'alice'), (2, 'bob'), (1, 'carol')"
            ),
            Err(Error::Execution(_))
        ));
        // Not just the colliding row: the whole statement, including rows
        // 1 and 2 which were individually fine, must not have applied
        // (PERFORMANCE_DURABILITY_PLAN.md D2 -- one statement, one
        // client-visible outcome).
        assert!(run(&storage, "SELECT * FROM t")
            .expect("select")
            .rows
            .is_empty());
    }

    #[test]
    fn multi_row_insert_is_all_or_nothing_when_a_later_row_fails_to_coerce() {
        // Not a crash scenario -- a plain type error partway through a
        // multi-row INSERT. Before PERFORMANCE_DURABILITY_PLAN.md D2, rows
        // before the bad one were already applied by the time this was
        // discovered; now the whole batch is validated before anything is
        // applied, so a statement that fails even for a completely
        // ordinary reason still leaves nothing behind.
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (id INT PRIMARY KEY)").expect("create");
        assert!(matches!(
            run(&storage, "INSERT INTO t VALUES (1), (2), ('not-a-number')"),
            Err(Error::Execution(_))
        ));
        assert!(run(&storage, "SELECT * FROM t")
            .expect("select")
            .rows
            .is_empty());
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

    fn seed_order_by_table(storage: &InMemoryStorage) {
        run(storage, "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR)").expect("create");
        for (id, name) in [(1, "carol"), (2, "alice"), (3, "bob")] {
            run(storage, &format!("INSERT INTO t VALUES ({id}, '{name}')")).expect("insert");
        }
    }

    #[test]
    fn order_by_ascending_sorts_a_varchar_column() {
        let storage = InMemoryStorage::new();
        seed_order_by_table(&storage);
        let result = run(&storage, "SELECT name FROM t ORDER BY name").expect("select");
        assert_eq!(
            result.rows,
            vec![vec![vc("alice")], vec![vc("bob")], vec![vc("carol")]]
        );
    }

    #[test]
    fn order_by_descending_reverses_the_order() {
        let storage = InMemoryStorage::new();
        seed_order_by_table(&storage);
        let result = run(&storage, "SELECT name FROM t ORDER BY name DESC").expect("select");
        assert_eq!(
            result.rows,
            vec![vec![vc("carol")], vec![vc("bob")], vec![vc("alice")]]
        );
    }

    #[test]
    fn order_by_can_reference_a_column_not_in_the_projection() {
        let storage = InMemoryStorage::new();
        seed_order_by_table(&storage);
        // Sorted by `id` (descending) even though only `name` is projected.
        let result = run(&storage, "SELECT name FROM t ORDER BY id DESC").expect("select");
        assert_eq!(
            result.rows,
            vec![vec![vc("bob")], vec![vc("alice")], vec![vc("carol")]]
        );
    }

    #[test]
    fn order_by_sorts_null_first_ascending() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT, b VARCHAR)").expect("create");
        run(&storage, "INSERT INTO t VALUES (1, 'b')").expect("insert");
        run(&storage, "INSERT INTO t VALUES (2, NULL)").expect("insert");
        run(&storage, "INSERT INTO t VALUES (3, 'a')").expect("insert");
        let result = run(&storage, "SELECT b FROM t ORDER BY b").expect("select");
        assert_eq!(
            result.rows,
            vec![vec![Value::Null], vec![vc("a")], vec![vc("b")]]
        );
    }

    #[test]
    fn order_by_numeric_column_sorts_numerically_not_lexically() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (id INT)").expect("create");
        for v in ["9", "10", "1", "20"] {
            run(&storage, &format!("INSERT INTO t VALUES ({v})")).expect("insert");
        }
        let result = run(&storage, "SELECT id FROM t ORDER BY id").expect("select");
        assert_eq!(
            result.rows,
            vec![vec![int(1)], vec![int(9)], vec![int(10)], vec![int(20)]]
        );
    }

    #[test]
    fn order_by_multiple_columns_breaks_ties_with_the_second_key() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT, b INT)").expect("create");
        for (a, b) in [(1, 2), (1, 1), (2, 1)] {
            run(&storage, &format!("INSERT INTO t VALUES ({a}, {b})")).expect("insert");
        }
        let result = run(&storage, "SELECT a, b FROM t ORDER BY a, b").expect("select");
        assert_eq!(
            result.rows,
            vec![
                vec![int(1), int(1)],
                vec![int(1), int(2)],
                vec![int(2), int(1)],
            ]
        );
    }

    #[test]
    fn order_by_unknown_column_errors() {
        let storage = InMemoryStorage::new();
        run(&storage, "CREATE TABLE t (a INT)").expect("create");
        assert!(matches!(
            run(&storage, "SELECT a FROM t ORDER BY bogus"),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn limit_caps_the_row_count() {
        let storage = InMemoryStorage::new();
        seed_order_by_table(&storage);
        let result = run(&storage, "SELECT id FROM t ORDER BY id LIMIT 2").expect("select");
        assert_eq!(result.rows, vec![vec![int(1)], vec![int(2)]]);
    }

    #[test]
    fn limit_zero_returns_no_rows() {
        let storage = InMemoryStorage::new();
        seed_order_by_table(&storage);
        let result = run(&storage, "SELECT id FROM t LIMIT 0").expect("select");
        assert!(result.rows.is_empty());
    }

    #[test]
    fn offset_skips_leading_rows() {
        let storage = InMemoryStorage::new();
        seed_order_by_table(&storage);
        let result =
            run(&storage, "SELECT id FROM t ORDER BY id LIMIT 10 OFFSET 1").expect("select");
        assert_eq!(result.rows, vec![vec![int(2)], vec![int(3)]]);
    }

    #[test]
    fn offset_past_the_end_returns_no_rows() {
        let storage = InMemoryStorage::new();
        seed_order_by_table(&storage);
        let result = run(&storage, "SELECT id FROM t LIMIT 10 OFFSET 100").expect("select");
        assert!(result.rows.is_empty());
    }

    #[test]
    fn limit_comma_offset_form_pages_correctly() {
        let storage = InMemoryStorage::new();
        seed_order_by_table(&storage);
        // `LIMIT 1, 2` = skip 1, take 2.
        let result = run(&storage, "SELECT id FROM t ORDER BY id LIMIT 1, 2").expect("select");
        assert_eq!(result.rows, vec![vec![int(2)], vec![int(3)]]);
    }

    #[test]
    fn limit_without_from_clause_caps_the_single_row() {
        let storage = InMemoryStorage::new();
        assert_eq!(
            run(&storage, "SELECT 1 LIMIT 1").expect("select").rows,
            vec![vec![int(1)]]
        );
        assert!(run(&storage, "SELECT 1 LIMIT 0")
            .expect("select")
            .rows
            .is_empty());
        // `OFFSET` is only valid as part of a `LIMIT` clause, same as MySQL.
        assert!(run(&storage, "SELECT 1 LIMIT 1 OFFSET 1")
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
