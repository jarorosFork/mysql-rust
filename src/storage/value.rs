//! Typed values and schema definitions.
//!
//! `INT` and `VARCHAR` were the two required types for Phase 5; Phase 11
//! added `BOOLEAN` (a pure alias for `INT` — exactly how real MySQL treats
//! it, not a distinct storage type), `DECIMAL` (exact fixed-point, not a
//! float — see `Value::Decimal`), and `DATE`.

use crate::{Error, Result};

/// A typed, storage-level value. `Null` is a distinct value from any other
/// variant, matching SQL's `NULL`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Value {
    Int(i64),
    Varchar(String),
    /// An exact fixed-point number: `unscaled / 10^scale` (e.g. `(12345, 2)`
    /// is `123.45`). Never a float — `DECIMAL`'s entire point is avoiding
    /// binary floating-point rounding, so `f64` would defeat it, and `f64`
    /// can't derive `Eq`/`Hash` anyway (`NAN`), which the primary-key index
    /// needs. `coerce` (see `query::executor`) always normalizes a value to
    /// its column's declared scale before it reaches storage, so any two
    /// values in one column compare/hash consistently.
    Decimal(i64, u8),
    /// A calendar date, stored pre-validated as canonical `YYYY-MM-DD` text
    /// (zero-padded). Deliberately just a `String`, not a `(year, month,
    /// day)` tuple: zero-padded ISO-8601 text sorts identically to
    /// chronological order, so ordinary string comparison is already
    /// correct — no date-arithmetic code needed anywhere (this server
    /// doesn't do date arithmetic; see ROADMAP.md Phase 11's cut list).
    Date(String),
    Null,
}

impl Value {
    /// Render for the text protocol: `None` means SQL `NULL` (encoded on
    /// the wire as the dedicated NULL marker, not the text "NULL").
    pub fn to_display_string(&self) -> Option<String> {
        match self {
            Value::Int(n) => Some(n.to_string()),
            Value::Varchar(s) => Some(s.clone()),
            Value::Decimal(unscaled, scale) => Some(format_decimal(*unscaled, *scale)),
            Value::Date(s) => Some(s.clone()),
            Value::Null => None,
        }
    }
}

/// Render a fixed-point `(unscaled, scale)` pair as decimal text, e.g.
/// `(12345, 2)` -> `"123.45"`, `(5, 2)` -> `"0.05"`, `(100, 0)` -> `"100"`.
pub fn format_decimal(unscaled: i64, scale: u8) -> String {
    if scale == 0 {
        return unscaled.to_string();
    }
    let negative = unscaled < 0;
    let scale = scale as usize;
    let digits = unscaled.unsigned_abs().to_string();
    // Left-pad with zeros so there's always at least one integer digit plus
    // `scale` fractional digits to split off (e.g. 5 at scale 2 -> "005").
    let digits = if digits.len() <= scale {
        format!("{digits:0>width$}", width = scale + 1)
    } else {
        digits
    };
    let (int_part, frac_part) = digits.split_at(digits.len() - scale);
    format!("{}{int_part}.{frac_part}", if negative { "-" } else { "" })
}

