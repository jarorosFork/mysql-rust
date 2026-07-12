//! SQL tokenizer, AST, and recursive-descent parser for a small subset of
//! SQL: `CREATE TABLE`, `INSERT`, and `SELECT` with an optional single-
//! predicate `WHERE`. AND/OR chaining, joins, and typed values are out of
//! scope for now (see ROADMAP.md Phase 4/5).

use crate::{Error, Result};

// ---------- AST ----------

/// A parsed SQL statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable {
        table: String,
        columns: Vec<ColumnDef>,
    },
    Insert {
        table: String,
        /// Explicit column list, if given (`INSERT INTO t (a, b) VALUES ...`).
        /// `None` means values are supplied in table-definition order.
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Expr>>,
    },
    Select {
        projection: Vec<SelectItem>,
        from: Option<String>,
        selection: Option<Condition>,
    },
    /// `BEGIN` or `START TRANSACTION`.
    Begin,
    Commit,
    Rollback,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    /// Recorded as-written; validated against `storage::ColumnType` by the
    /// executor at `CREATE TABLE` time.
    pub type_name: String,
    /// Whether this column was declared `PRIMARY KEY`.
    pub is_primary_key: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Wildcard,
    Expr(Expr),
}

/// A literal or column reference.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Integer(i64),
    String(String),
    Null,
    SystemVariable(String),
    Column(String),
    /// A `?` placeholder in a prepared statement, carrying its zero-based
    /// positional index. Only produced when placeholders are allowed (see
    /// [`parse_prepared`]); it must be replaced with a concrete literal by
    /// [`bind_parameters`] before the statement reaches the executor.
    Placeholder(usize),
}

