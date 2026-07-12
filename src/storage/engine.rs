//! An in-memory storage backend with an optional on-disk log for
//! persistence (see `storage::log`).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::config::SyncPolicy;
use crate::storage::log::{Entry, Log};
use crate::storage::log_writer::LogWriter;
use crate::storage::value::{ColumnSchema, TableSchema, Value};
use crate::storage::{BoxFuture, Storage};
use crate::{Error, Result};

/// How long a transaction waits for a table's write lock before giving up
/// (matches MySQL's `innodb_lock_wait_timeout` behavior: a stuck lock wait
/// fails loudly instead of hanging the connection forever). Deliberately
/// not configurable yet — revisit if a real deployment needs it tuned.
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

struct Table {
    columns: Vec<ColumnSchema>,
    primary_key: Option<String>,
    /// Position of the primary-key column within `columns`, cached for
    /// O(1) access on every insert/lookup.
    primary_key_index: Option<usize>,
    rows: Vec<Vec<Value>>,
    /// Primary-key value -> row index. Empty (and unused) if `primary_key`
    /// is `None`.
    index: HashMap<Value, usize>,
    /// `(column index, next value)` for this table's `AUTO_INCREMENT`
    /// column, if it has one. Bumped by every inserted row (live or
    /// replayed — see `push_trusted`) to stay ahead of the largest value
    /// actually present, so restart-then-insert and explicit-value-then-
    /// auto-assign both continue correctly, matching real MySQL.
    auto_increment: Option<(usize, i64)>,
}

impl Table {
    fn new(columns: Vec<ColumnSchema>, primary_key: Option<String>) -> Self {
        let primary_key_index = primary_key
            .as_ref()
            .and_then(|pk| columns.iter().position(|c| &c.name == pk));
        let auto_increment = columns
            .iter()
            .position(|c| c.auto_increment)
            .map(|idx| (idx, 1i64));
        Table {
            columns,
            primary_key,
            primary_key_index,
            rows: Vec::new(),
            index: HashMap::new(),
            auto_increment,
        }
    }

    /// Append a row without checking primary-key uniqueness — used to
    /// replay already-validated entries from the log.
    fn push_trusted(&mut self, row: Vec<Value>) {
        if let Some(idx) = self.primary_key_index {
            self.index.insert(row[idx].clone(), self.rows.len());
        }
        if let Some((ai_idx, next)) = &mut self.auto_increment {
            if let Value::Int(v) = row[*ai_idx] {
                *next = (*next).max(v + 1);
            }
        }
        self.rows.push(row);
    }

    /// Validate that `row` can be inserted — primary-key uniqueness only
    /// (column count is checked by the caller) — without mutating
    /// anything. Split out from the old fused check-and-insert so
    /// `InMemoryStorage::insert_row` can validate, then durably log,
    /// *then* apply — see its doc comment for why that order matters
    /// (PERFORMANCE_DURABILITY_PLAN.md D3).
    fn check_insertable(&self, row: &[Value]) -> Result<()> {
        if let Some(idx) = self.primary_key_index {
            if self.index.contains_key(&row[idx]) {
                return Err(Error::Execution(format!(
                    "Duplicate entry '{}' for key 'PRIMARY'",
                    row[idx].to_display_string().unwrap_or_default()
                )));
            }
        }
        Ok(())
    }

    /// The primary-key value `row` would be inserted under, if this table
    /// has one — used to additionally detect a duplicate key *within* one
    /// batch (see `InMemoryStorage::insert_rows`), which `check_insertable`
    /// alone can't: it only ever sees already-committed state.
    fn primary_key_value<'a>(&self, row: &'a [Value]) -> Option<&'a Value> {
        self.primary_key_index.map(|idx| &row[idx])
    }
}

fn apply_entry(tables: &mut HashMap<String, Table>, entry: Entry) {
    match entry {
        Entry::CreateTable {
            table,
            columns,
            primary_key,
        } => {
            tables.insert(table, Table::new(columns, primary_key));
        }
        Entry::InsertRow { table, row } => {
            // Trusted: this table was created by an earlier entry in the
            // same log, replayed just above.
            if let Some(t) = tables.get_mut(&table) {
                t.push_trusted(row);
            }
        }
        Entry::Transaction { rows } => {
            // The record itself is all-or-nothing (see `storage::log`'s
            // module doc comment) -- by the time `apply_entry` sees it,
            // every row in it is known-intact, so applying them in a
            // simple loop is exactly as atomic as the record was.
            for (table, row) in rows {
                if let Some(t) = tables.get_mut(&table) {
                    t.push_trusted(row);
                }
            }
        }
    }
}

