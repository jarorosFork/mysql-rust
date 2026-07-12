//! A minimal write-ahead log for on-disk persistence.
//!
//! Every mutation (`CREATE TABLE`, row insert) is appended as one
//! length-prefixed entry; reopening a data file replays every entry in
//! order to rebuild the in-memory state. This is deliberately simple —
//! no checkpointing/compaction, no fsync durability guarantees beyond what
//! `File::write_all` gives — but it satisfies "data written before shutdown
//! is present after restart" (ROADMAP.md Phase 5) without the complexity of
//! a production WAL, which the roadmap explicitly allows ("write-ahead or
//! file-backed").

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::storage::value::{ColumnSchema, ColumnType, Value};
use crate::{Error, Result};

const TAG_CREATE_TABLE: u8 = 1;
const TAG_INSERT_ROW: u8 = 2;

const VALUE_NULL: u8 = 0;
const VALUE_INT: u8 = 1;
const VALUE_VARCHAR: u8 = 2;

const COLUMN_TYPE_INT: u8 = 0;
const COLUMN_TYPE_VARCHAR: u8 = 1;

/// One replayed operation from the log.
pub enum Entry {
    CreateTable {
        table: String,
        columns: Vec<ColumnSchema>,
        primary_key: Option<String>,
    },
    InsertRow {
        table: String,
        row: Vec<Value>,
    },
}

fn write_u32(buf: &mut Vec<u8>, n: u32) {
    buf.extend_from_slice(&n.to_le_bytes());
}

fn write_string(buf: &mut Vec<u8>, s: &str) {
    write_u32(buf, s.len() as u32);
    buf.extend_from_slice(s.as_bytes());
}

fn write_value(buf: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => buf.push(VALUE_NULL),
        Value::Int(n) => {
            buf.push(VALUE_INT);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        Value::Varchar(s) => {
            buf.push(VALUE_VARCHAR);
            write_string(buf, s);
        }
    }
}

fn encode_create_table(
    table: &str,
    columns: &[ColumnSchema],
    primary_key: Option<&str>,
) -> Vec<u8> {
    let mut buf = vec![TAG_CREATE_TABLE];
    write_string(&mut buf, table);
    match primary_key {
        Some(pk) => {
            buf.push(1);
            write_string(&mut buf, pk);
        }
        None => buf.push(0),
    }
    write_u32(&mut buf, columns.len() as u32);
    for col in columns {
        write_string(&mut buf, &col.name);
        buf.push(match col.column_type {
            ColumnType::Int => COLUMN_TYPE_INT,
            ColumnType::Varchar => COLUMN_TYPE_VARCHAR,
        });
    }
    buf
}

fn encode_insert_row(table: &str, row: &[Value]) -> Vec<u8> {
    let mut buf = vec![TAG_INSERT_ROW];
    write_string(&mut buf, table);
    write_u32(&mut buf, row.len() as u32);
    for value in row {
        write_value(&mut buf, value);
    }
    buf
}

fn corrupt(context: &str) -> Error {
    Error::Execution(format!("corrupt data file: {context}"))
}

fn read_u32(bytes: &[u8], pos: &mut usize) -> Result<u32> {
    let end = pos
        .checked_add(4)
        .ok_or_else(|| corrupt("length overflow"))?;
    let slice = bytes
        .get(*pos..end)
        .ok_or_else(|| corrupt("truncated u32"))?;
    let arr = [slice[0], slice[1], slice[2], slice[3]];
    *pos = end;
    Ok(u32::from_le_bytes(arr))
}

fn read_i64(bytes: &[u8], pos: &mut usize) -> Result<i64> {
    let end = pos
        .checked_add(8)
        .ok_or_else(|| corrupt("length overflow"))?;
    let slice = bytes
        .get(*pos..end)
        .ok_or_else(|| corrupt("truncated i64"))?;
    let arr: [u8; 8] = [
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ];
    *pos = end;
    Ok(i64::from_le_bytes(arr))
}

fn read_byte(bytes: &[u8], pos: &mut usize) -> Result<u8> {
    let b = *bytes
        .get(*pos)
        .ok_or_else(|| corrupt("truncated tag byte"))?;
    *pos += 1;
    Ok(b)
}

fn read_string(bytes: &[u8], pos: &mut usize) -> Result<String> {
    let len = read_u32(bytes, pos)? as usize;
    let end = pos
        .checked_add(len)
        .ok_or_else(|| corrupt("length overflow"))?;
    let slice = bytes
        .get(*pos..end)
        .ok_or_else(|| corrupt("truncated string"))?;
    let s = String::from_utf8(slice.to_vec()).map_err(|_| corrupt("invalid utf-8"))?;
    *pos = end;
    Ok(s)
}

fn read_value(bytes: &[u8], pos: &mut usize) -> Result<Value> {
    match read_byte(bytes, pos)? {
        VALUE_NULL => Ok(Value::Null),
        VALUE_INT => Ok(Value::Int(read_i64(bytes, pos)?)),
        VALUE_VARCHAR => Ok(Value::Varchar(read_string(bytes, pos)?)),
        other => Err(corrupt(&format!("unknown value tag {other}"))),
    }
}