impl Expr {
    /// Render as display text for a literal with no table/column context
    /// (e.g. `SELECT 1`, `SELECT NULL`). `None` means SQL `NULL`.
    pub fn to_value_string(&self) -> Option<String> {
        match self {
            Expr::Integer(n) => Some(n.to_string()),
            Expr::String(s) => Some(s.clone()),
            Expr::Null => None,
            Expr::SystemVariable(name) => Some(format!("@@{name}")),
            Expr::Column(name) => Some(name.clone()),
            Expr::Placeholder(i) => Some(format!("?{i}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    NotEq,
    Lt,
    Gt,
    Le,
    Ge,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    pub column: String,
    pub op: CompareOp,
    pub value: Expr,
}

// ---------- Tokenizer ----------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Keyword(Keyword),
    Ident(String),
    SystemVariable(String),
    Integer(i64),
    Str(String),
    Comma,
    LParen,
    RParen,
    Semicolon,
    Star,
    Eq,
    NotEq,
    Lt,
    Gt,
    Le,
    Ge,
    /// `?` — a prepared-statement parameter placeholder.
    Placeholder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keyword {
    Create,
    Table,
    Insert,
    Into,
    Values,
    Select,
    From,
    Where,
    Null,
    Primary,
    Key,
    Begin,
    Start,
    Transaction,
    Commit,
    Rollback,
}

fn keyword_text(kw: Keyword) -> String {
    format!("{kw:?}").to_ascii_uppercase()
}

fn keyword_from_ident(s: &str) -> Option<Keyword> {
    match s.to_ascii_uppercase().as_str() {
        "CREATE" => Some(Keyword::Create),
        "TABLE" => Some(Keyword::Table),
        "INSERT" => Some(Keyword::Insert),
        "INTO" => Some(Keyword::Into),
        "VALUES" => Some(Keyword::Values),
        "SELECT" => Some(Keyword::Select),
        "FROM" => Some(Keyword::From),
        "WHERE" => Some(Keyword::Where),
        "NULL" => Some(Keyword::Null),
        "PRIMARY" => Some(Keyword::Primary),
        "KEY" => Some(Keyword::Key),
        "BEGIN" => Some(Keyword::Begin),
        "START" => Some(Keyword::Start),
        "TRANSACTION" => Some(Keyword::Transaction),
        "COMMIT" => Some(Keyword::Commit),
        "ROLLBACK" => Some(Keyword::Rollback),
        _ => None,
    }
}

fn token_text(token: &Token) -> String {
    match token {
        Token::Keyword(k) => keyword_text(*k),
        Token::Ident(s) => s.clone(),
        Token::SystemVariable(s) => format!("@@{s}"),
        Token::Integer(n) => n.to_string(),
        Token::Str(s) => format!("'{s}'"),
        Token::Comma => ",".to_string(),
        Token::LParen => "(".to_string(),
        Token::RParen => ")".to_string(),
        Token::Semicolon => ";".to_string(),
        Token::Star => "*".to_string(),
        Token::Eq => "=".to_string(),
        Token::NotEq => "<>".to_string(),
        Token::Lt => "<".to_string(),
        Token::Gt => ">".to_string(),
        Token::Le => "<=".to_string(),
        Token::Ge => ">=".to_string(),
        Token::Placeholder => "?".to_string(),
    }
}

fn describe(token: Option<&Token>) -> String {
    match token {
        None => "end of input".to_string(),
        Some(t) => format!("'{}'", token_text(t)),
    }
}

fn tokenize(sql: &str) -> Result<Vec<Token>> {
    let chars: Vec<char> = sql.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if c.is_whitespace() {
            i += 1;
            continue;
        }

        match c {
            ',' => {
                tokens.push(Token::Comma);
                i += 1;
            }
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            ';' => {
                tokens.push(Token::Semicolon);
                i += 1;
            }
            '*' => {
                tokens.push(Token::Star);
                i += 1;
            }
            '?' => {
                tokens.push(Token::Placeholder);
                i += 1;
            }
            '=' => {
                tokens.push(Token::Eq);
                i += 1;
            }
            '<' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Token::Le);
                    i += 2;
                } else if chars.get(i + 1) == Some(&'>') {
                    tokens.push(Token::NotEq);
                    i += 2;
                } else {
                    tokens.push(Token::Lt);
                    i += 1;
                }
            }
            '>' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Token::Ge);
                    i += 2;
                } else {
                    tokens.push(Token::Gt);
                    i += 1;
                }
            }
            '!' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Token::NotEq);
                    i += 2;
                } else {
                    return Err(Error::Parse(format!(
                        "unexpected character '!' at position {i}"
                    )));
                }
            }
            '\'' => {
                let (s, consumed) = scan_string_literal(&chars[i..])?;
                tokens.push(Token::Str(s));
                i += consumed;
            }
            '@' => {
                if chars.get(i + 1) == Some(&'@') {
                    let (name, consumed) = scan_ident(&chars[i + 2..]);
                    if name.is_empty() {
                        return Err(Error::Parse(format!(
                            "expected an identifier after '@@' at position {i}"
                        )));
                    }
                    tokens.push(Token::SystemVariable(name));
                    i += 2 + consumed;
                } else {
                    return Err(Error::Parse(format!(
                        "unexpected character '@' at position {i}"
                    )));
                }
            }
            c if c.is_ascii_digit()
                || (c == '-' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit())) =>
            {
                let (n, consumed) = scan_integer(&chars[i..])?;
                tokens.push(Token::Integer(n));
                i += consumed;
            }
            c if c.is_alphabetic() || c == '_' => {
                let (word, consumed) = scan_ident(&chars[i..]);
                i += consumed;
                tokens.push(match keyword_from_ident(&word) {
                    Some(k) => Token::Keyword(k),
                    None => Token::Ident(word),
                });
            }
            other => {
                return Err(Error::Parse(format!(
                    "unexpected character '{other}' at position {i}"
                )));
            }
        }
    }

    Ok(tokens)
}

fn scan_ident(chars: &[char]) -> (String, usize) {
    let mut n = 0;
    while n < chars.len() && (chars[n].is_alphanumeric() || chars[n] == '_') {
        n += 1;
    }
    (chars[..n].iter().collect(), n)
}

fn scan_integer(chars: &[char]) -> Result<(i64, usize)> {
    let mut n = 0;
    if chars.first() == Some(&'-') {
        n += 1;
    }
    let start_digits = n;
    while n < chars.len() && chars[n].is_ascii_digit() {
        n += 1;
    }
    if n == start_digits {
        return Err(Error::Parse(
            "expected digits in integer literal".to_string(),
        ));
    }
    let text: String = chars[..n].iter().collect();
    let value = text
        .parse::<i64>()
        .map_err(|_| Error::Parse(format!("integer literal '{text}' out of range")))?;
    Ok((value, n))
}

/// `chars[0]` must be the opening `'`.
fn scan_string_literal(chars: &[char]) -> Result<(String, usize)> {
    let mut n = 1;
    let mut s = String::new();
    loop {
        match chars.get(n) {
            None => return Err(Error::Parse("unterminated string literal".to_string())),
            Some('\'') => {
                if chars.get(n + 1) == Some(&'\'') {
                    s.push('\'');
                    n += 2;
                } else {
                    n += 1;
                    break;
                }
            }
            Some(&c) => {
                s.push(c);
                n += 1;
            }
        }
    }
    Ok((s, n))
}