/// Keeps everything in memory; optionally mirrors every mutation to an
/// on-disk log so it survives a restart (see [`InMemoryStorage::open`]).
///
/// Shared across every connection on a server via `Arc` (see
/// `server::Server::serve`), so reads (`scan`, `lookup_by_primary_key`, ...)
/// use a read lock and can proceed concurrently; only `create_table` /
/// `insert_row` take the write lock.
#[derive(Default)]
pub struct InMemoryStorage {
    tables: RwLock<HashMap<String, Table>>,
    /// The dedicated log-writer thread (PERFORMANCE_DURABILITY_PLAN.md
    /// PD-2) — `None` for a purely in-memory store with nothing to persist
    /// (see [`Self::new`]). Every mutating `Storage` method `.await`s an
    /// ack from it rather than writing inline, so a multi-millisecond
    /// `fsync` never blocks the tokio worker running the caller's
    /// statement, and concurrent writers naturally get group commit (many
    /// queued appends become one write + one fsync) instead of paying their
    /// own separate fsync each.
    log_writer: Option<LogWriter>,
    /// One dedicated async mutex per table, handed out by [`Self::lock_table`]
    /// so a transaction (see `storage::transaction`) can hold a table's
    /// write lock across multiple statements/await points — something a
    /// `std::sync::Mutex` guard cannot safely do. This registry mutex
    /// itself is only ever held for the instant it takes to look up or
    /// insert an `Arc`, never across an `.await`.
    table_locks: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Registered database names (see `Storage::create_database`). Unlike
    /// tables, this is **not** persisted to the log or across a restart: it's
    /// a lightweight compatibility namespace, not a real schema-separation
    /// feature, and table data was never partitioned by database to begin
    /// with (so nothing about table data is affected by this being in-memory
    /// only).
    databases: RwLock<HashSet<String>>,
    /// Test-only fault injection: when set, the next [`Self::append_log`]
    /// call fails without touching the real log, so tests can verify the
    /// log-before-memory ordering invariant (PERFORMANCE_DURABILITY_PLAN.md
    /// D3) deterministically. A genuine OS-level write failure isn't
    /// reliably triggerable on an already-open file handle without
    /// platform-specific machinery this project doesn't otherwise need;
    /// this compiles to nothing in a non-test build.
    #[cfg(test)]
    fail_next_log_write: std::sync::atomic::AtomicBool,
}

impl InMemoryStorage {
    /// A purely in-memory store; nothing is persisted.
    pub fn new() -> Self {
        InMemoryStorage::default()
    }

