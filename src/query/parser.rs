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
    /// `SET ...` — accepted and ignored. Session settings (`SET NAMES`,
    /// `SET autocommit`, `SET sql_mode`, ...) are common client boilerplate;
    /// this server doesn't model them, so it acknowledges them with an OK.
    Set,
    /// `USE <db>` — accepted as a no-op; this server is schemaless.
    Use,
    /// `SHOW ...` — limited introspection (see [`ShowStatement`]).
    Show(ShowStatement),
    /// `CREATE DATABASE [IF NOT EXISTS] <name> [...]` — any `CHARACTER SET`/
    /// `COLLATE` clause is parsed and discarded (this server has exactly one
    /// charset/collation). Database names are a lightweight, in-memory-only
    /// namespace registry; table storage itself remains flat/global, unchanged
    /// from before this existed (see `storage::Storage`).
    CreateDatabase {
        name: String,
        if_not_exists: bool,
    },
    /// `DROP DATABASE [IF EXISTS] <name>`.
    DropDatabase {
        name: String,
        if_exists: bool,
    },
}

/// The recognized forms of `SHOW`. Anything not modelled here parses to
/// [`ShowStatement::Other`] and yields an empty result rather than an error,
/// so client/GUI introspection queries don't break the session.
#[derive(Debug, Clone, PartialEq)]
pub enum ShowStatement {
    Databases,
    Tables,
    Warnings,
    Variables {
        like: Option<String>,
    },
    /// `SHOW CHARACTER SET` / `SHOW CHARSET`.
    CharacterSet,
    /// `SHOW COLLATION`.
    Collation,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    /// Recorded as-written; validated against `storage::ColumnType` by the
    /// executor at `CREATE TABLE` time.
    pub type_name: String,
    /// Whether this column was declared `PRIMARY KEY` (inline, or via a
    /// table-level `PRIMARY KEY (...)` constraint naming it).
    pub is_primary_key: bool,
    /// Whether `NULL` is a legal value (default `true`; `false` after
    /// explicit `NOT NULL`). A primary-key column is always non-nullable
    /// regardless of this flag (see `Executor::execute_create_table`).
    pub nullable: bool,
    /// Whether this column is `AUTO_INCREMENT`. Only meaningful (and only
    /// accepted) on the primary-key column — see `Executor::execute_create_table`.
    pub auto_increment: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Wildcard,
    /// A projected expression with an optional `AS` alias (the column label the
    /// client sees). Clients like JDBC read connect-time variables by their
    /// alias, so the alias must be honored.
    Expr(Expr, Option<String>),
}