// ---------- Parser ----------

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    /// Whether `?` placeholders are permitted (only in the prepared-statement
    /// path). In the plain text-query path a `?` is a syntax error, matching
    /// MySQL: text `COM_QUERY` can't carry parameters.
    allow_placeholders: bool,
    /// Number of placeholders seen so far; also the next placeholder's index.
    param_count: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token], allow_placeholders: bool) -> Self {
        Parser {
            tokens,
            pos: 0,
            allow_placeholders,
            param_count: 0,
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<&Token> {
        let t = self.tokens.get(self.pos);
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect_keyword(&mut self, kw: Keyword) -> Result<()> {
        match self.advance() {
            Some(Token::Keyword(k)) if *k == kw => Ok(()),
            other => Err(Error::Parse(format!(
                "expected {}, found {}",
                keyword_text(kw),
                describe(other)
            ))),
        }
    }

    fn expect_ident(&mut self) -> Result<String> {
        match self.advance() {
            Some(Token::Ident(s)) => Ok(s.clone()),
            other => Err(Error::Parse(format!(
                "expected an identifier, found {}",
                describe(other)
            ))),
        }
    }

    fn expect_punct(&mut self, expected: &Token) -> Result<()> {
        match self.advance() {
            Some(t) if t == expected => Ok(()),
            other => Err(Error::Parse(format!(
                "expected '{}', found {}",
                token_text(expected),
                describe(other)
            ))),
        }
    }

    fn eat_semicolon_and_ensure_end(&mut self) -> Result<()> {
        if let Some(Token::Semicolon) = self.peek() {
            self.pos += 1;
        }
        if self.pos != self.tokens.len() {
            return Err(Error::Parse(format!(
                "unexpected trailing input near {}",
                describe(self.peek())
            )));
        }
        Ok(())
    }

    fn parse_statement(&mut self) -> Result<Statement> {
        match self.peek() {
            Some(Token::Keyword(Keyword::Create)) => self.parse_create_table(),
            Some(Token::Keyword(Keyword::Insert)) => self.parse_insert(),
            Some(Token::Keyword(Keyword::Select)) => self.parse_select(),
            Some(Token::Keyword(Keyword::Begin)) => self.parse_begin(),
            Some(Token::Keyword(Keyword::Start)) => self.parse_start_transaction(),
            Some(Token::Keyword(Keyword::Commit)) => {
                self.parse_simple(Keyword::Commit, Statement::Commit)
            }
            Some(Token::Keyword(Keyword::Rollback)) => {
                self.parse_simple(Keyword::Rollback, Statement::Rollback)
            }
            other => Err(Error::Parse(format!(
                "expected CREATE, INSERT, SELECT, BEGIN, START, COMMIT, or ROLLBACK, found {}",
                describe(other)
            ))),
        }
    }

    fn parse_begin(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Begin)?;
        self.eat_semicolon_and_ensure_end()?;
        Ok(Statement::Begin)
    }

    fn parse_start_transaction(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Start)?;
        self.expect_keyword(Keyword::Transaction)?;
        self.eat_semicolon_and_ensure_end()?;
        Ok(Statement::Begin)
    }

    /// Parse a bare `<keyword> [;]` statement (`COMMIT`, `ROLLBACK`).
    fn parse_simple(&mut self, kw: Keyword, statement: Statement) -> Result<Statement> {
        self.expect_keyword(kw)?;
        self.eat_semicolon_and_ensure_end()?;
        Ok(statement)
    }

    fn parse_create_table(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Create)?;
        self.expect_keyword(Keyword::Table)?;
        let table = self.expect_ident()?;
        self.expect_punct(&Token::LParen)?;

        let mut columns = Vec::new();
        loop {
            let name = self.expect_ident()?;
            let type_name = self.expect_ident()?;
            // Optional size, e.g. VARCHAR(255); consumed, not stored.
            if let Some(Token::LParen) = self.peek() {
                self.pos += 1;
                match self.advance() {
                    Some(Token::Integer(_)) => {}
                    other => {
                        return Err(Error::Parse(format!(
                            "expected an integer in type size, found {}",
                            describe(other)
                        )))
                    }
                }
                self.expect_punct(&Token::RParen)?;
            }

            let is_primary_key = if let Some(Token::Keyword(Keyword::Primary)) = self.peek() {
                self.pos += 1;
                self.expect_keyword(Keyword::Key)?;
                true
            } else {
                false
            };

            columns.push(ColumnDef {
                name,
                type_name,
                is_primary_key,
            });

            match self.advance() {
                Some(Token::Comma) => continue,
                Some(Token::RParen) => break,
                other => {
                    return Err(Error::Parse(format!(
                        "expected ',' or ')', found {}",
                        describe(other)
                    )))
                }
            }
        }

        self.eat_semicolon_and_ensure_end()?;
        Ok(Statement::CreateTable { table, columns })
    }

    fn parse_insert(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Insert)?;
        self.expect_keyword(Keyword::Into)?;
        let table = self.expect_ident()?;

        let columns = if let Some(Token::LParen) = self.peek() {
            self.pos += 1;
            let mut cols = Vec::new();
            loop {
                cols.push(self.expect_ident()?);
                match self.advance() {
                    Some(Token::Comma) => continue,
                    Some(Token::RParen) => break,
                    other => {
                        return Err(Error::Parse(format!(
                            "expected ',' or ')', found {}",
                            describe(other)
                        )))
                    }
                }
            }
            Some(cols)
        } else {
            None
        };

        self.expect_keyword(Keyword::Values)?;

        let mut rows = Vec::new();
        loop {
            self.expect_punct(&Token::LParen)?;
            let mut values = Vec::new();
            loop {
                values.push(self.parse_expr()?);
                match self.advance() {
                    Some(Token::Comma) => continue,
                    Some(Token::RParen) => break,
                    other => {
                        return Err(Error::Parse(format!(
                            "expected ',' or ')', found {}",
                            describe(other)
                        )))
                    }
                }
            }
            rows.push(values);

            match self.peek() {
                Some(Token::Comma) => self.pos += 1,
                _ => break,
            }
        }

        self.eat_semicolon_and_ensure_end()?;
        Ok(Statement::Insert {
            table,
            columns,
            rows,
        })
    }

    fn parse_select(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Select)?;

        let mut projection = Vec::new();
        loop {
            if let Some(Token::Star) = self.peek() {
                self.pos += 1;
                projection.push(SelectItem::Wildcard);
            } else {
                projection.push(SelectItem::Expr(self.parse_expr()?));
            }
            match self.peek() {
                Some(Token::Comma) => self.pos += 1,
                _ => break,
            }
        }

        let (from, selection) = if let Some(Token::Keyword(Keyword::From)) = self.peek() {
            self.pos += 1;
            let table = self.expect_ident()?;
            let selection = if let Some(Token::Keyword(Keyword::Where)) = self.peek() {
                self.pos += 1;
                Some(self.parse_condition()?)
            } else {
                None
            };
            (Some(table), selection)
        } else {
            (None, None)
        };

        self.eat_semicolon_and_ensure_end()?;
        Ok(Statement::Select {
            projection,
            from,
            selection,
        })
    }

    fn parse_condition(&mut self) -> Result<Condition> {
        let column = self.expect_ident()?;
        let op = match self.advance() {
            Some(Token::Eq) => CompareOp::Eq,
            Some(Token::NotEq) => CompareOp::NotEq,
            Some(Token::Lt) => CompareOp::Lt,
            Some(Token::Gt) => CompareOp::Gt,
            Some(Token::Le) => CompareOp::Le,
            Some(Token::Ge) => CompareOp::Ge,
            other => {
                return Err(Error::Parse(format!(
                    "expected a comparison operator, found {}",
                    describe(other)
                )))
            }
        };
        let value = self.parse_expr()?;
        Ok(Condition { column, op, value })
    }

    fn parse_expr(&mut self) -> Result<Expr> {
        let allow_placeholders = self.allow_placeholders;
        match self.advance() {
            Some(Token::Integer(n)) => Ok(Expr::Integer(*n)),
            Some(Token::Str(s)) => Ok(Expr::String(s.clone())),
            Some(Token::Keyword(Keyword::Null)) => Ok(Expr::Null),
            Some(Token::SystemVariable(name)) => Ok(Expr::SystemVariable(name.clone())),
            Some(Token::Ident(name)) => Ok(Expr::Column(name.clone())),
            Some(Token::Placeholder) if allow_placeholders => {
                let index = self.param_count;
                self.param_count += 1;
                Ok(Expr::Placeholder(index))
            }
            Some(Token::Placeholder) => Err(Error::Parse(
                "'?' placeholders are only allowed in prepared statements".to_string(),
            )),
            other => Err(Error::Parse(format!(
                "expected a value or column, found {}",
                describe(other)
            ))),
        }
    }
}

