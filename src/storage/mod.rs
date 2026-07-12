//! Storage engine abstraction.
//!
//! Concrete engines (in-memory, on-disk, ...) implement the [`Storage`]
//! trait so the query executor can stay engine-agnostic.

pub mod engine;
pub mod log;
pub mod transaction;
pub mod value;

pub use engine::InMemoryStorage;
pub use transaction::Transaction;
pub use value::{format_decimal, ColumnSchema, ColumnType, TableSchema, Value};

use crate::Result;

/// A pluggable storage backend. `&self` (not `&mut self`) so a single
/// instance can eventually be shared across connections (Phase 6); engines
/// use interior mutability.
pub trait Storage {
    /// Create a table with the given schema and optional primary-key column.
    fn create_table(
        &self,
        name: &str,
        columns: Vec<ColumnSchema>,
        primary_key: Option<String>,
    ) -> Result<()>;

    /// Return the names of all tables.
    fn tables(&self) -> Result<Vec<String>>;

    /// Return `name`'s full schema.
    fn table_schema(&self, name: &str) -> Result<TableSchema>;

    /// Append a row. `row` must have exactly as many values as the table
    /// has columns, in column order, and already be type-checked.
    fn insert_row(&self, table: &str, row: Vec<Value>) -> Result<()>;

    /// Return every row in `table`, in insertion order.
    fn scan(&self, table: &str) -> Result<Vec<Vec<Value>>>;

    /// Look up a row by its primary-key value in O(1) rather than scanning.
    /// Returns `Ok(None)` if the table has no matching row (or no primary
    /// key at all — callers should check `table_schema` first).
    fn lookup_by_primary_key(&self, table: &str, key: &Value) -> Result<Option<Vec<Value>>>;

    /// Reserve and return the next value for `table`'s `AUTO_INCREMENT`
    /// column. Errors if the table doesn't exist or has no such column.
    /// **Not transactional** — matching real MySQL/InnoDB, a reserved value
    /// is never reused even if the insert that requested it is rolled back
    /// (see `Transaction`'s implementation), so a sequence can show gaps.
    fn next_auto_increment(&self, table: &str) -> Result<i64>;

    /// Register a database name. Errors if it already exists, unless
    /// `if_not_exists` is set (in which case that's a silent no-op, matching
    /// `CREATE DATABASE IF NOT EXISTS`). This is a lightweight namespace
    /// registry only — table storage itself stays flat/global regardless of
    /// which database name is "current" (this server has no `USE`-scoped
    /// table separation); it exists so `CREATE`/`DROP`/`SHOW DATABASES`, which
    /// standard clients and GUI tools issue, work rather than error.
    fn create_database(&self, name: &str, if_not_exists: bool) -> Result<()>;

    /// Unregister a database name. Errors if it doesn't exist, unless
    /// `if_exists` is set.
    fn drop_database(&self, name: &str, if_exists: bool) -> Result<()>;

    /// Return the names of all registered databases.
    fn databases(&self) -> Result<Vec<String>>;
}
