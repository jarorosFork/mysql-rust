//! Storage engine abstraction.
//!
//! Concrete engines (in-memory, on-disk, ...) implement the [`Storage`]
//! trait so the query executor can stay engine-agnostic.

pub mod engine;
pub mod log;
pub mod log_writer;
pub mod transaction;
pub mod value;

pub use engine::InMemoryStorage;
pub use transaction::Transaction;
pub use value::{format_decimal, ColumnSchema, ColumnType, TableSchema, Value};

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::Result;

/// The return type of every [`Storage`] method that has to reach the log
/// (and, once PERFORMANCE_DURABILITY_PLAN.md PD-2's dedicated writer thread
/// is in the mix, genuinely `.await` an ack rather than block a caller).
/// Hand-rolled rather than pulling in the `async-trait` crate: `Storage` is
/// used as `&dyn Storage` (see [`crate::query::executor::Executor`]), and
/// native `async fn` in traits isn't dyn-compatible — a boxed future is
/// exactly what that crate expands to anyway, just written out directly for
/// the handful of methods that actually need it.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A pluggable storage backend. `&self` (not `&mut self`) so a single
/// instance can eventually be shared across connections (Phase 6); engines
/// use interior mutability. `Send + Sync`: a `Connection`'s per-task future
/// holds a `&dyn Storage` (or a `Transaction`, which implements it) across
/// `.await` points, so both the trait object and everything reachable
/// through it must be safe to send between threads.
pub trait Storage: Send + Sync {
    /// Create a table with the given schema and optional primary-key column.
    fn create_table<'a>(
        &'a self,
        name: &'a str,
        columns: Vec<ColumnSchema>,
        primary_key: Option<String>,
    ) -> BoxFuture<'a, Result<()>>;

    /// Return the names of all tables.
    fn tables(&self) -> Result<Vec<String>>;

    /// Return `name`'s full schema, shared rather than cloned
    /// (PERFORMANCE_DURABILITY_PLAN.md P6): called at least once per
    /// statement and twice-plus per `JOIN`, so a deep clone of every
    /// column's name `String` on every call is a real per-query allocation
    /// storm on a wide table. `InMemoryStorage` keeps one `Arc<TableSchema>`
    /// per table and hands out clones of the `Arc` (a refcount bump).
    fn table_schema(&self, name: &str) -> Result<Arc<TableSchema>>;

    /// Append a row. `row` must have exactly as many values as the table
    /// has columns, in column order, and already be type-checked.
    fn insert_row<'a>(&'a self, table: &'a str, row: Vec<Value>) -> BoxFuture<'a, Result<()>>;

    /// Insert several `(table, row)` pairs as one atomic unit where the
    /// underlying engine supports it — a durable engine logs them as a
    /// single record, so a crash partway through can never leave a partial
    /// result on disk (see [`InMemoryStorage`]'s override and
    /// PERFORMANCE_DURABILITY_PLAN.md D2). Used for a multi-row `INSERT`
    /// statement and for `COMMIT`.
    ///
    /// The default just inserts one at a time, which is exactly right for
    /// [`Transaction`]: its buffered pending rows aren't durable until
    /// `commit()` — which itself calls this same method on the *real*
    /// storage, where the override's atomicity actually applies.
    fn insert_rows(&self, rows: Vec<(String, Vec<Value>)>) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            for (table, row) in rows {
                self.insert_row(&table, row).await?;
            }
            Ok(())
        })
    }

    /// Return every row in `table`, in insertion order.
    fn scan(&self, table: &str) -> Result<Vec<Vec<Value>>>;

    /// Return every row in `table` for which `filter` returns `true`,
    /// without cloning the rows that don't match
    /// (PERFORMANCE_DURABILITY_PLAN.md P1) — unlike `scan()` followed by an
    /// in-memory `.filter()`, which clones the *whole* table (every
    /// `Varchar`'s heap `String` included) before throwing most of it away.
    /// No default: a fallback of "call `scan` then filter" would compile
    /// but silently defeat the entire point for a future implementor who
    /// forgets to override it, so both current implementors
    /// ([`InMemoryStorage`] and [`Transaction`]'s pending-row overlay) are
    /// required to provide the real thing.
    fn scan_filtered(
        &self,
        table: &str,
        filter: &mut dyn FnMut(&[Value]) -> bool,
    ) -> Result<Vec<Vec<Value>>>;

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
    ///
    /// Durable (PERFORMANCE_DURABILITY_PLAN.md D8) — like `create_table`,
    /// this has to reach the log, hence `BoxFuture` rather than a plain
    /// `Result`.
    fn create_database<'a>(
        &'a self,
        name: &'a str,
        if_not_exists: bool,
    ) -> BoxFuture<'a, Result<()>>;

    /// Unregister a database name. Errors if it doesn't exist, unless
    /// `if_exists` is set.
    fn drop_database<'a>(&'a self, name: &'a str, if_exists: bool) -> BoxFuture<'a, Result<()>>;

    /// Return the names of all registered databases.
    fn databases(&self) -> Result<Vec<String>>;
}