/// Parse a raw SQL string into a [`Statement`]. `?` placeholders are
/// rejected — use [`parse_prepared`] for prepared-statement text.
pub fn parse(sql: &str) -> Result<Statement> {
    let tokens = tokenize(sql)?;
    if tokens.is_empty() {
        return Err(Error::Parse("empty statement".to_string()));
    }
    Parser::new(&tokens, false).parse_statement()
}

/// Parse one or more `;`-separated statements from a single query string
/// (the `CLIENT_MULTI_STATEMENTS` path). Empty segments — a trailing `;`, or
/// `;;` — are skipped. Errors if the whole string is empty/blank or if any
/// segment is not a complete valid statement. `?` placeholders are rejected,
/// same as [`parse`].
pub fn parse_many(sql: &str) -> Result<Vec<Statement>> {
    let tokens = tokenize(sql)?;
    let mut statements = Vec::new();
    // Splitting the *token* stream (not the raw string) on `;` respects
    // semicolons inside string literals, which the tokenizer already handled.
    // Each segment has no trailing `;`, so the per-statement end-of-input
    // check inside `parse_statement` passes cleanly.
    for segment in tokens.split(|t| *t == Token::Semicolon) {
        if segment.is_empty() {
            continue;
        }
        statements.push(Parser::new(segment, false).parse_statement()?);
    }
    if statements.is_empty() {
        return Err(Error::Parse("empty statement".to_string()));
    }
    Ok(statements)
}