    /// Open (creating if necessary) a store backed by a log file at `path`.
    /// Any existing data is replayed into memory immediately. `sync_policy`
    /// governs how aggressively subsequent writes are forced durable (see
    /// `Log::open`, PERFORMANCE_DURABILITY_PLAN.md D1).
    pub fn open(path: &Path, sync_policy: SyncPolicy) -> Result<Self> {
        let mut tables: HashMap<String, Table> = HashMap::new();
        let log = Log::open(path, sync_policy, |entry| apply_entry(&mut tables, entry))?;
        Ok(InMemoryStorage {
            tables: RwLock::new(tables),
            log_writer: Some(LogWriter::spawn(log)),
            table_locks: tokio::sync::Mutex::new(HashMap::new()),
            databases: RwLock::new(HashSet::new()),
            #[cfg(test)]
            fail_next_log_write: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Test-only: make the next log append fail, once, as if the real
    /// write had failed at the OS level — checked by every mutating
    /// `Storage` method just before it hands a record to the log writer.
    #[cfg(test)]
    fn fail_next_log_write(&self) {
        self.fail_next_log_write
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Test-only: `Err` (consuming the one-shot fault) if
    /// [`Self::fail_next_log_write`] armed it, `Ok` otherwise — see that
    /// method's doc comment. A real OS-level write failure isn't reliably
    /// triggerable on an already-open file handle, so this is the seam
    /// tests use instead (mirrors `LogWriter`'s own `set_fail_batches`,
    /// which covers the writer thread's *batch* semantics specifically;
    /// this one covers "the caller's append attempt never happened at
    /// all").
    #[cfg(test)]
    fn check_fault_injection(&self) -> Result<()> {
        if self
            .fail_next_log_write
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(Error::Io(std::io::Error::other(
                "fault-injected log write failure (test only)",
            )));
        }
        Ok(())
    }

    /// Acquire `table`'s exclusive write lock, waiting up to
    /// `LOCK_WAIT_TIMEOUT`. Used by transactions (`storage::transaction`)
    /// and by single-statement autocommit writes alike, so a multi-statement
    /// transaction can never be raced by (or race) another writer on the
    /// same table — this is what makes the locking "sufficient to prevent
    /// lost updates" (see ROADMAP.md Phase 7).
    pub async fn lock_table(&self, table: &str) -> Result<tokio::sync::OwnedMutexGuard<()>> {
        let lock = {
            let mut locks = self.table_locks.lock().await;
            locks
                .entry(table.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        tokio::time::timeout(LOCK_WAIT_TIMEOUT, lock.lock_owned())
            .await
            .map_err(|_| {
                Error::Execution(
                    "Lock wait timeout exceeded; try restarting transaction".to_string(),
                )
            })
    }

    /// Open (creating the directory and a fixed-name data file within it if
    /// necessary) a persistent store rooted at `dir`.
    pub fn open_in_dir(dir: &Path, sync_policy: SyncPolicy) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        Self::open(&dir.join("data.log"), sync_policy)
    }
}

impl Storage for InMemoryStorage {
    fn create_table<'a>(
        &'a self,
        name: &'a str,
        columns: Vec<ColumnSchema>,
        primary_key: Option<String>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(pk) = &primary_key {
                if !columns.iter().any(|c| &c.name == pk) {
                    return Err(Error::Execution(format!(
                        "Primary key column '{pk}' is not defined"
                    )));
                }
            }

            // Log-before-memory (PERFORMANCE_DURABILITY_PLAN.md D3), same
            // shape as `insert_row` below: check under a read lock
            // (released), append durably (no lock held across the
            // `.await`), then apply under a freshly-acquired write lock.
            // Before PD-2 this held one write lock across the whole
            // operation instead — CREATE TABLE has no connection-level
            // per-table lock to lean on the way INSERT does (there's no
            // table yet to lock by name), so that was the one place a log
            // append happened inside a critical section. Holding a
            // `std::sync` lock across an `.await` now that the log append
            // genuinely awaits the writer thread would block every other
            // reader/writer for however long that takes, and risks
            // stalling a tokio worker if another task's blocking
            // `.read()`/`.write()` call lands while this task is
            // suspended holding the guard — so this drops the lock instead
            // and re-checks after re-acquiring it. The rare cost: two
            // concurrent `CREATE TABLE t` calls can now both pass the
            // first check and both durably log a `CreateTable` record for
            // the same name; the loser's apply below sees the name already
            // taken and returns "already exists" instead of silently
            // overwriting the winner. On replay this is harmless (a
            // `CreateTable` for an already-existing name simply re-creates
            // the same empty table, exactly matching the winner's own
            // record), so the outcome is a wasted log record on a genuinely
            // rare race, never data loss or corruption.
            {
                let tables = self.tables.read().unwrap_or_else(|e| e.into_inner());
                if tables.contains_key(name) {
                    return Err(Error::Execution(format!("Table '{name}' already exists")));
                }
            }

            #[cfg(test)]
            self.check_fault_injection()?;
            if let Some(writer) = &self.log_writer {
                writer
                    .append_create_table(name, &columns, primary_key.as_deref())
                    .await?;
            }

            let mut tables = self.tables.write().unwrap_or_else(|e| e.into_inner());
            if tables.contains_key(name) {
                return Err(Error::Execution(format!("Table '{name}' already exists")));
            }
            tables.insert(name.to_string(), Table::new(columns, primary_key));
            Ok(())
        })
    }

    fn tables(&self) -> Result<Vec<String>> {
        let tables = self.tables.read().unwrap_or_else(|e| e.into_inner());
        Ok(tables.keys().cloned().collect())
    }

    fn table_schema(&self, name: &str) -> Result<TableSchema> {
        let tables = self.tables.read().unwrap_or_else(|e| e.into_inner());
        tables
            .get(name)
            .map(|t| TableSchema {
                columns: t.columns.clone(),
                primary_key: t.primary_key.clone(),
            })
            .ok_or_else(|| Error::Execution(format!("Table '{name}' doesn't exist")))
    }

    fn insert_row<'a>(&'a self, table: &'a str, row: Vec<Value>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // Log-before-memory (PERFORMANCE_DURABILITY_PLAN.md D3): validate
            // under a read lock, append durably, *then* apply — not the other
            // way around. If the log append fails, nothing here has mutated
            // any state a reader could observe, so the row simply never
            // happened: no phantom row, no undo needed. Safe to validate
            // under only a read lock (rather than extending the write lock
            // across the log I/O the way `create_table` above has to)
            // because the caller already holds this table's exclusive lock
            // for the whole statement (`InMemoryStorage::lock_table`), so no
            // concurrent writer to this same table can appear between the
            // check and the apply below — and no lock is held across the
            // `.await` either way (PERFORMANCE_DURABILITY_PLAN.md PD-2).
            {
                let tables = self.tables.read().unwrap_or_else(|e| e.into_inner());
                let t = tables
                    .get(table)
                    .ok_or_else(|| Error::Execution(format!("Table '{table}' doesn't exist")))?;
                if row.len() != t.columns.len() {
                    return Err(Error::Execution(format!(
                        "Column count doesn't match value count: table '{table}' has {} column(s), got {}",
                        t.columns.len(),
                        row.len()
                    )));
                }
                t.check_insertable(&row)?;
            }

            #[cfg(test)]
            self.check_fault_injection()?;
            if let Some(writer) = &self.log_writer {
                writer.append_insert_row(table, &row).await?;
            }

            // Infallible: everything that could make it fail was already
            // checked above under the same guarantee that nothing could have
            // changed in between (see the comment above).
            let mut tables = self.tables.write().unwrap_or_else(|e| e.into_inner());
            let t = tables
                .get_mut(table)
                .ok_or_else(|| Error::Execution(format!("Table '{table}' doesn't exist")))?;
            t.push_trusted(row);
            Ok(())
        })
    }

    fn insert_rows(&self, rows: Vec<(String, Vec<Value>)>) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if rows.is_empty() {
                return Ok(());
            }

            // Same log-before-memory shape as `insert_row` above, just for a
            // whole batch: validate everything (including duplicate keys
            // *within* this batch, which `check_insertable` alone can't see
            // since it only ever looks at already-committed state) under a
            // read lock (released before the `.await` below), append the
            // whole batch as one durable record, then apply every row —
            // infallible, for the same reason `insert_row`'s apply is.
            {
                let tables = self.tables.read().unwrap_or_else(|e| e.into_inner());
                let mut seen_in_batch: HashMap<&str, HashSet<&Value>> = HashMap::new();
                for (table, row) in &rows {
                    let t = tables.get(table.as_str()).ok_or_else(|| {
                        Error::Execution(format!("Table '{table}' doesn't exist"))
                    })?;
                    if row.len() != t.columns.len() {
                        return Err(Error::Execution(format!(
                            "Column count doesn't match value count: table '{table}' has {} column(s), got {}",
                            t.columns.len(),
                            row.len()
                        )));
                    }
                    t.check_insertable(row)?;
                    if let Some(key) = t.primary_key_value(row) {
                        if !seen_in_batch.entry(table.as_str()).or_default().insert(key) {
                            return Err(Error::Execution(format!(
                                "Duplicate entry '{}' for key 'PRIMARY'",
                                key.to_display_string().unwrap_or_default()
                            )));
                        }
                    }
                }
            }

            #[cfg(test)]
            self.check_fault_injection()?;
            if let Some(writer) = &self.log_writer {
                writer.append_transaction(&rows).await?;
            }

            let mut tables = self.tables.write().unwrap_or_else(|e| e.into_inner());
            for (table, row) in rows {
                // Trusted for the same reason `insert_row`'s apply is; a
                // missing table here would mean one vanished between the
                // check above and here, which can't happen under the same
                // per-table-lock guarantee `insert_row` relies on.
                if let Some(t) = tables.get_mut(&table) {
                    t.push_trusted(row);
                }
            }
            Ok(())
        })
    }

    fn scan(&self, table: &str) -> Result<Vec<Vec<Value>>> {
        let tables = self.tables.read().unwrap_or_else(|e| e.into_inner());
        tables
            .get(table)
            .map(|t| t.rows.clone())
            .ok_or_else(|| Error::Execution(format!("Table '{table}' doesn't exist")))
    }

    fn scan_filtered(
        &self,
        table: &str,
        filter: &mut dyn FnMut(&[Value]) -> bool,
    ) -> Result<Vec<Vec<Value>>> {
        let tables = self.tables.read().unwrap_or_else(|e| e.into_inner());
        let t = tables
            .get(table)
            .ok_or_else(|| Error::Execution(format!("Table '{table}' doesn't exist")))?;
        // The clone happens only for rows `filter` accepts -- everything
        // else is inspected by reference and dropped, never copied
        // (PERFORMANCE_DURABILITY_PLAN.md P1).
        Ok(t.rows.iter().filter(|row| filter(row)).cloned().collect())
    }

    fn lookup_by_primary_key(&self, table: &str, key: &Value) -> Result<Option<Vec<Value>>> {
        let tables = self.tables.read().unwrap_or_else(|e| e.into_inner());
        let t = tables
            .get(table)
            .ok_or_else(|| Error::Execution(format!("Table '{table}' doesn't exist")))?;
        Ok(t.index.get(key).map(|&idx| t.rows[idx].clone()))
    }

    fn next_auto_increment(&self, table: &str) -> Result<i64> {
        let mut tables = self.tables.write().unwrap_or_else(|e| e.into_inner());
        let t = tables
            .get_mut(table)
            .ok_or_else(|| Error::Execution(format!("Table '{table}' doesn't exist")))?;
        let (_, next) = t.auto_increment.as_mut().ok_or_else(|| {
            Error::Execution(format!("Table '{table}' has no AUTO_INCREMENT column"))
        })?;
        let value = *next;
        *next += 1;
        Ok(value)
    }

    fn create_database(&self, name: &str, if_not_exists: bool) -> Result<()> {
        let mut databases = self.databases.write().unwrap_or_else(|e| e.into_inner());
        if databases.contains(name) {
            return if if_not_exists {
                Ok(())
            } else {
                Err(Error::Execution(format!(
                    "Can't create database '{name}'; database exists"
                )))
            };
        }
        databases.insert(name.to_string());
        Ok(())
    }

    fn drop_database(&self, name: &str, if_exists: bool) -> Result<()> {
        let mut databases = self.databases.write().unwrap_or_else(|e| e.into_inner());
        if databases.remove(name) || if_exists {
            Ok(())
        } else {
            Err(Error::Execution(format!(
                "Can't drop database '{name}'; database doesn't exist"
            )))
        }
    }

    fn databases(&self) -> Result<Vec<String>> {
        let databases = self.databases.read().unwrap_or_else(|e| e.into_inner());
        Ok(databases.iter().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::value::ColumnType;
    use std::sync::Mutex;

    fn col(name: &str, ty: ColumnType) -> ColumnSchema {
        ColumnSchema {
            name: name.to_string(),
            column_type: ty,
            nullable: true,
            auto_increment: false,
        }
    }

    #[tokio::test]
    async fn create_table_then_list_it() {
        let storage = InMemoryStorage::new();
        storage
            .create_table("t", vec![col("a", ColumnType::Int)], None)
            .await
            .unwrap();
        assert_eq!(storage.tables().unwrap(), vec!["t".to_string()]);
    }

    #[tokio::test]
    async fn create_table_rejects_duplicate() {
        let storage = InMemoryStorage::new();
        storage
            .create_table("t", vec![col("a", ColumnType::Int)], None)
            .await
            .unwrap();
        assert!(storage
            .create_table("t", vec![col("a", ColumnType::Int)], None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn create_table_rejects_primary_key_not_in_columns() {
        let storage = InMemoryStorage::new();
        assert!(storage
            .create_table(
                "t",
                vec![col("a", ColumnType::Int)],
                Some("bogus".to_string())
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn insert_and_scan_round_trips() {
        let storage = InMemoryStorage::new();
        storage
            .create_table(
                "t",
                vec![col("a", ColumnType::Int), col("b", ColumnType::Varchar)],
                None,
            )
            .await
            .unwrap();
        storage
            .insert_row("t", vec![Value::Int(1), Value::Varchar("x".to_string())])
            .await
            .unwrap();
        storage
            .insert_row("t", vec![Value::Null, Value::Varchar("y".to_string())])
            .await
            .unwrap();

        assert_eq!(
            storage.scan("t").unwrap(),
            vec![
                vec![Value::Int(1), Value::Varchar("x".to_string())],
                vec![Value::Null, Value::Varchar("y".to_string())],
            ]
        );
    }

    #[tokio::test]
    async fn scan_filtered_returns_only_matching_rows() {
        let storage = InMemoryStorage::new();
        storage
            .create_table("t", vec![col("id", ColumnType::Int)], None)
            .await
            .unwrap();
        for i in 0..10 {
            storage.insert_row("t", vec![Value::Int(i)]).await.unwrap();
        }

        let matched = storage
            .scan_filtered(
                "t",
                &mut |row| matches!(row[0], Value::Int(n) if n % 2 == 0),
            )
            .unwrap();
        assert_eq!(
            matched,
            vec![
                vec![Value::Int(0)],
                vec![Value::Int(2)],
                vec![Value::Int(4)],
                vec![Value::Int(6)],
                vec![Value::Int(8)],
            ]
        );
    }

    #[tokio::test]
    async fn scan_filtered_calls_the_filter_exactly_once_per_row_and_clones_only_matches() {
        let storage = InMemoryStorage::new();
        storage
            .create_table("t", vec![col("id", ColumnType::Int)], None)
            .await
            .unwrap();
        for i in 0..5 {
            storage.insert_row("t", vec![Value::Int(i)]).await.unwrap();
        }

        // PERFORMANCE_DURABILITY_PLAN.md P1's whole point: every row is
        // *inspected* (the filter runs once per row, same as a plain
        // scan-then-filter would), but only matching rows are ever cloned
        // into the result -- proven here by counting filter invocations
        // separately from the returned row count, rather than just
        // asserting the final answer is correct (which a naive
        // scan-then-filter would also produce).
        let mut calls = 0;
        let matched = storage
            .scan_filtered("t", &mut |row| {
                calls += 1;
                matches!(row[0], Value::Int(n) if n == 3)
            })
            .unwrap();
        assert_eq!(calls, 5, "filter must be invoked once per row in the table");
        assert_eq!(matched, vec![vec![Value::Int(3)]]);
    }

    #[tokio::test]
    async fn scan_filtered_errors_on_missing_table_not_panic() {
        let storage = InMemoryStorage::new();
        assert!(storage.scan_filtered("missing", &mut |_| true).is_err());
    }

    #[tokio::test]
    async fn insert_rejects_wrong_column_count() {
        let storage = InMemoryStorage::new();
        storage
            .create_table("t", vec![col("a", ColumnType::Int)], None)
            .await
            .unwrap();
        assert!(storage
            .insert_row("t", vec![Value::Int(1), Value::Int(2)])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn operations_on_missing_table_error_not_panic() {
        let storage = InMemoryStorage::new();
        assert!(storage.table_schema("missing").is_err());
        assert!(storage.insert_row("missing", vec![]).await.is_err());
        assert!(storage.scan("missing").is_err());
        assert!(storage
            .lookup_by_primary_key("missing", &Value::Int(1))
            .is_err());
    }

    #[test]
    fn create_database_then_list_it() {
        let storage = InMemoryStorage::new();
        storage.create_database("mydb", false).unwrap();
        assert_eq!(storage.databases().unwrap(), vec!["mydb".to_string()]);
    }

    #[test]
    fn create_database_rejects_duplicate_unless_if_not_exists() {
        let storage = InMemoryStorage::new();
        storage.create_database("mydb", false).unwrap();
        assert!(storage.create_database("mydb", false).is_err());
        storage.create_database("mydb", true).unwrap(); // no-op, not an error
    }

    #[test]
    fn drop_database_removes_it_and_rejects_missing_unless_if_exists() {
        let storage = InMemoryStorage::new();
        storage.create_database("mydb", false).unwrap();
        storage.drop_database("mydb", false).unwrap();
        assert!(storage.databases().unwrap().is_empty());

        assert!(storage.drop_database("mydb", false).is_err());
        storage.drop_database("mydb", true).unwrap(); // no-op, not an error
    }

    #[tokio::test]
    async fn primary_key_enforces_uniqueness() {
        let storage = InMemoryStorage::new();
        storage
            .create_table(
                "t",
                vec![col("id", ColumnType::Int)],
                Some("id".to_string()),
            )
            .await
            .unwrap();
        storage.insert_row("t", vec![Value::Int(1)]).await.unwrap();
        assert!(storage.insert_row("t", vec![Value::Int(1)]).await.is_err());
        storage.insert_row("t", vec![Value::Int(2)]).await.unwrap(); // distinct key still fine
    }

    #[tokio::test]
    async fn primary_key_lookup_finds_and_misses() {
        let storage = InMemoryStorage::new();
        storage
            .create_table(
                "t",
                vec![col("id", ColumnType::Int), col("name", ColumnType::Varchar)],
                Some("id".to_string()),
            )
            .await
            .unwrap();
        storage
            .insert_row(
                "t",
                vec![Value::Int(1), Value::Varchar("alice".to_string())],
            )
            .await
            .unwrap();

        assert_eq!(
            storage.lookup_by_primary_key("t", &Value::Int(1)).unwrap(),
            Some(vec![Value::Int(1), Value::Varchar("alice".to_string())])
        );
        assert_eq!(
            storage.lookup_by_primary_key("t", &Value::Int(99)).unwrap(),
            None
        );
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        static COUNTER: Mutex<u64> = Mutex::new(0);
        let mut counter = COUNTER.lock().unwrap_or_else(|e| e.into_inner());
        *counter += 1;
        std::env::temp_dir().join(format!(
            "mysql-rust-engine-test-{name}-{}-{}",
            std::process::id(),
            *counter
        ))
    }

    #[tokio::test]
    async fn failed_log_append_on_insert_leaves_no_trace_in_memory() {
        let path = temp_path("fault-injection-insert");
        std::fs::remove_file(&path).ok();
        let storage = InMemoryStorage::open(&path, SyncPolicy::Never).unwrap();
        storage
            .create_table(
                "t",
                vec![col("id", ColumnType::Int)],
                Some("id".to_string()),
            )
            .await
            .unwrap();
        storage.insert_row("t", vec![Value::Int(1)]).await.unwrap();

        storage.fail_next_log_write();
        let result = storage.insert_row("t", vec![Value::Int(2)]).await;
        assert!(
            result.is_err(),
            "a failed log append must surface as an error, not succeed silently"
        );

        assert_eq!(
            storage.scan("t").unwrap(),
            vec![vec![Value::Int(1)]],
            "the row from the failed insert must never become visible -- \
             log-then-apply, not apply-then-log"
        );
        assert!(
            storage
                .lookup_by_primary_key("t", &Value::Int(2))
                .unwrap()
                .is_none(),
            "a failed insert must not be reachable via the primary-key index either"
        );
        // The fault only fires once -- a retry with the same value must
        // succeed cleanly, proving the failed attempt left no phantom PK
        // entry that would incorrectly reject it as a duplicate.
        storage.insert_row("t", vec![Value::Int(2)]).await.unwrap();
        assert_eq!(storage.scan("t").unwrap().len(), 2);

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn failed_log_append_on_create_table_leaves_it_absent() {
        let path = temp_path("fault-injection-create");
        std::fs::remove_file(&path).ok();
        let storage = InMemoryStorage::open(&path, SyncPolicy::Never).unwrap();

        storage.fail_next_log_write();
        let result = storage
            .create_table("t", vec![col("id", ColumnType::Int)], None)
            .await;
        assert!(result.is_err());
        assert!(
            storage.tables().unwrap().is_empty(),
            "a table whose CREATE TABLE failed to log must not exist in memory either"
        );

        // The fault only fires once -- retrying must succeed cleanly.
        storage
            .create_table("t", vec![col("id", ColumnType::Int)], None)
            .await
            .unwrap();
        assert_eq!(storage.tables().unwrap(), vec!["t".to_string()]);

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn insert_rows_rejects_a_duplicate_key_within_the_batch_and_applies_none_of_it() {
        let storage = InMemoryStorage::new();
        storage
            .create_table(
                "t",
                vec![col("id", ColumnType::Int), col("name", ColumnType::Varchar)],
                Some("id".to_string()),
            )
            .await
            .unwrap();

        let result = storage
            .insert_rows(vec![
                (
                    "t".to_string(),
                    vec![Value::Int(1), Value::Varchar("alice".to_string())],
                ),
                (
                    "t".to_string(),
                    vec![Value::Int(2), Value::Varchar("bob".to_string())],
                ),
                (
                    "t".to_string(),
                    vec![Value::Int(1), Value::Varchar("carol".to_string())],
                ),
            ])
            .await;
        assert!(result.is_err());
        assert!(
            storage.scan("t").unwrap().is_empty(),
            "rows 1 and 2 (individually fine) must not survive a batch rejected for row 3's \
             duplicate key -- one batch, one outcome"
        );
    }

    #[tokio::test]
    async fn insert_rows_rejects_a_row_that_collides_with_already_committed_data() {
        let storage = InMemoryStorage::new();
        storage
            .create_table(
                "t",
                vec![col("id", ColumnType::Int)],
                Some("id".to_string()),
            )
            .await
            .unwrap();
        storage.insert_row("t", vec![Value::Int(1)]).await.unwrap();

        let result = storage
            .insert_rows(vec![
                ("t".to_string(), vec![Value::Int(2)]),
                ("t".to_string(), vec![Value::Int(1)]), // collides with the already-committed row
            ])
            .await;
        assert!(result.is_err());
        assert_eq!(
            storage.scan("t").unwrap(),
            vec![vec![Value::Int(1)]],
            "the batch's own row 2 must not have applied either"
        );
    }

    #[tokio::test]
    async fn failed_log_append_on_insert_rows_leaves_no_trace_of_the_whole_batch() {
        let path = temp_path("fault-injection-insert-rows");
        std::fs::remove_file(&path).ok();
        let storage = InMemoryStorage::open(&path, SyncPolicy::Never).unwrap();
        storage
            .create_table(
                "t",
                vec![col("id", ColumnType::Int)],
                Some("id".to_string()),
            )
            .await
            .unwrap();

        storage.fail_next_log_write();
        let result = storage
            .insert_rows(vec![
                ("t".to_string(), vec![Value::Int(1)]),
                ("t".to_string(), vec![Value::Int(2)]),
                ("t".to_string(), vec![Value::Int(3)]),
            ])
            .await;
        assert!(result.is_err());
        assert!(
            storage.scan("t").unwrap().is_empty(),
            "none of the batch's rows must be visible when the log append for the \
             whole batch fails"
        );

        // The fault only fires once -- retrying the same batch must
        // succeed cleanly, proving nothing was left half-applied.
        storage
            .insert_rows(vec![
                ("t".to_string(), vec![Value::Int(1)]),
                ("t".to_string(), vec![Value::Int(2)]),
                ("t".to_string(), vec![Value::Int(3)]),
            ])
            .await
            .unwrap();
        assert_eq!(storage.scan("t").unwrap().len(), 3);

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn data_survives_reopening_the_same_path() {
        let path = temp_path("persist");
        std::fs::remove_file(&path).ok();

        {
            let storage = InMemoryStorage::open(&path, SyncPolicy::Never).unwrap();
            storage
                .create_table(
                    "t",
                    vec![col("id", ColumnType::Int), col("name", ColumnType::Varchar)],
                    Some("id".to_string()),
                )
                .await
                .unwrap();
            storage
                .insert_row(
                    "t",
                    vec![Value::Int(1), Value::Varchar("alice".to_string())],
                )
                .await
                .unwrap();
            storage
                .insert_row("t", vec![Value::Int(2), Value::Varchar("bob".to_string())])
                .await
                .unwrap();
        } // `storage` (and its log file handle) dropped here — simulates a restart.

        let reopened = InMemoryStorage::open(&path, SyncPolicy::Never).unwrap();
        assert_eq!(reopened.tables().unwrap(), vec!["t".to_string()]);
        assert_eq!(
            reopened.scan("t").unwrap(),
            vec![
                vec![Value::Int(1), Value::Varchar("alice".to_string())],
                vec![Value::Int(2), Value::Varchar("bob".to_string())],
            ]
        );
        // The primary-key index is correctly rebuilt from the replayed rows.
        assert_eq!(
            reopened.lookup_by_primary_key("t", &Value::Int(2)).unwrap(),
            Some(vec![Value::Int(2), Value::Varchar("bob".to_string())])
        );

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn decimal_and_date_columns_survive_reopening() {
        let path = temp_path("persist-decimal-date");
        std::fs::remove_file(&path).ok();

        {
            let storage = InMemoryStorage::open(&path, SyncPolicy::Never).unwrap();
            storage
                .create_table(
                    "orders",
                    vec![
                        ColumnSchema {
                            name: "total".to_string(),
                            column_type: ColumnType::Decimal(2),
                            nullable: true,
                            auto_increment: false,
                        },
                        ColumnSchema {
                            name: "placed_on".to_string(),
                            column_type: ColumnType::Date,
                            nullable: true,
                            auto_increment: false,
                        },
                    ],
                    None,
                )
                .await
                .unwrap();
            storage
                .insert_row(
                    "orders",
                    vec![
                        Value::Decimal(4999, 2),
                        Value::Date("2024-06-01".to_string()),
                    ],
                )
                .await
                .unwrap();
        } // dropped here — simulates a restart.

        let reopened = InMemoryStorage::open(&path, SyncPolicy::Never).unwrap();
        assert_eq!(
            reopened.scan("orders").unwrap(),
            vec![vec![
                Value::Decimal(4999, 2),
                Value::Date("2024-06-01".to_string()),
            ]]
        );

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn reopening_still_enforces_primary_key_uniqueness() {
        let path = temp_path("persist-pk");
        std::fs::remove_file(&path).ok();

        {
            let storage = InMemoryStorage::open(&path, SyncPolicy::Never).unwrap();
            storage
                .create_table(
                    "t",
                    vec![col("id", ColumnType::Int)],
                    Some("id".to_string()),
                )
                .await
                .unwrap();
            storage.insert_row("t", vec![Value::Int(1)]).await.unwrap();
        }

        let reopened = InMemoryStorage::open(&path, SyncPolicy::Never).unwrap();
        assert!(reopened.insert_row("t", vec![Value::Int(1)]).await.is_err());

        std::fs::remove_file(&path).ok();
    }

    fn auto_increment_col(name: &str) -> ColumnSchema {
        ColumnSchema {
            name: name.to_string(),
            column_type: ColumnType::Int,
            nullable: false,
            auto_increment: true,
        }
    }

    #[tokio::test]
    async fn next_auto_increment_returns_sequential_values() {
        let storage = InMemoryStorage::new();
        storage
            .create_table("t", vec![auto_increment_col("id")], Some("id".to_string()))
            .await
            .unwrap();
        assert_eq!(storage.next_auto_increment("t").unwrap(), 1);
        assert_eq!(storage.next_auto_increment("t").unwrap(), 2);
        assert_eq!(storage.next_auto_increment("t").unwrap(), 3);
    }

    #[tokio::test]
    async fn next_auto_increment_errors_on_missing_table_or_no_such_column() {
        let storage = InMemoryStorage::new();
        assert!(storage.next_auto_increment("missing").is_err());

        storage
            .create_table("t", vec![col("a", ColumnType::Int)], None)
            .await
            .unwrap();
        assert!(
            storage.next_auto_increment("t").is_err(),
            "table has no AUTO_INCREMENT column"
        );
    }

    #[tokio::test]
    async fn inserting_an_explicit_value_advances_the_auto_increment_counter() {
        let storage = InMemoryStorage::new();
        storage
            .create_table("t", vec![auto_increment_col("id")], Some("id".to_string()))
            .await
            .unwrap();
        storage.insert_row("t", vec![Value::Int(41)]).await.unwrap();
        // The counter must jump past the manually-inserted value, not
        // collide with it.
        assert_eq!(storage.next_auto_increment("t").unwrap(), 42);
    }

    #[tokio::test]
    async fn auto_increment_sequence_continues_correctly_after_reopening() {
        let path = temp_path("persist-auto-increment");
        std::fs::remove_file(&path).ok();

        {
            let storage = InMemoryStorage::open(&path, SyncPolicy::Never).unwrap();
            storage
                .create_table("t", vec![auto_increment_col("id")], Some("id".to_string()))
                .await
                .unwrap();
            storage.insert_row("t", vec![Value::Int(1)]).await.unwrap();
            storage.insert_row("t", vec![Value::Int(2)]).await.unwrap();
        }

        // Reopening must replay the rows and pick the counter up from the
        // largest value actually present, not restart it from 1 — otherwise
        // a fresh auto-assigned insert would collide with an existing row.
        let reopened = InMemoryStorage::open(&path, SyncPolicy::Never).unwrap();
        assert_eq!(reopened.next_auto_increment("t").unwrap(), 3);

        std::fs::remove_file(&path).ok();
    }
}