/// Convert a fixed-point value from `from_scale` to `to_scale` (widening
/// multiplies; narrowing rounds half-away-from-zero), with checked
/// arithmetic throughout so an absurd scale/magnitude combination is a clean
/// `Error::Execution`, never an overflow panic.
pub fn rescale_decimal(
    unscaled: i64,
    from_scale: u8,
    to_scale: u8,
    column_name: &str,
) -> Result<i64> {
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
pub fn parse_decimal_literal(s: &str, column_name: &str) -> Result<(i64, u8)> {
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
pub fn parse_date_literal(s: &str, column_name: &str) -> Result<String> {
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

/// A column's declared type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Int,
    Varchar,
    /// Exact fixed-point, carrying the declared scale (digits after the
    /// decimal point) — `DECIMAL` alone or `DECIMAL(M)` is scale 0,
    /// `DECIMAL(M, D)` is scale `D`, matching MySQL's own default.
    Decimal(u8),
    Date,
}

impl ColumnType {
    /// Recognize a type name as written in `CREATE TABLE` (case-insensitive),
    /// e.g. `"VARCHAR(255)"` or `"DECIMAL(10,2)"`. Many SQL type names share
    /// one of the physical representations here — every integer width
    /// (including `BOOLEAN`/`BOOL`, exactly as real MySQL treats them) stores
    /// as `Int`, every string/text/blob kind as `Varchar` — so they're
    /// accepted as synonyms.
    pub fn from_name(name: &str) -> Option<Self> {
        let trimmed = name.trim();
        let base = trimmed.split('(').next().unwrap_or(trimmed).trim();
        match base.to_ascii_uppercase().as_str() {
            "INT" | "INTEGER" | "BIGINT" | "SMALLINT" | "TINYINT" | "MEDIUMINT" | "BOOLEAN"
            | "BOOL" => Some(ColumnType::Int),
            "VARCHAR" | "CHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" => {
                Some(ColumnType::Varchar)
            }
            "DATE" => Some(ColumnType::Date),
            "DECIMAL" | "NUMERIC" | "DEC" => Some(ColumnType::Decimal(decimal_scale(trimmed))),
            _ => None,
        }
    }
}

/// Parse the scale (second, comma-separated number) out of a `DECIMAL(M, D)`
/// suffix; `DECIMAL`, `DECIMAL(M)`, or an unparseable suffix all mean scale 0.
fn decimal_scale(type_text: &str) -> u8 {
    type_text
        .split_once('(')
        .and_then(|(_, rest)| rest.strip_suffix(')'))
        .and_then(|args| args.split(',').nth(1))
        .and_then(|scale| scale.trim().parse::<u8>().ok())
        .unwrap_or(0)
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
            "boolean",
            "BOOL",
        ] {
            assert_eq!(ColumnType::from_name(name), Some(ColumnType::Int));
        }
        for name in ["varchar", "VARCHAR", "char", "text", "longtext", "TinyText"] {
            assert_eq!(ColumnType::from_name(name), Some(ColumnType::Varchar));
        }
        assert_eq!(ColumnType::from_name("date"), Some(ColumnType::Date));
        assert_eq!(ColumnType::from_name("bogus"), None);
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
    fn decimal_scale_is_parsed_from_the_second_argument() {
        assert_eq!(
            ColumnType::from_name("DECIMAL"),
            Some(ColumnType::Decimal(0))
        );
        assert_eq!(
            ColumnType::from_name("DECIMAL(10)"),
            Some(ColumnType::Decimal(0))
        );
        assert_eq!(
            ColumnType::from_name("DECIMAL(10,2)"),
            Some(ColumnType::Decimal(2))
        );
        assert_eq!(
            ColumnType::from_name("numeric(8, 4)"),
            Some(ColumnType::Decimal(4))
        );
        assert_eq!(
            ColumnType::from_name("DEC(5,1)"),
            Some(ColumnType::Decimal(1))
        );
    }

    #[test]
    fn null_has_no_display_string() {
        assert_eq!(Value::Null.to_display_string(), None);
        assert_eq!(Value::Int(5).to_display_string(), Some("5".to_string()));
    }

    #[test]
    fn format_decimal_places_the_decimal_point() {
        assert_eq!(format_decimal(12345, 2), "123.45");
        assert_eq!(format_decimal(100, 2), "1.00");
        assert_eq!(format_decimal(5, 2), "0.05");
        assert_eq!(format_decimal(0, 2), "0.00");
        assert_eq!(format_decimal(-1550, 2), "-15.50");
        assert_eq!(format_decimal(42, 0), "42");
    }

    #[test]
    fn decimal_value_to_display_string_matches_format_decimal() {
        assert_eq!(
            Value::Decimal(12345, 2).to_display_string(),
            Some("123.45".to_string())
        );
    }

    #[test]
    fn date_value_to_display_string_is_the_stored_text() {
        assert_eq!(
            Value::Date("2024-01-15".to_string()).to_display_string(),
            Some("2024-01-15".to_string())
        );
    }
}
