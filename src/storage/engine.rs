//! An in-memory storage backend with an optional on-disk log for
//! persistence (see `storage::log`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use crate::storage::log::{Entry, Log};
use crate::storage::value::{ColumnSchema, TableSchema, Value};
use crate::storage::Storage;
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
}

impl Table {
    fn new(columns: Vec<ColumnSchema>, primary_key: Option<String>) -> Self {
        let primary_key_index = primary_key
            .as_ref()
            .and_then(|pk| columns.iter().position(|c| &c.name == pk));
        Table {
            columns,
            primary_key,
            primary_key_index,
            rows: Vec::new(),
            index: HashMap::new(),
        }
    }

    /// Append a row without checking primary-key uniqueness — used to
    /// replay already-validated entries from the log.
    fn push_trusted(&mut self, row: Vec<Value>) {
        if let Some(idx) = self.primary_key_index {
            self.index.insert(row[idx].clone(), self.rows.len());
        }
        self.rows.push(row);
    }

    fn insert_checked(&mut self, row: Vec<Value>) -> Result<()> {
        if let Some(idx) = self.primary_key_index {
            if self.index.contains_key(&row[idx]) {
                return Err(Error::Execution(format!(
                    "Duplicate entry '{}' for key 'PRIMARY'",
                    row[idx].to_display_string().unwrap_or_default()
                )));
            }
        }
        self.push_trusted(row);
        Ok(())
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
    log: Mutex<Option<Log>>,
    /// One dedicated async mutex per table, handed out by [`Self::lock_table`]
    /// so a transaction (see `storage::transaction`) can hold a table's
    /// write lock across multiple statements/await points — something a
    /// `std::sync::Mutex` guard cannot safely do. This registry mutex
    /// itself is only ever held for the instant it takes to look up or
    /// insert an `Arc`, never across an `.await`.
    table_locks: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl InMemoryStorage {
    /// A purely in-memory store; nothing is persisted.
    pub fn new() -> Self {
        InMemoryStorage::default()
    }

    /// Open (creating if necessary) a store backed by a log file at `path`.
    /// Any existing data is replayed into memory immediately.
    pub fn open(path: &Path) -> Result<Self> {
        let mut tables: HashMap<String, Table> = HashMap::new();
        let log = Log::open(path, |entry| apply_entry(&mut tables, entry))?;
        Ok(InMemoryStorage {
            tables: RwLock::new(tables),
            log: Mutex::new(Some(log)),
            table_locks: tokio::sync::Mutex::new(HashMap::new()),
        })
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
    pub fn open_in_dir(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        Self::open(&dir.join("data.log"))
    }

    fn append_log(&self, write: impl FnOnce(&mut Log) -> Result<()>) -> Result<()> {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        match log.as_mut() {
            Some(l) => write(l),
            None => Ok(()),
        }
    }
}

impl Storage for InMemoryStorage {
    fn create_table(
        &self,
        name: &str,
        columns: Vec<ColumnSchema>,
        primary_key: Option<String>,
    ) -> Result<()> {
        if let Some(pk) = &primary_key {
            if !columns.iter().any(|c| &c.name == pk) {
                return Err(Error::Execution(format!(
                    "Primary key column '{pk}' is not defined"
                )));
            }
        }

        {
            let mut tables = self.tables.write().unwrap_or_else(|e| e.into_inner());
            if tables.contains_key(name) {
                return Err(Error::Execution(format!("Table '{name}' already exists")));
            }
            tables.insert(
                name.to_string(),
                Table::new(columns.clone(), primary_key.clone()),
            );
        }

        self.append_log(|log| log.append_create_table(name, &columns, primary_key.as_deref()))
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

    fn insert_row(&self, table: &str, row: Vec<Value>) -> Result<()> {
        {
            let mut tables = self.tables.write().unwrap_or_else(|e| e.into_inner());
            let t = tables
                .get_mut(table)
                .ok_or_else(|| Error::Execution(format!("Table '{table}' doesn't exist")))?;
            if row.len() != t.columns.len() {
                return Err(Error::Execution(format!(
                    "Column count doesn't match value count: table '{table}' has {} column(s), got {}",
                    t.columns.len(),
                    row.len()
                )));
            }
            t.insert_checked(row.clone())?;
        }

        self.append_log(|log| log.append_insert_row(table, &row))
    }

    fn scan(&self, table: &str) -> Result<Vec<Vec<Value>>> {
        let tables = self.tables.read().unwrap_or_else(|e| e.into_inner());
        tables
            .get(table)
            .map(|t| t.rows.clone())
            .ok_or_else(|| Error::Execution(format!("Table '{table}' doesn't exist")))
    }

    fn lookup_by_primary_key(&self, table: &str, key: &Value) -> Result<Option<Vec<Value>>> {
        let tables = self.tables.read().unwrap_or_else(|e| e.into_inner());
        let t = tables
            .get(table)
            .ok_or_else(|| Error::Execution(format!("Table '{table}' doesn't exist")))?;
        Ok(t.index.get(key).map(|&idx| t.rows[idx].clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::value::ColumnType;

    fn col(name: &str, ty: ColumnType) -> ColumnSchema {
        ColumnSchema {
            name: name.to_string(),
            column_type: ty,
        }
    }

    #[test]
    fn create_table_then_list_it() {
        let storage = InMemoryStorage::new();
        storage
            .create_table("t", vec![col("a", ColumnType::Int)], None)
            .unwrap();
        assert_eq!(storage.tables().unwrap(), vec!["t".to_string()]);
    }

    #[test]
    fn create_table_rejects_duplicate() {
        let storage = InMemoryStorage::new();
        storage
            .create_table("t", vec![col("a", ColumnType::Int)], None)
            .unwrap();
        assert!(storage
            .create_table("t", vec![col("a", ColumnType::Int)], None)
            .is_err());
    }

    #[test]
    fn create_table_rejects_primary_key_not_in_columns() {
        let storage = InMemoryStorage::new();
        assert!(storage
            .create_table(
                "t",
                vec![col("a", ColumnType::Int)],
                Some("bogus".to_string())
            )
            .is_err());
    }

    #[test]
    fn insert_and_scan_round_trips() {
        let storage = InMemoryStorage::new();
        storage
            .create_table(
                "t",
                vec![col("a", ColumnType::Int), col("b", ColumnType::Varchar)],
                None,
            )
            .unwrap();
        storage
            .insert_row("t", vec![Value::Int(1), Value::Varchar("x".to_string())])
            .unwrap();
        storage
            .insert_row("t", vec![Value::Null, Value::Varchar("y".to_string())])
            .unwrap();

        assert_eq!(
            storage.scan("t").unwrap(),
            vec![
                vec![Value::Int(1), Value::Varchar("x".to_string())],
                vec![Value::Null, Value::Varchar("y".to_string())],
            ]
        );
    }

    #[test]
    fn insert_rejects_wrong_column_count() {
        let storage = InMemoryStorage::new();
        storage
            .create_table("t", vec![col("a", ColumnType::Int)], None)
            .unwrap();
        assert!(storage
            .insert_row("t", vec![Value::Int(1), Value::Int(2)])
            .is_err());
    }

    #[test]
    fn operations_on_missing_table_error_not_panic() {
        let storage = InMemoryStorage::new();
        assert!(storage.table_schema("missing").is_err());
        assert!(storage.insert_row("missing", vec![]).is_err());
        assert!(storage.scan("missing").is_err());
        assert!(storage
            .lookup_by_primary_key("missing", &Value::Int(1))
            .is_err());
    }

    #[test]
    fn primary_key_enforces_uniqueness() {
        let storage = InMemoryStorage::new();
        storage
            .create_table(
                "t",
                vec![col("id", ColumnType::Int)],
                Some("id".to_string()),
            )
            .unwrap();
        storage.insert_row("t", vec![Value::Int(1)]).unwrap();
        assert!(storage.insert_row("t", vec![Value::Int(1)]).is_err());
        storage.insert_row("t", vec![Value::Int(2)]).unwrap(); // distinct key still fine
    }

    #[test]
    fn primary_key_lookup_finds_and_misses() {
        let storage = InMemoryStorage::new();
        storage
            .create_table(
                "t",
                vec![col("id", ColumnType::Int), col("name", ColumnType::Varchar)],
                Some("id".to_string()),
            )
            .unwrap();
        storage
            .insert_row(
                "t",
                vec![Value::Int(1), Value::Varchar("alice".to_string())],
            )
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

    #[test]
    fn data_survives_reopening_the_same_path() {
        let path = temp_path("persist");
        std::fs::remove_file(&path).ok();

        {
            let storage = InMemoryStorage::open(&path).unwrap();
            storage
                .create_table(
                    "t",
                    vec![col("id", ColumnType::Int), col("name", ColumnType::Varchar)],
                    Some("id".to_string()),
                )
                .unwrap();
            storage
                .insert_row(
                    "t",
                    vec![Value::Int(1), Value::Varchar("alice".to_string())],
                )
                .unwrap();
            storage
                .insert_row("t", vec![Value::Int(2), Value::Varchar("bob".to_string())])
                .unwrap();
        } // `storage` (and its log file handle) dropped here — simulates a restart.

        let reopened = InMemoryStorage::open(&path).unwrap();
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

    #[test]
    fn reopening_still_enforces_primary_key_uniqueness() {
        let path = temp_path("persist-pk");
        std::fs::remove_file(&path).ok();

        {
            let storage = InMemoryStorage::open(&path).unwrap();
            storage
                .create_table(
                    "t",
                    vec![col("id", ColumnType::Int)],
                    Some("id".to_string()),
                )
                .unwrap();
            storage.insert_row("t", vec![Value::Int(1)]).unwrap();
        }

        let reopened = InMemoryStorage::open(&path).unwrap();
        assert!(reopened.insert_row("t", vec![Value::Int(1)]).is_err());

        std::fs::remove_file(&path).ok();
    }
}