/// Parse prepared-statement text, allowing `?` placeholders. Returns the
/// statement (with [`Expr::Placeholder`] holes) and the number of
/// placeholders, which is the parameter count the client must supply on
/// `COM_STMT_EXECUTE`.
pub fn parse_prepared(sql: &str) -> Result<(Statement, usize)> {
    let tokens = tokenize(sql)?;
    if tokens.is_empty() {
        return Err(Error::Parse("empty statement".to_string()));
    }
    let mut parser = Parser::new(&tokens, true);
    let statement = parser.parse_statement()?;
    Ok((statement, parser.param_count))
}

/// Replace every [`Expr::Placeholder`] in `statement` with the corresponding
/// literal from `params` (a `?` at positional index `i` is filled by
/// `params[i]`). Errors if a placeholder index is out of range (i.e. the
/// client sent fewer parameters than the statement has placeholders).
pub fn bind_parameters(statement: Statement, params: &[Expr]) -> Result<Statement> {
    let bound = match statement {
        Statement::Insert {
            table,
            columns,
            rows,
        } => Statement::Insert {
            table,
            columns,
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().map(|e| bind_expr(e, params)).collect())
                .collect::<Result<Vec<Vec<Expr>>>>()?,
        },
        Statement::Select {
            projection,
            from,
            selection,
        } => Statement::Select {
            projection: projection
                .into_iter()
                .map(|item| match item {
                    SelectItem::Wildcard => Ok(SelectItem::Wildcard),
                    SelectItem::Expr(e) => bind_expr(e, params).map(SelectItem::Expr),
                })
                .collect::<Result<Vec<SelectItem>>>()?,
            from,
            selection: match selection {
                Some(cond) => Some(Condition {
                    value: bind_expr(cond.value, params)?,
                    ..cond
                }),
                None => None,
            },
        },
        // CREATE TABLE and transaction-control statements carry no exprs.
        other => other,
    };
    Ok(bound)
}