fn decode_entry(bytes: &[u8]) -> Result<Entry> {
    let mut pos = 0;
    match read_byte(bytes, &mut pos)? {
        TAG_CREATE_TABLE => {
            let table = read_string(bytes, &mut pos)?;
            let primary_key = match read_byte(bytes, &mut pos)? {
                0 => None,
                1 => Some(read_string(bytes, &mut pos)?),
                other => return Err(corrupt(&format!("unknown primary-key flag {other}"))),
            };
            let column_count = read_u32(bytes, &mut pos)? as usize;
            let mut columns = Vec::with_capacity(column_count);
            for _ in 0..column_count {
                let name = read_string(bytes, &mut pos)?;
                let column_type = match read_byte(bytes, &mut pos)? {
                    COLUMN_TYPE_INT => ColumnType::Int,
                    COLUMN_TYPE_VARCHAR => ColumnType::Varchar,
                    other => return Err(corrupt(&format!("unknown column type tag {other}"))),
                };
                columns.push(ColumnSchema { name, column_type });
            }
            Ok(Entry::CreateTable {
                table,
                columns,
                primary_key,
            })
        }
        TAG_INSERT_ROW => {
            let table = read_string(bytes, &mut pos)?;
            let value_count = read_u32(bytes, &mut pos)? as usize;
            let mut row = Vec::with_capacity(value_count);
            for _ in 0..value_count {
                row.push(read_value(bytes, &mut pos)?);
            }
            Ok(Entry::InsertRow { table, row })
        }
        other => Err(corrupt(&format!("unknown entry tag {other}"))),
    }
}

/// An open, append-only log file.
pub struct Log {
    file: File,
}

impl Log {
    /// Open (creating if necessary) the log at `path` and replay every
    /// entry in order, feeding each to `apply`.
    pub fn open(path: &Path, mut apply: impl FnMut(Entry)) -> Result<Self> {
        let existing = std::fs::read(path);
        match existing {
            Ok(bytes) => {
                let mut pos = 0;
                while pos < bytes.len() {
                    let len = read_u32(&bytes, &mut pos)? as usize;
                    let end = pos
                        .checked_add(len)
                        .ok_or_else(|| corrupt("entry length overflow"))?;
                    let entry_bytes = bytes
                        .get(pos..end)
                        .ok_or_else(|| corrupt("truncated entry"))?;
                    apply(decode_entry(entry_bytes)?);
                    pos = end;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::Io(e)),
        }

        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Log { file })
    }

    fn append(&mut self, entry_bytes: &[u8]) -> Result<()> {
        let mut framed = Vec::with_capacity(4 + entry_bytes.len());
        write_u32(&mut framed, entry_bytes.len() as u32);
        framed.extend_from_slice(entry_bytes);
        self.file.write_all(&framed)?;
        self.file.flush()?;
        Ok(())
    }

    pub fn append_create_table(
        &mut self,
        table: &str,
        columns: &[ColumnSchema],
        primary_key: Option<&str>,
    ) -> Result<()> {
        self.append(&encode_create_table(table, columns, primary_key))
    }

    pub fn append_insert_row(&mut self, table: &str, row: &[Value]) -> Result<()> {
        self.append(&encode_insert_row(table, row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn temp_path(name: &str) -> std::path::PathBuf {
        static COUNTER: StdMutex<u64> = StdMutex::new(0);
        let mut counter = COUNTER.lock().unwrap_or_else(|e| e.into_inner());
        *counter += 1;
        std::env::temp_dir().join(format!(
            "mysql-rust-log-test-{name}-{}-{}",
            std::process::id(),
            *counter
        ))
    }

    #[test]
    fn round_trips_create_table_and_insert_entries() {
        let path = temp_path("round-trip");
        let mut replayed = Vec::new();
        {
            let mut log = Log::open(&path, |_| {}).unwrap();
            log.append_create_table(
                "t",
                &[
                    ColumnSchema {
                        name: "a".to_string(),
                        column_type: ColumnType::Int,
                    },
                    ColumnSchema {
                        name: "b".to_string(),
                        column_type: ColumnType::Varchar,
                    },
                ],
                Some("a"),
            )
            .unwrap();
            log.append_insert_row("t", &[Value::Int(1), Value::Varchar("x".to_string())])
                .unwrap();
            log.append_insert_row("t", &[Value::Null, Value::Varchar("y".to_string())])
                .unwrap();
        }

        let _log = Log::open(&path, |entry| replayed.push(entry)).unwrap();

        assert_eq!(replayed.len(), 3);
        match &replayed[0] {
            Entry::CreateTable {
                table,
                columns,
                primary_key,
            } => {
                assert_eq!(table, "t");
                assert_eq!(columns.len(), 2);
                assert_eq!(primary_key.as_deref(), Some("a"));
            }
            _ => panic!("expected CreateTable"),
        }
        match &replayed[1] {
            Entry::InsertRow { table, row } => {
                assert_eq!(table, "t");
                assert_eq!(row, &vec![Value::Int(1), Value::Varchar("x".to_string())]);
            }
            _ => panic!("expected InsertRow"),
        }
        match &replayed[2] {
            Entry::InsertRow { row, .. } => assert_eq!(row[0], Value::Null),
            _ => panic!("expected InsertRow"),
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn opening_a_missing_file_starts_empty_and_creates_it() {
        let path = temp_path("missing");
        std::fs::remove_file(&path).ok();

        let mut seen = 0;
        let _log = Log::open(&path, |_| seen += 1).unwrap();
        assert_eq!(seen, 0);
        assert!(path.exists());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_truncated_entry_not_panicking() {
        let path = temp_path("truncated");
        std::fs::write(&path, [5, 0, 0, 0, 1, 2, 3]).unwrap(); // claims 5 bytes, has 3

        let result = Log::open(&path, |_| {});
        assert!(result.is_err());

        std::fs::remove_file(&path).ok();
    }
}
