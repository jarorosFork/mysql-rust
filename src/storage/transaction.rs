//! A connection-scoped transaction: buffers writes until `commit`, discards
//! them on `rollback`, and holds each written table's exclusive lock for
//! the transaction's whole lifetime.
//!
//! Isolation level: **read committed**. A transaction always sees its own
//! writes layered on top of the latest committed state (never a stale
//! snapshot), and other connections never see this transaction's writes
//! until `commit` — reads never block (no read locks), only writers
//! serialize against each other, one table-level lock at a time. This is
//! the minimum level the roadmap calls for; `REPEATABLE READ`/`SERIALIZABLE`
//! would need real MVCC (row versioning), which is out of scope here.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tokio::sync::OwnedMutexGuard;

use crate::storage::engine::InMemoryStorage;
use crate::storage::value::{ColumnSchema, TableSchema, Value};
use crate::storage::{BoxFuture, Storage};
use crate::{Error, Result};

/// An in-progress transaction. Logically owned by exactly one `Connection`,
/// but its fields still need `std::sync::Mutex` rather than a plain
/// `RefCell`: an `async fn(&self)` (`ensure_locked`, below) that awaits
/// while holding `&self` requires `Self: Sync` for the enclosing future to
/// be `Send` — `RefCell` can never satisfy that, regardless of actual
/// access patterns. The mutexes themselves are only ever held for a plain
/// synchronous field update, never across an `.await`.
pub struct Transaction {
    storage: Arc<InMemoryStorage>,
    /// Rows inserted by this transaction but not yet committed, per table.
    pending: Mutex<HashMap<String, Vec<Vec<Value>>>>,
    /// Tables this transaction has written to, each holding that table's
    /// write lock until `commit`/`rollback` (i.e. until `self` is dropped
    /// or consumed by one of those methods).
    locks: Mutex<Vec<(String, OwnedMutexGuard<()>)>>,
    locked_tables: Mutex<HashSet<String>>,
}

impl Transaction {
    pub fn new(storage: Arc<InMemoryStorage>) -> Self {
        Transaction {
            storage,
            pending: Mutex::new(HashMap::new()),
            locks: Mutex::new(Vec::new()),
            locked_tables: Mutex::new(HashSet::new()),
        }
    }

    /// Acquire `table`'s write lock if this transaction doesn't already
    /// hold it, blocking (bounded by `InMemoryStorage::lock_table`'s
    /// timeout) until it's available. Idempotent per table.
    pub async fn ensure_locked(&self, table: &str) -> Result<()> {
        if self
            .locked_tables
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(table)
        {
            return Ok(());
        }
        let guard = self.storage.lock_table(table).await?;
        self.locked_tables
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(table.to_string());
        self.locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((table.to_string(), guard));
        Ok(())
    }

    /// Apply every buffered row to the real storage, as one atomic batch
    /// (PERFORMANCE_DURABILITY_PLAN.md D2 — see `InMemoryStorage::
    /// insert_rows`), and release all locks. Held locks guarantee nothing
    /// else could have changed these tables since they were buffered, so
    /// this cannot fail on a conflict it didn't already check for at
    /// insert time.
    pub async fn commit(self) -> Result<()> {
        let pending = self.pending.into_inner().unwrap_or_else(|e| e.into_inner());
        let rows: Vec<(String, Vec<Value>)> = pending
            .into_iter()
            .flat_map(|(table, rows)| rows.into_iter().map(move |row| (table.clone(), row)))
            .collect();
        self.storage.insert_rows(rows).await // locks release as `self.locks` drops here.
    }

    /// Discard every buffered row and release all locks. Nothing was ever
    /// applied to the real storage, so there's nothing to undo.
    pub fn rollback(self) {}

    fn primary_key_index(&self, schema: &TableSchema) -> Option<usize> {
        let pk = schema.primary_key.as_ref()?;
        schema.columns.iter().position(|c| &c.name == pk)
    }
}