/// A literal or column reference.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Integer(i64),
    String(String),
    Null,
    SystemVariable(String),
    Column(String),
    /// A function call such as `DATABASE()` or `VERSION()`. Arguments are
    /// captured but only a small set of no-argument informational functions is
    /// evaluated (see the executor); unknown functions yield `NULL`.
    Function(String, Vec<Expr>),
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
            Expr::Function(name, _) => Some(format!("{name}()")),
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
    /// `.` — separates a schema/table qualifier from what follows (e.g.
    /// `information_schema.TABLES`, `mydb.mytable`).
    Dot,
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
    Set,
    Use,
    Show,
    As,
    Drop,
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
        "SET" => Some(Keyword::Set),
        "USE" => Some(Keyword::Use),
        "SHOW" => Some(Keyword::Show),
        "AS" => Some(Keyword::As),
        "DROP" => Some(Keyword::Drop),
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
        Token::Dot => ".".to_string(),
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

        // --- Comments ---------------------------------------------------
        // Block comments `/* ... */`. MySQL executable comments `/*! ... */`
        // (optionally version-gated, e.g. `/*!40101 ... */`) have their inner
        // SQL unwrapped and tokenized; plain block comments are skipped. This
        // is what lets a JDBC driver's `/* driver-name */SELECT ...` init
        // queries parse.
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            let executable = chars.get(i + 2) == Some(&'!');
            let mut j = i + 2;
            let mut closed = false;
            while j + 1 < chars.len() {
                if chars[j] == '*' && chars[j + 1] == '/' {
                    closed = true;
                    break;
                }
                j += 1;
            }
            if !closed {
                return Err(Error::Parse(format!(
                    "unterminated block comment starting at position {i}"
                )));
            }
            if executable {
                // Skip `/*!` and any immediately-following version digits.
                let mut inner_start = i + 3;
                while inner_start < j && chars[inner_start].is_ascii_digit() {
                    inner_start += 1;
                }
                let inner: String = chars[inner_start..j].iter().collect();
                tokens.append(&mut tokenize(&inner)?);
            }
            i = j + 2; // past the closing `*/`
            continue;
        }
        // Line comments: `# ...` and `-- ...` (the `--` form requires the
        // dashes be followed by whitespace or end-of-input, per MySQL, so
        // negative numbers like `--5` are unaffected).
        if c == '#' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '-'
            && chars.get(i + 1) == Some(&'-')
            && chars.get(i + 2).is_none_or(|c| c.is_whitespace())
        {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
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
            '.' => {
                tokens.push(Token::Dot);
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
            '`' => {
                // Backtick-quoted identifier, e.g. `my table`. Standard MySQL
                // quoting used pervasively by GUI clients and drivers.
                let (name, consumed) = scan_backtick_ident(&chars[i..])?;
                tokens.push(Token::Ident(name));
                i += consumed;
            }
            '@' => {
                if chars.get(i + 1) == Some(&'@') {
                    // System variable, optionally scope-qualified:
                    // `@@x`, `@@session.x`, `@@global.x`.
                    let (mut name, mut consumed) = scan_ident(&chars[i + 2..]);
                    if name.is_empty() {
                        return Err(Error::Parse(format!(
                            "expected an identifier after '@@' at position {i}"
                        )));
                    }
                    if chars.get(i + 2 + consumed) == Some(&'.') {
                        let (second, c2) = scan_ident(&chars[i + 2 + consumed + 1..]);
                        if !second.is_empty() {
                            name = format!("{name}.{second}");
                            consumed += 1 + c2;
                        }
                    }
                    tokens.push(Token::SystemVariable(name));
                    i += 2 + consumed;
                } else {
                    // User-defined variable `@name`. We don't model these, but
                    // lex them (as an Ident carrying the `@`) so statements like
                    // `SET @x = 1` don't fail — `SET` is accepted and ignored.
                    let (name, consumed) = scan_ident(&chars[i + 1..]);
                    tokens.push(Token::Ident(format!("@{name}")));
                    i += 1 + consumed;
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

/// `chars[0]` must be the opening backtick. A doubled backtick (```` `` ````)
/// inside is an escaped literal backtick, matching MySQL.
fn scan_backtick_ident(chars: &[char]) -> Result<(String, usize)> {
    let mut n = 1;
    let mut s = String::new();
    loop {
        match chars.get(n) {
            None => return Err(Error::Parse("unterminated `identifier`".to_string())),
            Some('`') => {
                if chars.get(n + 1) == Some(&'`') {
                    s.push('`');
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
    if s.is_empty() {
        return Err(Error::Parse("empty `` identifier".to_string()));
    }
    Ok((s, n))
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

/// Whether `token` is the (non-reserved) identifier `DATABASE` or its
/// MySQL-standard synonym `SCHEMA`, case-insensitively — used to disambiguate
/// `CREATE TABLE` from `CREATE DATABASE`/`CREATE SCHEMA` with one token of
/// lookahead. (JDBC drivers — and so DBeaver — emit `CREATE SCHEMA`.)
fn is_database_ident(token: &Token) -> bool {
    matches!(token, Token::Ident(s) if s.eq_ignore_ascii_case("DATABASE") || s.eq_ignore_ascii_case("SCHEMA"))
}

/// Apply a table-level `PRIMARY KEY (col_list)` constraint onto the matching
/// column in `columns`. This engine's storage supports only a single-column
/// primary key (see `storage::Table`), so a multi-column list is rejected
/// with a clear error rather than silently keeping just one column.
fn apply_primary_key_constraint(columns: &mut [ColumnDef], pk_columns: &[String]) -> Result<()> {
    let [name] = pk_columns else {
        return Err(Error::Unsupported("composite primary keys"));
    };
    let col = columns
        .iter_mut()
        .find(|c| &c.name == name)
        .ok_or_else(|| Error::Parse(format!("PRIMARY KEY references unknown column '{name}'")))?;
    col.is_primary_key = true;
    Ok(())
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

    /// Peek `offset` tokens ahead of the current position (0 = same as `peek`).
    fn peek_at(&self, offset: usize) -> Option<&Token> {
        self.tokens.get(self.pos + offset)
    }

    /// Whether the next token is a plain identifier equal to `word`,
    /// case-insensitively. Used for the small set of clause words (`DATABASE`,
    /// `IF`, `NOT`, `EXISTS`, ...) that aren't reserved keywords — so they
    /// remain valid as ordinary column/table names elsewhere.
    fn peek_is_ident_ci(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Ident(s)) if s.eq_ignore_ascii_case(word))
    }

    fn expect_ident_ci(&mut self, word: &str) -> Result<()> {
        match self.advance() {
            Some(Token::Ident(s)) if s.eq_ignore_ascii_case(word) => Ok(()),
            other => Err(Error::Parse(format!(
                "expected '{word}', found {}",
                describe(other)
            ))),
        }
    }

    /// Consume `DATABASE` or its synonym `SCHEMA` (see [`is_database_ident`]).
    fn expect_database_or_schema(&mut self) -> Result<()> {
        match self.advance() {
            Some(t) if is_database_ident(t) => Ok(()),
            other => Err(Error::Parse(format!(
                "expected 'DATABASE' or 'SCHEMA', found {}",
                describe(other)
            ))),
        }
    }

    /// A parenthesized, comma-separated column-name list, as used by table
    /// constraints: `(col1, col2, ...)`. A column may carry an index
    /// prefix-length, e.g. `KEY (name(20))` — consumed, not stored (this
    /// server has no concept of prefix indexes).
    fn parse_parenthesized_column_list(&mut self) -> Result<Vec<String>> {
        self.expect_punct(&Token::LParen)?;
        let mut cols = Vec::new();
        loop {
            cols.push(self.expect_ident()?);
            if let Some(Token::LParen) = self.peek() {
                self.pos += 1;
                match self.advance() {
                    Some(Token::Integer(_)) => {}
                    other => {
                        return Err(Error::Parse(format!(
                            "expected an integer prefix length, found {}",
                            describe(other)
                        )))
                    }
                }
                self.expect_punct(&Token::RParen)?;
            }
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
        Ok(cols)
    }

    /// An optional bare identifier naming an index/constraint — consumed and
    /// discarded (we don't track names beyond what's needed to parse past them).
    fn eat_optional_name(&mut self) {
        if matches!(self.peek(), Some(Token::Ident(_))) {
            self.pos += 1;
        }
    }

    /// Whether the parser is positioned at a table-level constraint —
    /// `[CONSTRAINT name] {PRIMARY KEY | UNIQUE [KEY|INDEX] | KEY | INDEX |
    /// FOREIGN KEY} ...` — rather than the start of another column
    /// definition, inside a `CREATE TABLE`'s column list.
    fn peek_is_table_constraint_start(&self) -> bool {
        matches!(
            self.peek(),
            Some(Token::Keyword(Keyword::Primary)) | Some(Token::Keyword(Keyword::Key))
        ) || self.peek_is_ident_ci("CONSTRAINT")
            || self.peek_is_ident_ci("UNIQUE")
            || self.peek_is_ident_ci("INDEX")
            || self.peek_is_ident_ci("FOREIGN")
    }

    /// Parse one table-level constraint clause and apply its effect (if any)
    /// onto `columns`. Only `PRIMARY KEY (...)` has an effect (marking the
    /// named column primary); `UNIQUE`/plain `KEY`/`INDEX`/`FOREIGN KEY` are
    /// parsed fully (so they don't break parsing) but not enforced — this
    /// engine has one index (the primary key) and no referential-integrity
    /// checking.
    fn parse_table_constraint(&mut self, columns: &mut [ColumnDef]) -> Result<()> {
        if self.peek_is_ident_ci("CONSTRAINT") {
            self.pos += 1;
            self.eat_optional_name(); // the constraint's own name
        }

        if matches!(self.peek(), Some(Token::Keyword(Keyword::Primary))) {
            self.pos += 1;
            self.expect_keyword(Keyword::Key)?;
            let pk_columns = self.parse_parenthesized_column_list()?;
            apply_primary_key_constraint(columns, &pk_columns)?;
        } else if self.peek_is_ident_ci("UNIQUE") {
            self.pos += 1;
            if matches!(self.peek(), Some(Token::Keyword(Keyword::Key)))
                || self.peek_is_ident_ci("INDEX")
            {
                self.pos += 1;
            }
            self.eat_optional_name();
            self.parse_parenthesized_column_list()?;
        } else if matches!(self.peek(), Some(Token::Keyword(Keyword::Key)))
            || self.peek_is_ident_ci("INDEX")
        {
            self.pos += 1;
            self.eat_optional_name();
            self.parse_parenthesized_column_list()?;
        } else if self.peek_is_ident_ci("FOREIGN") {
            self.pos += 1;
            self.expect_keyword(Keyword::Key)?;
            self.eat_optional_name();
            self.parse_parenthesized_column_list()?; // local columns
            self.expect_ident_ci("REFERENCES")?;
            self.expect_qualified_ident()?; // referenced table
            self.parse_parenthesized_column_list()?; // referenced columns
                                                     // Optional trailing `ON DELETE ...` / `ON UPDATE ...` clauses —
                                                     // consume up to the next top-level `,`/`)`.
            while !matches!(self.peek(), Some(Token::Comma) | Some(Token::RParen) | None) {
                self.pos += 1;
            }
        } else {
            return Err(Error::Parse(format!(
                "expected a table constraint (PRIMARY KEY/UNIQUE/KEY/FOREIGN KEY), found {}",
                describe(self.peek())
            )));
        }
        Ok(())
    }

    /// One column definition inside `CREATE TABLE (...)`: name, type
    /// (with optional size/precision), then any number of attributes in any
    /// order (`NOT NULL`/`NULL`, `AUTO_INCREMENT`, `PRIMARY KEY`, `UNIQUE
    /// [KEY]`, `DEFAULT <expr>`, `COMMENT '...'`) — matching real DDL, which
    /// doesn't fix an attribute order.
    fn parse_column_def(&mut self) -> Result<ColumnDef> {
        let name = self.expect_ident()?;
        let type_name = self.expect_ident()?;

        // Optional size/precision, e.g. VARCHAR(255) or DECIMAL(10,2);
        // consumed, not stored (this server's types have no size parameter).
        if let Some(Token::LParen) = self.peek() {
            self.pos += 1;
            loop {
                match self.advance() {
                    Some(Token::Integer(_)) => {}
                    other => {
                        return Err(Error::Parse(format!(
                            "expected an integer in type size, found {}",
                            describe(other)
                        )))
                    }
                }
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
        }
        // UNSIGNED/ZEROFILL immediately follow a numeric type, before any
        // other attribute; consumed, not enforced (one INT range here).
        while self.peek_is_ident_ci("UNSIGNED") || self.peek_is_ident_ci("ZEROFILL") {
            self.pos += 1;
        }

        let mut is_primary_key = false;
        let mut nullable = true;
        let mut auto_increment = false;
        loop {
            if matches!(self.peek(), Some(Token::Keyword(Keyword::Null))) {
                self.pos += 1;
                nullable = true;
            } else if self.peek_is_ident_ci("NOT")
                && matches!(self.peek_at(1), Some(Token::Keyword(Keyword::Null)))
            {
                self.pos += 2;
                nullable = false;
            } else if self.peek_is_ident_ci("AUTO_INCREMENT") {
                self.pos += 1;
                auto_increment = true;
            } else if matches!(self.peek(), Some(Token::Keyword(Keyword::Primary))) {
                self.pos += 1;
                self.expect_keyword(Keyword::Key)?;
                is_primary_key = true;
            } else if self.peek_is_ident_ci("UNIQUE") {
                self.pos += 1;
                if matches!(self.peek(), Some(Token::Keyword(Keyword::Key))) {
                    self.pos += 1;
                }
                // Not enforced beyond the primary key; parsed so it doesn't
                // break the rest of the column definition.
            } else if self.peek_is_ident_ci("DEFAULT") {
                self.pos += 1;
                self.parse_expr()?; // not modelled: no default-value substitution
            } else if self.peek_is_ident_ci("COMMENT") {
                self.pos += 1;
                match self.advance() {
                    Some(Token::Str(_)) => {}
                    other => {
                        return Err(Error::Parse(format!(
                            "expected a string after COMMENT, found {}",
                            describe(other)
                        )))
                    }
                }
            } else {
                break;
            }
        }

        Ok(ColumnDef {
            name,
            type_name,
            is_primary_key,
            nullable,
            auto_increment,
        })
    }

    /// Consume an optional `IF NOT EXISTS`, reporting whether it was present.
    fn eat_if_not_exists(&mut self) -> Result<bool> {
        if !self.peek_is_ident_ci("IF") {
            return Ok(false);
        }
        self.pos += 1;
        self.expect_ident_ci("NOT")?;
        self.expect_ident_ci("EXISTS")?;
        Ok(true)
    }

    /// Consume an optional `IF EXISTS`, reporting whether it was present.
    fn eat_if_exists(&mut self) -> Result<bool> {
        if !self.peek_is_ident_ci("IF") {
            return Ok(false);
        }
        self.pos += 1;
        self.expect_ident_ci("EXISTS")?;
        Ok(true)
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

    /// An identifier optionally schema-qualified with one or more `.`
    /// segments (`db.table`, or even `catalog.db.table`, as e.g.
    /// `information_schema.TABLES` or a JDBC-generated `mydb.NewTable`).
    /// Returns only the final segment — the object name — discarding any
    /// qualifier: this server is schemaless (`USE`/database names are a
    /// compatibility no-op; see `Statement::Use`), so there is nothing
    /// meaningful to route the qualifier to.
    fn expect_qualified_ident(&mut self) -> Result<String> {
        let mut name = self.expect_ident()?;
        while matches!(self.peek(), Some(Token::Dot)) {
            self.pos += 1;
            name = self.expect_ident()?;
        }
        Ok(name)
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
            Some(Token::Keyword(Keyword::Create)) => {
                if self.peek_at(1).is_some_and(is_database_ident) {
                    self.parse_create_database()
                } else {
                    self.parse_create_table()
                }
            }
            Some(Token::Keyword(Keyword::Drop)) => self.parse_drop_database(),
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
            Some(Token::Keyword(Keyword::Set)) => self.parse_set(),
            Some(Token::Keyword(Keyword::Use)) => self.parse_use(),
            Some(Token::Keyword(Keyword::Show)) => self.parse_show(),
            other => Err(Error::Parse(format!(
                "expected CREATE, DROP, INSERT, SELECT, BEGIN, START, COMMIT, ROLLBACK, SET, USE, or SHOW, found {}",
                describe(other)
            ))),
        }
    }

    /// `CREATE {DATABASE | SCHEMA} [IF NOT EXISTS] <name>
    /// [[DEFAULT] CHARACTER SET [=] x] [[DEFAULT] COLLATE [=] y]` — the
    /// charset/collate clause is parsed only to be discarded (this server has
    /// exactly one charset/collation). `SCHEMA` is a true MySQL synonym for
    /// `DATABASE` here (unlike standard SQL, where they differ) — JDBC drivers
    /// (and so DBeaver) emit `CREATE SCHEMA`.
    fn parse_create_database(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Create)?;
        self.expect_database_or_schema()?;
        let if_not_exists = self.eat_if_not_exists()?;
        let name = self.expect_ident()?;
        self.consume_to_statement_end();
        self.eat_semicolon_and_ensure_end()?;
        Ok(Statement::CreateDatabase {
            name,
            if_not_exists,
        })
    }

    /// `DROP {DATABASE | SCHEMA} [IF EXISTS] <name>`.
    fn parse_drop_database(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Drop)?;
        self.expect_database_or_schema()?;
        let if_exists = self.eat_if_exists()?;
        let name = self.expect_ident()?;
        self.eat_semicolon_and_ensure_end()?;
        Ok(Statement::DropDatabase { name, if_exists })
    }

    /// `SET ...` — consume and ignore the rest of the statement. Covers the
    /// whole family (`SET NAMES`, `SET autocommit=1`, `SET @@session.x=y`,
    /// `SET SESSION TRANSACTION ...`) that clients send on connect.
    fn parse_set(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Set)?;
        while let Some(tok) = self.peek() {
            if *tok == Token::Semicolon {
                break;
            }
            self.pos += 1;
        }
        self.eat_semicolon_and_ensure_end()?;
        Ok(Statement::Set)
    }

    /// `USE <db>` — consume and ignore (schemaless server).
    fn parse_use(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Use)?;
        while let Some(tok) = self.peek() {
            if *tok == Token::Semicolon {
                break;
            }
            self.pos += 1;
        }
        self.eat_semicolon_and_ensure_end()?;
        Ok(Statement::Use)
    }

    /// `SHOW <what>` — recognizes a few common forms; anything else becomes
    /// [`ShowStatement::Other`] (consumed, yields an empty result).
    fn parse_show(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Show)?;
        // The word after SHOW (WARNINGS/VARIABLES/TABLES/DATABASES/...) lexes
        // as an ordinary identifier; match it case-insensitively.
        let head = match self.peek() {
            Some(Token::Ident(s)) => Some(s.to_ascii_uppercase()),
            _ => None,
        };
        let show = match head.as_deref() {
            Some("WARNINGS") | Some("ERRORS") => ShowStatement::Warnings,
            Some("DATABASES") | Some("SCHEMAS") => ShowStatement::Databases,
            Some("TABLES") => ShowStatement::Tables,
            Some("CHARSET") => ShowStatement::CharacterSet,
            // "CHARACTER SET": SET is a reserved keyword, not an Ident, so the
            // second word doesn't match through `head`'s Ident-only lookahead.
            Some("CHARACTER") if matches!(self.peek_at(1), Some(Token::Keyword(Keyword::Set))) => {
                ShowStatement::CharacterSet
            }
            Some("COLLATION") => ShowStatement::Collation,
            Some("VARIABLES") | Some("STATUS") => {
                self.pos += 1; // consume the head word
                let like = self.parse_optional_like()?;
                self.consume_to_statement_end();
                self.eat_semicolon_and_ensure_end()?;
                return Ok(Statement::Show(ShowStatement::Variables { like }));
            }
            _ => ShowStatement::Other,
        };
        self.consume_to_statement_end();
        self.eat_semicolon_and_ensure_end()?;
        Ok(Statement::Show(show))
    }

    /// After `SHOW VARIABLES`, an optional `LIKE '<pattern>'`.
    fn parse_optional_like(&mut self) -> Result<Option<String>> {
        let is_like =
            matches!(self.peek(), Some(Token::Ident(s)) if s.eq_ignore_ascii_case("LIKE"));
        if !is_like {
            return Ok(None);
        }
        self.pos += 1; // LIKE
        match self.advance() {
            Some(Token::Str(pattern)) => Ok(Some(pattern.clone())),
            other => Err(Error::Parse(format!(
                "expected a string pattern after LIKE, found {}",
                describe(other)
            ))),
        }
    }

    /// Consume any remaining tokens up to `;` or end of input.
    fn consume_to_statement_end(&mut self) {
        while let Some(tok) = self.peek() {
            if *tok == Token::Semicolon {
                break;
            }
            self.pos += 1;
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

    /// `CREATE TABLE [db.]name (col_def | table_constraint, ...) [options]`.
    /// The table name may be schema-qualified (see [`expect_qualified_ident`]);
    /// each comma-separated item is either a column definition (see
    /// [`parse_column_def`]) or a table-level constraint (see
    /// [`parse_table_constraint`]), disambiguated by its leading token; any
    /// trailing table options (`ENGINE=...`, `DEFAULT CHARSET=...`, ...) are
    /// parsed only to be discarded, same as `CREATE DATABASE`'s tail.
    ///
    /// [`expect_qualified_ident`]: Parser::expect_qualified_ident
    /// [`parse_column_def`]: Parser::parse_column_def
    /// [`parse_table_constraint`]: Parser::parse_table_constraint
    fn parse_create_table(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Create)?;
        self.expect_keyword(Keyword::Table)?;
        let table = self.expect_qualified_ident()?;
        self.expect_punct(&Token::LParen)?;

        let mut columns: Vec<ColumnDef> = Vec::new();
        loop {
            if self.peek_is_table_constraint_start() {
                self.parse_table_constraint(&mut columns)?;
            } else {
                columns.push(self.parse_column_def()?);
            }

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

        self.consume_to_statement_end();
        self.eat_semicolon_and_ensure_end()?;
        Ok(Statement::CreateTable { table, columns })
    }

    fn parse_insert(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Insert)?;
        self.expect_keyword(Keyword::Into)?;
        let table = self.expect_qualified_ident()?;

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
                let expr = self.parse_expr()?;
                let alias = self.parse_optional_alias()?;
                projection.push(SelectItem::Expr(expr, alias));
            }
            match self.peek() {
                Some(Token::Comma) => self.pos += 1,
                _ => break,
            }
        }

        let (from, selection) = if let Some(Token::Keyword(Keyword::From)) = self.peek() {
            self.pos += 1;
            let table = self.expect_qualified_ident()?;
            // An optional table alias (`FROM t alias` / `FROM t AS alias`) is
            // parsed and discarded: this server doesn't support qualifying a
            // column reference by table/alias (`alias.col`), only by name, so
            // there's nowhere for it to be used yet — but accepting it means a
            // query that includes one (common in generated SQL, e.g. from
            // `information_schema` introspection) parses instead of failing
            // at this token.
            self.parse_optional_alias()?;
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
        // Take an owned copy so the borrow on `self` ends and we can look
        // ahead (e.g. to detect `name(` as a function call).
        let token = self.advance().cloned();
        match token {
            Some(Token::Integer(n)) => Ok(Expr::Integer(n)),
            Some(Token::Str(s)) => Ok(Expr::String(s)),
            Some(Token::Keyword(Keyword::Null)) => Ok(Expr::Null),
            Some(Token::SystemVariable(name)) => Ok(Expr::SystemVariable(name)),
            Some(Token::Ident(name)) => {
                if matches!(self.peek(), Some(Token::LParen)) {
                    self.parse_function_call(name)
                } else {
                    Ok(Expr::Column(name))
                }
            }
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
                describe(other.as_ref())
            ))),
        }
    }

    /// Parse a function call whose name is already consumed and `peek()` is at
    /// the opening `(`, e.g. `DATABASE()` or `CONCAT(a, b)`.
    fn parse_function_call(&mut self, name: String) -> Result<Expr> {
        self.expect_punct(&Token::LParen)?;
        let mut args = Vec::new();
        if !matches!(self.peek(), Some(Token::RParen)) {
            loop {
                args.push(self.parse_expr()?);
                match self.peek() {
                    Some(Token::Comma) => self.pos += 1,
                    _ => break,
                }
            }
        }
        self.expect_punct(&Token::RParen)?;
        Ok(Expr::Function(name, args))
    }

    /// An optional column alias after a projected expression: `expr AS name`
    /// or the bare `expr name` form. A following clause keyword (`FROM`, ...)
    /// or punctuation is not an alias.
    fn parse_optional_alias(&mut self) -> Result<Option<String>> {
        if matches!(self.peek(), Some(Token::Keyword(Keyword::As))) {
            self.pos += 1;
            return Ok(Some(self.expect_ident()?));
        }
        if let Some(Token::Ident(name)) = self.peek() {
            let name = name.clone();
            self.pos += 1;
            return Ok(Some(name));
        }
        Ok(None)
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
                    SelectItem::Expr(e, alias) => {
                        Ok(SelectItem::Expr(bind_expr(e, params)?, alias))
                    }
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
        Expr::Function(name, args) => Ok(Expr::Function(
            name,
            args.into_iter()
                .map(|a| bind_expr(a, params))
                .collect::<Result<Vec<_>>>()?,
        )),
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
                    vec![SelectItem::Expr(Expr::String("it's".to_string()), None)]
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
                assert_eq!(projection, vec![SelectItem::Expr(Expr::Integer(-5), None)]);
            }
            other => panic!("expected Select, got {other:?}"),
        }
    }

    #[test]
    fn null_literal() {
        let stmt = parse("SELECT NULL").unwrap();
        match stmt {
            Statement::Select { projection, .. } => {
                assert_eq!(projection, vec![SelectItem::Expr(Expr::Null, None)]);
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
        // `~` isn't part of the supported grammar (unlike `#`/`--`/`/* */`,
        // which are comments).
        assert!(matches!(parse("SELECT ~1"), Err(Error::Parse(_))));
    }

    #[test]
    fn comments_are_skipped() {
        // Line comments (`#`, `-- `) and block comments, including the MySQL
        // executable form `/*! ... */`, whose inner SQL is unwrapped.
        assert!(parse("SELECT 1 # trailing line comment").is_ok());
        assert!(parse("SELECT 1 -- trailing line comment").is_ok());
        assert!(parse("/* leading block comment */ SELECT 1").is_ok());
        assert!(parse("SELECT /* inline */ 1").is_ok());
        assert!(parse("/*! SELECT 1 */").is_ok());
        assert!(parse("/*!40101 SELECT 1 */").is_ok());
    }

    #[test]
    fn backtick_quoted_identifiers() {
        assert!(parse("SELECT `id`, `full name` FROM `my table`").is_ok());
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
                        nullable: true,
                        auto_increment: false,
                    },
                    ColumnDef {
                        name: "name".to_string(),
                        type_name: "VARCHAR".to_string(),
                        is_primary_key: false,
                        nullable: true,
                        auto_increment: false,
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

    /// The literal DDL DBeaver's visual table editor generated (captured
    /// verbatim from a live debug-log session): a schema-qualified name,
    /// `AUTO_INCREMENT NOT NULL`, `NULL`, a table-level named `CONSTRAINT
    /// ... PRIMARY KEY (...)`, and trailing `DEFAULT CHARSET=...
    /// COLLATE=...` — every one of which the original grammar rejected.
    #[test]
    fn dbeaver_generated_create_table_parses() {
        let sql = "CREATE TABLE testfd.NewTable (\n\
                    \tId INT auto_increment NOT NULL,\n\
                    \tName varchar(100) NULL,\n\
                    \tCONSTRAINT NewTable_PK PRIMARY KEY (Id)\n\
                    )\n\
                    DEFAULT CHARSET=utf8mb4\n\
                    COLLATE=utf8mb4_general_ci";
        let stmt = parse(sql).unwrap_or_else(|e| panic!("parse failed: {e}"));
        match stmt {
            Statement::CreateTable { table, columns } => {
                // The schema qualifier is dropped; only the table name is kept.
                assert_eq!(table, "NewTable");
                assert_eq!(columns.len(), 2);
                assert_eq!(columns[0].name, "Id");
                assert!(
                    columns[0].is_primary_key,
                    "set by the table-level CONSTRAINT"
                );
                assert!(!columns[0].nullable, "NOT NULL, and implied by PRIMARY KEY");
                assert!(columns[0].auto_increment);
                assert_eq!(columns[1].name, "Name");
                assert!(!columns[1].is_primary_key);
                assert!(columns[1].nullable);
                assert!(!columns[1].auto_increment);
            }
            other => panic!("expected CreateTable, got {other:?}"),
        }
    }

    #[test]
    fn create_table_name_can_be_schema_qualified() {
        let stmt = parse("CREATE TABLE mydb.t (a INT)").unwrap();
        match stmt {
            Statement::CreateTable { table, .. } => assert_eq!(table, "t"),
            other => panic!("expected CreateTable, got {other:?}"),
        }
    }

    #[test]
    fn not_null_and_null_column_attributes() {
        let stmt = parse("CREATE TABLE t (a INT NOT NULL, b INT NULL, c INT)").unwrap();
        match stmt {
            Statement::CreateTable { columns, .. } => {
                assert!(!columns[0].nullable);
                assert!(columns[1].nullable);
                assert!(columns[2].nullable, "nullable by default");
            }
            other => panic!("expected CreateTable, got {other:?}"),
        }
    }

    #[test]
    fn auto_increment_column_attribute() {
        let stmt = parse("CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY)").unwrap();
        match stmt {
            Statement::CreateTable { columns, .. } => {
                assert!(columns[0].auto_increment);
                assert!(columns[0].is_primary_key);
            }
            other => panic!("expected CreateTable, got {other:?}"),
        }
    }

    #[test]
    fn table_level_primary_key_constraint_marks_the_column() {
        let stmt = parse("CREATE TABLE t (id INT, name VARCHAR, PRIMARY KEY (id))").unwrap();
        match stmt {
            Statement::CreateTable { columns, .. } => {
                assert!(columns[0].is_primary_key);
                assert!(!columns[1].is_primary_key);
            }
            other => panic!("expected CreateTable, got {other:?}"),
        }
    }

    #[test]
    fn table_level_composite_primary_key_is_unsupported() {
        assert!(matches!(
            parse("CREATE TABLE t (a INT, b INT, PRIMARY KEY (a, b))"),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn table_level_primary_key_unknown_column_is_a_parse_error() {
        assert!(matches!(
            parse("CREATE TABLE t (a INT, PRIMARY KEY (bogus))"),
            Err(Error::Parse(_))
        ));
    }

    #[test]
    fn unique_key_index_and_foreign_key_constraints_are_parsed_and_discarded() {
        assert!(parse("CREATE TABLE t (a INT, UNIQUE (a))").is_ok());
        assert!(parse("CREATE TABLE t (a INT, UNIQUE KEY uq_a (a))").is_ok());
        assert!(parse("CREATE TABLE t (a INT, KEY idx_a (a))").is_ok());
        assert!(parse("CREATE TABLE t (a INT, INDEX idx_a (a))").is_ok());
        assert!(parse(
            "CREATE TABLE t (a INT, b INT, \
             CONSTRAINT fk_b FOREIGN KEY (b) REFERENCES other(id) ON DELETE CASCADE)"
        )
        .is_ok());
    }

    #[test]
    fn column_default_comment_and_unique_attributes_are_parsed_and_discarded() {
        assert!(parse("CREATE TABLE t (a INT DEFAULT 0, b VARCHAR DEFAULT NULL)").is_ok());
        assert!(parse("CREATE TABLE t (a INT COMMENT 'a column')").is_ok());
        assert!(parse("CREATE TABLE t (a INT UNIQUE, b INT UNIQUE KEY)").is_ok());
        assert!(parse("CREATE TABLE t (a INT UNSIGNED, b BIGINT ZEROFILL)").is_ok());
    }

    #[test]
    fn create_table_trailing_options_are_discarded() {
        assert!(parse(
            "CREATE TABLE t (a INT) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 \
             COLLATE=utf8mb4_general_ci COMMENT='a table'"
        )
        .is_ok());
    }

    // ---- INSERT ----

    #[test]
    fn insert_into_qualified_table_name() {
        let stmt = parse("INSERT INTO mydb.t VALUES (1)").unwrap();
        match stmt {
            Statement::Insert { table, .. } => assert_eq!(table, "t"),
            other => panic!("expected Insert, got {other:?}"),
        }
    }

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
    fn select_from_qualified_table_name() {
        let stmt = parse("SELECT * FROM information_schema.TABLES").unwrap();
        assert_eq!(
            stmt,
            Statement::Select {
                projection: vec![SelectItem::Wildcard],
                from: Some("TABLES".to_string()),
                selection: None,
            }
        );
    }

    #[test]
    fn select_from_table_with_bare_and_as_alias() {
        assert!(parse("SELECT * FROM t alias").is_ok());
        assert!(parse("SELECT * FROM t AS alias").is_ok());
        // The alias is parsed and discarded, but WHERE afterward still works.
        assert!(parse("SELECT * FROM t alias WHERE a = 1").is_ok());
    }

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
                    SelectItem::Expr(Expr::Column("a".to_string()), None),
                    SelectItem::Expr(Expr::Column("b".to_string()), None),
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
                projection: vec![SelectItem::Expr(Expr::Integer(1), None)],
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
                projection: vec![SelectItem::Expr(
                    Expr::SystemVariable("version".to_string()),
                    None
                )],
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
        // A trailing integer isn't a valid alias, so it's genuine garbage.
        // (`SELECT 1 alias` *is* accepted now — a bare identifier is an alias.)
        assert!(matches!(parse("SELECT 1 2"), Err(Error::Parse(_))));
    }

    #[test]
    fn select_supports_column_aliases() {
        let stmt = parse("SELECT @@version AS v, 1 AS one, 2 three").unwrap();
        match stmt {
            Statement::Select { projection, .. } => {
                let aliases: Vec<Option<String>> = projection
                    .iter()
                    .map(|item| match item {
                        SelectItem::Expr(_, alias) => alias.clone(),
                        SelectItem::Wildcard => None,
                    })
                    .collect();
                assert_eq!(
                    aliases,
                    vec![
                        Some("v".to_string()),
                        Some("one".to_string()),
                        Some("three".to_string()),
                    ]
                );
            }
            other => panic!("expected Select, got {other:?}"),
        }
    }

    // ---- CREATE/DROP DATABASE, SHOW CHARACTER SET/COLLATION ----

    #[test]
    fn create_database_basic_and_if_not_exists() {
        assert_eq!(
            parse("CREATE DATABASE mydb").unwrap(),
            Statement::CreateDatabase {
                name: "mydb".to_string(),
                if_not_exists: false,
            }
        );
        assert_eq!(
            parse("CREATE DATABASE IF NOT EXISTS mydb").unwrap(),
            Statement::CreateDatabase {
                name: "mydb".to_string(),
                if_not_exists: true,
            }
        );
        // Case-insensitive, and CREATE TABLE is unaffected by the lookahead.
        assert!(parse("create database mydb").is_ok());
        assert!(parse("CREATE TABLE t (a INT)").is_ok());
    }

    /// `SCHEMA` is a true MySQL synonym for `DATABASE` (unlike standard SQL).
    /// JDBC drivers — and so DBeaver's "Create Database" action — emit
    /// `CREATE SCHEMA`, not `CREATE DATABASE`; this is a regression test for
    /// exactly that failure (`expected TABLE, found 'SCHEMA'`).
    #[test]
    fn schema_is_a_synonym_for_database() {
        assert_eq!(
            parse("CREATE SCHEMA mydb").unwrap(),
            Statement::CreateDatabase {
                name: "mydb".to_string(),
                if_not_exists: false,
            }
        );
        assert_eq!(
            parse("CREATE SCHEMA IF NOT EXISTS mydb").unwrap(),
            Statement::CreateDatabase {
                name: "mydb".to_string(),
                if_not_exists: true,
            }
        );
        assert_eq!(
            parse("DROP SCHEMA IF EXISTS mydb").unwrap(),
            Statement::DropDatabase {
                name: "mydb".to_string(),
                if_exists: true,
            }
        );
    }

    #[test]
    fn create_database_discards_charset_and_collate_clauses() {
        assert!(parse("CREATE DATABASE mydb DEFAULT CHARACTER SET utf8mb4").is_ok());
        assert!(
            parse("CREATE DATABASE mydb CHARACTER SET = utf8mb4 COLLATE = utf8mb4_general_ci")
                .is_ok()
        );
    }

    #[test]
    fn drop_database_basic_and_if_exists() {
        assert_eq!(
            parse("DROP DATABASE mydb").unwrap(),
            Statement::DropDatabase {
                name: "mydb".to_string(),
                if_exists: false,
            }
        );
        assert_eq!(
            parse("DROP DATABASE IF EXISTS mydb").unwrap(),
            Statement::DropDatabase {
                name: "mydb".to_string(),
                if_exists: true,
            }
        );
    }

    #[test]
    fn show_character_set_and_charset_and_collation() {
        assert_eq!(
            parse("SHOW CHARACTER SET").unwrap(),
            Statement::Show(ShowStatement::CharacterSet)
        );
        assert_eq!(
            parse("SHOW CHARSET").unwrap(),
            Statement::Show(ShowStatement::CharacterSet)
        );
        assert_eq!(
            parse("SHOW COLLATION").unwrap(),
            Statement::Show(ShowStatement::Collation)
        );
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