fn bind_expr(expr: Expr, params: &[Expr]) -> Result<Expr> {
    match expr {
        Expr::Placeholder(i) => params
            .get(i)
            .cloned()
            .ok_or_else(|| Error::Execution(format!("missing value for parameter {}", i + 1))),
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- tokenizer / low-level parsing behavior, exercised via parse() ----

    #[test]
    fn keywords_are_case_insensitive() {
        assert!(parse("select 1").is_ok());
        assert!(parse("SeLeCt 1").is_ok());
    }

    #[test]
    fn rejects_empty_statement() {
        assert!(matches!(parse(""), Err(Error::Parse(_))));
        assert!(matches!(parse("   "), Err(Error::Parse(_))));
    }

    #[test]
    fn rejects_unterminated_string() {
        assert!(matches!(parse("SELECT 'abc"), Err(Error::Parse(_))));
    }

    #[test]
    fn string_literal_handles_escaped_quote() {
        let stmt = parse("SELECT 'it''s'").unwrap();
        match stmt {
            Statement::Select { projection, .. } => {
                assert_eq!(
                    projection,
                    vec![SelectItem::Expr(Expr::String("it's".to_string()))]
                );
            }
            other => panic!("expected Select, got {other:?}"),
        }
    }

    #[test]
    fn negative_integer_literal() {
        let stmt = parse("SELECT -5").unwrap();
        match stmt {
            Statement::Select { projection, .. } => {
                assert_eq!(projection, vec![SelectItem::Expr(Expr::Integer(-5))]);
            }
            other => panic!("expected Select, got {other:?}"),
        }
    }

    #[test]
    fn null_literal() {
        let stmt = parse("SELECT NULL").unwrap();
        match stmt {
            Statement::Select { projection, .. } => {
                assert_eq!(projection, vec![SelectItem::Expr(Expr::Null)]);
            }
            other => panic!("expected Select, got {other:?}"),
        }
        assert_eq!(Expr::Null.to_value_string(), None);
    }

    #[test]
    fn null_is_case_insensitive() {
        assert!(parse("SELECT null").is_ok());
        assert!(parse("INSERT INTO t VALUES (Null)").is_ok());
    }

    #[test]
    fn rejects_unexpected_character() {
        assert!(matches!(parse("SELECT 1 # comment"), Err(Error::Parse(_))));
    }

    // ---- CREATE TABLE ----

    #[test]
    fn create_table_basic() {
        let stmt = parse("CREATE TABLE users (id INT, name VARCHAR)").unwrap();
        assert_eq!(
            stmt,
            Statement::CreateTable {
                table: "users".to_string(),
                columns: vec![
                    ColumnDef {
                        name: "id".to_string(),
                        type_name: "INT".to_string(),
                        is_primary_key: false,
                    },
                    ColumnDef {
                        name: "name".to_string(),
                        type_name: "VARCHAR".to_string(),
                        is_primary_key: false,
                    },
                ],
            }
        );
    }

    #[test]
    fn create_table_with_primary_key() {
        let stmt = parse("CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR)").unwrap();
        match stmt {
            Statement::CreateTable { columns, .. } => {
                assert!(columns[0].is_primary_key);
                assert!(!columns[1].is_primary_key);
            }
            other => panic!("expected CreateTable, got {other:?}"),
        }
    }

    #[test]
    fn create_table_rejects_primary_without_key() {
        assert!(matches!(
            parse("CREATE TABLE t (id INT PRIMARY, name VARCHAR)"),
            Err(Error::Parse(_))
        ));
    }

    #[test]
    fn create_table_with_type_size_is_accepted() {
        assert!(parse("CREATE TABLE t (name VARCHAR(255))").is_ok());
    }

    #[test]
    fn create_table_requires_at_least_one_column() {
        assert!(matches!(parse("CREATE TABLE t ()"), Err(Error::Parse(_))));
    }

    #[test]
    fn create_table_rejects_missing_closing_paren() {
        assert!(matches!(
            parse("CREATE TABLE t (id INT"),
            Err(Error::Parse(_))
        ));
    }

    // ---- INSERT ----

    #[test]
    fn insert_with_explicit_columns_multi_row() {
        let stmt = parse("INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y')").unwrap();
        assert_eq!(
            stmt,
            Statement::Insert {
                table: "t".to_string(),
                columns: Some(vec!["a".to_string(), "b".to_string()]),
                rows: vec![
                    vec![Expr::Integer(1), Expr::String("x".to_string())],
                    vec![Expr::Integer(2), Expr::String("y".to_string())],
                ],
            }
        );
    }

    #[test]
    fn insert_without_explicit_columns() {
        let stmt = parse("INSERT INTO t VALUES (1, 'x')").unwrap();
        assert_eq!(
            stmt,
            Statement::Insert {
                table: "t".to_string(),
                columns: None,
                rows: vec![vec![Expr::Integer(1), Expr::String("x".to_string())]],
            }
        );
    }

    #[test]
    fn insert_rejects_missing_values_keyword() {
        // Valid column list, but the VALUES keyword itself is missing.
        assert!(matches!(
            parse("INSERT INTO t (a, b) (1, 2)"),
            Err(Error::Parse(_))
        ));
    }

    #[test]
    fn insert_rejects_non_identifier_in_column_list() {
        assert!(matches!(
            parse("INSERT INTO t (1, 2) VALUES (1, 2)"),
            Err(Error::Parse(_))
        ));
    }

    // ---- transaction control ----

    #[test]
    fn begin_and_start_transaction_are_synonyms() {
        assert_eq!(parse("BEGIN").unwrap(), Statement::Begin);
        assert_eq!(parse("begin").unwrap(), Statement::Begin);
        assert_eq!(parse("START TRANSACTION").unwrap(), Statement::Begin);
        assert_eq!(parse("start transaction").unwrap(), Statement::Begin);
    }

    #[test]
    fn commit_and_rollback() {
        assert_eq!(parse("COMMIT").unwrap(), Statement::Commit);
        assert_eq!(parse("COMMIT;").unwrap(), Statement::Commit);
        assert_eq!(parse("ROLLBACK").unwrap(), Statement::Rollback);
    }

    #[test]
    fn start_without_transaction_keyword_is_a_parse_error() {
        assert!(matches!(parse("START"), Err(Error::Parse(_))));
        assert!(matches!(parse("START FOO"), Err(Error::Parse(_))));
    }

    #[test]
    fn transaction_control_rejects_trailing_garbage() {
        assert!(matches!(parse("BEGIN FOO"), Err(Error::Parse(_))));
        assert!(matches!(parse("COMMIT NOW"), Err(Error::Parse(_))));
    }

    // ---- prepared-statement placeholders ----

    #[test]
    fn plain_parse_rejects_placeholders() {
        assert!(matches!(
            parse("SELECT * FROM t WHERE id = ?"),
            Err(Error::Parse(_))
        ));
        assert!(matches!(
            parse("INSERT INTO t VALUES (?)"),
            Err(Error::Parse(_))
        ));
    }

    #[test]
    fn parse_prepared_counts_and_indexes_placeholders() {
        let (stmt, count) = parse_prepared("INSERT INTO t (a, b) VALUES (?, ?)").unwrap();
        assert_eq!(count, 2);
        match stmt {
            Statement::Insert { rows, .. } => {
                assert_eq!(rows[0], vec![Expr::Placeholder(0), Expr::Placeholder(1)]);
            }
            other => panic!("expected Insert, got {other:?}"),
        }
    }

    #[test]
    fn parse_prepared_placeholder_in_where() {
        let (stmt, count) = parse_prepared("SELECT * FROM t WHERE id = ?").unwrap();
        assert_eq!(count, 1);
        match stmt {
            Statement::Select {
                selection: Some(cond),
                ..
            } => {
                assert_eq!(cond.value, Expr::Placeholder(0));
            }
            other => panic!("expected Select with WHERE, got {other:?}"),
        }
    }

    #[test]
    fn parse_prepared_with_no_placeholders_reports_zero() {
        let (_stmt, count) = parse_prepared("SELECT 1").unwrap();
        assert_eq!(count, 0);
    }

    // ---- multi-statement parsing ----

    #[test]
    fn parse_many_single_statement() {
        let stmts = parse_many("SELECT 1").unwrap();
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn parse_many_splits_on_semicolons() {
        let stmts = parse_many("SELECT 1; SELECT 2; SELECT 3").unwrap();
        assert_eq!(stmts.len(), 3);
    }

    #[test]
    fn parse_many_ignores_trailing_and_empty_segments() {
        assert_eq!(parse_many("SELECT 1;").unwrap().len(), 1);
        assert_eq!(parse_many("SELECT 1;;SELECT 2;").unwrap().len(), 2);
    }

    #[test]
    fn parse_many_does_not_split_semicolons_inside_string_literals() {
        // The ';' is inside a string literal, so this is one statement.
        let stmts = parse_many("INSERT INTO t VALUES ('a;b')").unwrap();
        assert_eq!(stmts.len(), 1);
        match &stmts[0] {
            Statement::Insert { rows, .. } => {
                assert_eq!(rows[0], vec![Expr::String("a;b".to_string())]);
            }
            other => panic!("expected Insert, got {other:?}"),
        }
    }

    #[test]
    fn parse_many_rejects_a_malformed_segment() {
        assert!(matches!(
            parse_many("SELECT 1; GARBAGE HERE"),
            Err(Error::Parse(_))
        ));
    }

    #[test]
    fn parse_many_rejects_all_empty() {
        assert!(matches!(parse_many(";;"), Err(Error::Parse(_))));
        assert!(matches!(parse_many("  "), Err(Error::Parse(_))));
    }

    #[test]
    fn bind_parameters_fills_placeholders_in_order() {
        let (stmt, _) = parse_prepared("INSERT INTO t (a, b) VALUES (?, ?)").unwrap();
        let bound =
            bind_parameters(stmt, &[Expr::Integer(1), Expr::String("x".to_string())]).unwrap();
        match bound {
            Statement::Insert { rows, .. } => {
                assert_eq!(
                    rows[0],
                    vec![Expr::Integer(1), Expr::String("x".to_string())]
                );
            }
            other => panic!("expected Insert, got {other:?}"),
        }
    }

    #[test]
    fn bind_parameters_errors_when_a_value_is_missing() {
        let (stmt, _) = parse_prepared("SELECT * FROM t WHERE id = ?").unwrap();
        assert!(matches!(
            bind_parameters(stmt, &[]),
            Err(Error::Execution(_))
        ));
    }

    #[test]
    fn bind_parameters_leaves_non_placeholder_statements_unchanged() {
        let (stmt, _) = parse_prepared("SELECT 1").unwrap();
        let bound = bind_parameters(stmt.clone(), &[]).unwrap();
        assert_eq!(bound, stmt);
    }

    // ---- SELECT ----

    #[test]
    fn select_wildcard_from_table() {
        let stmt = parse("SELECT * FROM t").unwrap();
        assert_eq!(
            stmt,
            Statement::Select {
                projection: vec![SelectItem::Wildcard],
                from: Some("t".to_string()),
                selection: None,
            }
        );
    }

    #[test]
    fn select_columns_with_where_equality() {
        let stmt = parse("SELECT a, b FROM t WHERE a = 1").unwrap();
        assert_eq!(
            stmt,
            Statement::Select {
                projection: vec![
                    SelectItem::Expr(Expr::Column("a".to_string())),
                    SelectItem::Expr(Expr::Column("b".to_string())),
                ],
                from: Some("t".to_string()),
                selection: Some(Condition {
                    column: "a".to_string(),
                    op: CompareOp::Eq,
                    value: Expr::Integer(1),
                }),
            }
        );
    }

    #[test]
    fn select_where_supports_all_comparison_operators() {
        for (text, op) in [
            ("=", CompareOp::Eq),
            ("<>", CompareOp::NotEq),
            ("!=", CompareOp::NotEq),
            ("<", CompareOp::Lt),
            (">", CompareOp::Gt),
            ("<=", CompareOp::Le),
            (">=", CompareOp::Ge),
        ] {
            let sql = format!("SELECT * FROM t WHERE a {text} 1");
            let stmt = parse(&sql).unwrap_or_else(|e| panic!("parse({sql:?}) failed: {e}"));
            match stmt {
                Statement::Select {
                    selection: Some(cond),
                    ..
                } => assert_eq!(cond.op, op),
                other => panic!("expected a Select with a WHERE, got {other:?}"),
            }
        }
    }

    #[test]
    fn select_literal_without_from() {
        let stmt = parse("SELECT 1").unwrap();
        assert_eq!(
            stmt,
            Statement::Select {
                projection: vec![SelectItem::Expr(Expr::Integer(1))],
                from: None,
                selection: None,
            }
        );
    }

    #[test]
    fn select_system_variable() {
        let stmt = parse("SELECT @@version").unwrap();
        assert_eq!(
            stmt,
            Statement::Select {
                projection: vec![SelectItem::Expr(Expr::SystemVariable(
                    "version".to_string()
                ))],
                from: None,
                selection: None,
            }
        );
    }

    #[test]
    fn select_trailing_semicolon_is_optional() {
        assert!(parse("SELECT 1;").is_ok());
        assert!(parse("SELECT 1").is_ok());
    }

    #[test]
    fn select_rejects_trailing_garbage() {
        assert!(matches!(parse("SELECT 1 GARBAGE"), Err(Error::Parse(_))));
    }

    #[test]
    fn select_rejects_where_without_from() {
        // No FROM means no columns exist to filter on; this should be a
        // syntax error the same way real MySQL rejects it.
        assert!(matches!(
            parse("SELECT 1 WHERE a = 1"),
            Err(Error::Parse(_))
        ));
    }

    #[test]
    fn parse_error_messages_name_the_offending_token() {
        let err = parse("DELETE FROM t").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("DELETE"), "message was: {message}");
    }
}