impl Storage for Transaction {
    fn create_table<'a>(
        &'a self,
        name: &'a str,
        columns: Vec<ColumnSchema>,
        primary_key: Option<String>,
    ) -> BoxFuture<'a, Result<()>> {
        // DDL auto-commits immediately, even inside a transaction — matches
        // real MySQL (CREATE TABLE implicitly commits any open transaction
        // first); we don't go quite that far, but we don't buffer it either.
        Box::pin(async move { self.storage.create_table(name, columns, primary_key).await })
    }

    fn tables(&self) -> Result<Vec<String>> {
        self.storage.tables()
    }

    fn table_schema(&self, name: &str) -> Result<TableSchema> {
        self.storage.table_schema(name)
    }

    fn insert_row<'a>(&'a self, table: &'a str, row: Vec<Value>) -> BoxFuture<'a, Result<()>> {
        // Purely in-memory buffering — no log I/O, so nothing here actually
        // needs to suspend; this is `async` only because the `Storage`
        // trait's signature is (every implementor must match, since it's
        // used as `&dyn Storage` — see `storage::BoxFuture`'s doc comment).
        Box::pin(async move {
            let schema = self.storage.table_schema(table)?;
            if row.len() != schema.columns.len() {
                return Err(Error::Execution(format!(
                    "Column count doesn't match value count: table '{table}' has {} column(s), got {}",
                    schema.columns.len(),
                    row.len()
                )));
            }

            let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(pk_idx) = self.primary_key_index(&schema) {
                let key = &row[pk_idx];
                let committed = self.storage.lookup_by_primary_key(table, key)?.is_some();
                let already_pending = pending
                    .get(table)
                    .is_some_and(|rows| rows.iter().any(|r| &r[pk_idx] == key));
                if committed || already_pending {
                    return Err(Error::Execution(format!(
                        "Duplicate entry '{}' for key 'PRIMARY'",
                        key.to_display_string().unwrap_or_default()
                    )));
                }
            }
            drop(pending);

            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(table.to_string())
                .or_default()
                .push(row);
            Ok(())
        })
    }

    fn scan(&self, table: &str) -> Result<Vec<Vec<Value>>> {
        let mut rows = self.storage.scan(table)?;
        if let Some(pending) = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(table)
        {
            rows.extend(pending.iter().cloned());
        }
        Ok(rows)
    }

    fn lookup_by_primary_key(&self, table: &str, key: &Value) -> Result<Option<Vec<Value>>> {
        if let Some(row) = self.storage.lookup_by_primary_key(table, key)? {
            return Ok(Some(row));
        }
        let schema = self.storage.table_schema(table)?;
        let Some(pk_idx) = self.primary_key_index(&schema) else {
            return Ok(None);
        };
        Ok(self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(table)
            .and_then(|rows| rows.iter().find(|r| &r[pk_idx] == key).cloned()))
    }

    // Real MySQL/InnoDB never rolls back a reserved AUTO_INCREMENT value
    // either — go straight to the real storage, same as create_table.
    fn next_auto_increment(&self, table: &str) -> Result<i64> {
        self.storage.next_auto_increment(table)
    }

    // Database-name registration isn't part of the buffered-write model at
    // all (unlike table rows) — same as `create_table`, it goes straight to
    // the real storage, "auto-committing" immediately.
    fn create_database(&self, name: &str, if_not_exists: bool) -> Result<()> {
        self.storage.create_database(name, if_not_exists)
    }

    fn drop_database(&self, name: &str, if_exists: bool) -> Result<()> {
        self.storage.drop_database(name, if_exists)
    }

    fn databases(&self) -> Result<Vec<String>> {
        self.storage.databases()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::value::ColumnType;

    async fn setup() -> Arc<InMemoryStorage> {
        let storage = Arc::new(InMemoryStorage::new());
        storage
            .create_table(
                "t",
                vec![
                    ColumnSchema {
                        name: "id".to_string(),
                        column_type: ColumnType::Int,
                        nullable: false,
                        auto_increment: false,
                    },
                    ColumnSchema {
                        name: "name".to_string(),
                        column_type: ColumnType::Varchar,
                        nullable: true,
                        auto_increment: false,
                    },
                ],
                Some("id".to_string()),
            )
            .await
            .unwrap();
        storage
    }

    #[tokio::test]
    async fn transaction_sees_its_own_pending_writes() {
        let storage = setup().await;
        let tx = Transaction::new(Arc::clone(&storage));
        tx.ensure_locked("t").await.unwrap();
        tx.insert_row(
            "t",
            vec![Value::Int(1), Value::Varchar("alice".to_string())],
        )
        .await
        .unwrap();

        assert_eq!(
            tx.scan("t").unwrap().len(),
            1,
            "the transaction should see its own uncommitted row"
        );
    }

    #[tokio::test]
    async fn other_readers_do_not_see_pending_writes() {
        let storage = setup().await;
        let tx = Transaction::new(Arc::clone(&storage));
        tx.ensure_locked("t").await.unwrap();
        tx.insert_row(
            "t",
            vec![Value::Int(1), Value::Varchar("alice".to_string())],
        )
        .await
        .unwrap();

        // Reading straight from storage (as any other connection would) —
        // read committed: nothing is visible until commit.
        assert!(storage.scan("t").unwrap().is_empty());
    }

    #[tokio::test]
    async fn commit_applies_pending_rows_to_storage() {
        let storage = setup().await;
        let tx = Transaction::new(Arc::clone(&storage));
        tx.ensure_locked("t").await.unwrap();
        tx.insert_row(
            "t",
            vec![Value::Int(1), Value::Varchar("alice".to_string())],
        )
        .await
        .unwrap();

        tx.commit().await.unwrap();

        assert_eq!(
            storage.scan("t").unwrap(),
            vec![vec![Value::Int(1), Value::Varchar("alice".to_string())]]
        );
    }

    #[tokio::test]
    async fn commit_applies_rows_across_multiple_tables_as_one_atomic_batch() {
        let storage = setup().await;
        storage
            .create_table(
                "u",
                vec![ColumnSchema {
                    name: "id".to_string(),
                    column_type: ColumnType::Int,
                    nullable: false,
                    auto_increment: false,
                }],
                Some("id".to_string()),
            )
            .await
            .unwrap();

        let tx = Transaction::new(Arc::clone(&storage));
        tx.ensure_locked("t").await.unwrap();
        tx.ensure_locked("u").await.unwrap();
        tx.insert_row(
            "t",
            vec![Value::Int(1), Value::Varchar("alice".to_string())],
        )
        .await
        .unwrap();
        tx.insert_row("u", vec![Value::Int(100)]).await.unwrap();
        tx.insert_row("u", vec![Value::Int(101)]).await.unwrap();

        tx.commit().await.unwrap();

        // PERFORMANCE_DURABILITY_PLAN.md D2: one log record for the whole
        // transaction, regardless of how many tables it touched -- all of
        // it lands (or, on a crash, none of it does; see tests/crash.rs).
        assert_eq!(
            storage.scan("t").unwrap(),
            vec![vec![Value::Int(1), Value::Varchar("alice".to_string())]]
        );
        assert_eq!(
            storage.scan("u").unwrap(),
            vec![vec![Value::Int(100)], vec![Value::Int(101)]]
        );
    }

    #[tokio::test]
    async fn rollback_discards_pending_rows() {
        let storage = setup().await;
        let tx = Transaction::new(Arc::clone(&storage));
        tx.ensure_locked("t").await.unwrap();
        tx.insert_row(
            "t",
            vec![Value::Int(1), Value::Varchar("alice".to_string())],
        )
        .await
        .unwrap();

        tx.rollback();

        assert!(
            storage.scan("t").unwrap().is_empty(),
            "rollback must leave storage untouched"
        );
    }

    #[tokio::test]
    async fn duplicate_primary_key_within_the_same_transaction_is_rejected() {
        let storage = setup().await;
        let tx = Transaction::new(Arc::clone(&storage));
        tx.ensure_locked("t").await.unwrap();
        tx.insert_row(
            "t",
            vec![Value::Int(1), Value::Varchar("alice".to_string())],
        )
        .await
        .unwrap();
        assert!(tx
            .insert_row("t", vec![Value::Int(1), Value::Varchar("bob".to_string())])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn duplicate_primary_key_against_already_committed_data_is_rejected() {
        let storage = setup().await;
        storage
            .insert_row(
                "t",
                vec![Value::Int(1), Value::Varchar("alice".to_string())],
            )
            .await
            .unwrap();

        let tx = Transaction::new(Arc::clone(&storage));
        tx.ensure_locked("t").await.unwrap();
        assert!(tx
            .insert_row("t", vec![Value::Int(1), Value::Varchar("bob".to_string())])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn ensure_locked_is_idempotent_within_one_transaction() {
        // A non-reentrant lock acquired twice by the same "owner" without a
        // guard would deadlock — this is exactly what multiple INSERTs into
        // the same table within one transaction do in practice.
        let storage = setup().await;
        let tx = Transaction::new(Arc::clone(&storage));
        tx.ensure_locked("t").await.unwrap();
        tx.ensure_locked("t").await.unwrap(); // must not hang
        tx.insert_row(
            "t",
            vec![Value::Int(1), Value::Varchar("alice".to_string())],
        )
        .await
        .unwrap();
        tx.insert_row("t", vec![Value::Int(2), Value::Varchar("bob".to_string())])
            .await
            .unwrap();
        assert_eq!(tx.scan("t").unwrap().len(), 2);
    }

    #[tokio::test]
    async fn lookup_by_primary_key_checks_pending_too() {
        let storage = setup().await;
        let tx = Transaction::new(Arc::clone(&storage));
        tx.ensure_locked("t").await.unwrap();
        tx.insert_row(
            "t",
            vec![Value::Int(1), Value::Varchar("alice".to_string())],
        )
        .await
        .unwrap();

        assert_eq!(
            tx.lookup_by_primary_key("t", &Value::Int(1)).unwrap(),
            Some(vec![Value::Int(1), Value::Varchar("alice".to_string())])
        );
        assert_eq!(
            tx.lookup_by_primary_key("t", &Value::Int(99)).unwrap(),
            None
        );
    }
}
