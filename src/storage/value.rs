//! Typed values and schema definitions.
//!
//! `INT` and `VARCHAR` are the two required types (see ROADMAP.md Phase 5);
//! more (`DECIMAL`, `DATE`, ...) can join `ColumnType` as they're needed.

/// A typed, storage-level value. `Null` is a distinct value from any
/// `Int`/`Varchar`, matching SQL's `NULL`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Value {
    Int(i64),
    Varchar(String),
    Null,
}

impl Value {
    /// Render for the text protocol: `None` means SQL `NULL` (encoded on
    /// the wire as the dedicated NULL marker, not the text "NULL").
    pub fn to_display_string(&self) -> Option<String> {
        match self {
            Value::Int(n) => Some(n.to_string()),
            Value::Varchar(s) => Some(s.clone()),
            Value::Null => None,
        }
    }
}

/// A column's declared type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Int,
    Varchar,
}

impl ColumnType {
    /// Recognize a type name as written in `CREATE TABLE` (case-insensitive).
    /// Many SQL type names share one of the two physical representations —
    /// every integer width stores as `Int` (i64) and every string/text/blob
    /// kind stores as `Varchar` — so they're accepted as synonyms. Genuinely
    /// distinct physical types (`DATE`, `DECIMAL` with exact scale, real
    /// binary `BLOB`) would need their own `Value` variant and are not
    /// accepted yet.
    pub fn from_name(name: &str) -> Option<Self> {
        // Strip a size/precision suffix like `VARCHAR(255)` or `INT(11)` —
        // the parser also consumes it, but be lenient if a bare name arrives.
        let base = name.split('(').next().unwrap_or(name).trim();
        match base.to_ascii_uppercase().as_str() {
            "INT" | "INTEGER" | "BIGINT" | "SMALLINT" | "TINYINT" | "MEDIUMINT" => {
                Some(ColumnType::Int)
            }
            "VARCHAR" | "CHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" => {
                Some(ColumnType::Varchar)
            }
            _ => None,
        }
    }
}

/// A single column's name and type.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnSchema {
    pub name: String,
    pub column_type: ColumnType,
    /// Whether `NULL` is a legal value for this column. A primary-key column
    /// is always non-nullable, regardless of how it's declared (matching SQL:
    /// `PRIMARY KEY` implies `NOT NULL`) — see `Executor::execute_create_table`.
    pub nullable: bool,
    /// Whether this column auto-assigns the next sequential integer value on
    /// insert when its value is `NULL` (explicitly, or because it was omitted
    /// from an explicit column list). At most one per table, and it must be
    /// the primary key — this engine's single index is the primary key, and
    /// `AUTO_INCREMENT` needs to be on an indexed column to mean anything
    /// (see `Executor::execute_create_table`, `Storage::next_auto_increment`).
    pub auto_increment: bool,
}

/// A table's full schema.
#[derive(Debug, Clone)]
pub struct TableSchema {
    pub columns: Vec<ColumnSchema>,
    /// The column serving as the primary key, if any (see ROADMAP.md
    /// Phase 5's "primary-key / basic index lookup").
    pub primary_key: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_type_recognizes_synonyms_case_insensitively() {
        for name in [
            "int",
            "INT",
            "Integer",
            "bigint",
            "SMALLINT",
            "tinyint",
            "mediumint",
        ] {
            assert_eq!(ColumnType::from_name(name), Some(ColumnType::Int));
        }
        for name in ["varchar", "VARCHAR", "char", "text", "longtext", "TinyText"] {
            assert_eq!(ColumnType::from_name(name), Some(ColumnType::Varchar));
        }
        assert_eq!(ColumnType::from_name("bogus"), None);
        // Genuinely distinct physical types aren't accepted yet.
        assert_eq!(ColumnType::from_name("DATE"), None);
        assert_eq!(ColumnType::from_name("DECIMAL"), None);
    }

    #[test]
    fn column_type_ignores_a_size_suffix() {
        assert_eq!(
            ColumnType::from_name("VARCHAR(255)"),
            Some(ColumnType::Varchar)
        );
        assert_eq!(ColumnType::from_name("INT(11)"), Some(ColumnType::Int));
    }

    #[test]
    fn null_has_no_display_string() {
        assert_eq!(Value::Null.to_display_string(), None);
        assert_eq!(Value::Int(5).to_display_string(), Some("5".to_string()));
    }
}
