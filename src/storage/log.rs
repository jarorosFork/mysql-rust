//! A minimal write-ahead log for on-disk persistence.
//!
//! Every mutation (`CREATE TABLE`, row insert, or an atomic batch of either)
//! is appended as one length-prefixed, checksummed, durably-flushed (per
//! [`SyncPolicy`]) entry; reopening a data file replays every entry in
//! order to rebuild the in-memory state. This is deliberately simple — no
//! checkpointing/compaction yet — but it satisfies "data written before
//! shutdown is present after restart" (ROADMAP.md Phase 5) without the
//! complexity of a production WAL, which the roadmap explicitly allows
//! ("write-ahead or file-backed"). See PERFORMANCE_DURABILITY_PLAN.md for
//! what's still missing (checkpointing/compaction, group commit) and what
//! this module already closed (checksums, torn-tail recovery, `fsync`
//! durability, atomic multi-record commits).
//!
//! ## Record framing and crash recovery
//!
//! Each record on disk is `[len: u32 LE][crc: u32 LE][payload: len bytes]`,
//! where `crc` is a CRC-32 over `len`'s own four bytes *and* the payload
//! together — covering the length field itself, not just the payload, is
//! what makes the recovery below safe (see [`read_record`]).
//!
//! [`Log::open`] replays records front to back. A crash can only ever cut
//! off the *end* of a file (the OS writes bytes in order), so any problem
//! reading a record breaks into exactly two cases:
//!
//! - **Nothing decodable follows**: either there aren't enough bytes left
//!   for a full header, the header's claimed length runs past the end of
//!   the file, or the checksum fails and this is the last record the file
//!   has. All three are exactly what an interrupted write looks like, so
//!   this is a **torn tail** — recovery discards it and the file is
//!   truncated on disk to the last good record, so a subsequent append
//!   lands right after the good data instead of after orphaned garbage
//!   (which would otherwise turn a torn tail from *this* crash into
//!   apparent mid-file corruption after the *next* one).
//! - **The checksum fails but more (structured-looking) data follows it**:
//!   this cannot be explained by a crash — a torn write never leaves valid
//!   bytes *after* the damage — so it's treated as unrecoverable
//!   corruption and [`Log::open`] refuses outright rather than silently
//!   discarding a suffix of data that might still be intact.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::config::SyncPolicy;
use crate::storage::value::{ColumnSchema, ColumnType, Value};
use crate::{Error, Result};

const TAG_CREATE_TABLE: u8 = 1;
const TAG_INSERT_ROW: u8 = 2;
/// A batch of row inserts spanning one or more tables, applied all
/// together or not at all on replay (PERFORMANCE_DURABILITY_PLAN.md D2) —
/// used for a multi-row `INSERT` statement and for `COMMIT`. This falls out
/// of the existing per-record framing for free: a batch is exactly *one*
/// on-disk record, and `Log::open`'s torn-tail/corruption handling already
/// only ever applies a record whose checksum verified intact, so there's no
/// separate "partial batch" state to handle here.
const TAG_TRANSACTION: u8 = 3;

const VALUE_NULL: u8 = 0;
const VALUE_INT: u8 = 1;
const VALUE_VARCHAR: u8 = 2;
const VALUE_DECIMAL: u8 = 3;
const VALUE_DATE: u8 = 4;

const COLUMN_TYPE_INT: u8 = 0;
const COLUMN_TYPE_VARCHAR: u8 = 1;
const COLUMN_TYPE_DECIMAL: u8 = 2;
const COLUMN_TYPE_DATE: u8 = 3;

const COLUMN_FLAG_NULLABLE: u8 = 0b01;
const COLUMN_FLAG_AUTO_INCREMENT: u8 = 0b10;

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
    /// A batch of `(table, row)` inserts recorded as a single record — see
    /// [`TAG_TRANSACTION`].
    Transaction {
        rows: Vec<(String, Vec<Value>)>,
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
        Value::Decimal(unscaled, scale) => {
            buf.push(VALUE_DECIMAL);
            buf.extend_from_slice(&unscaled.to_le_bytes());
            buf.push(*scale);
        }
        Value::Date(s) => {
            buf.push(VALUE_DATE);
            write_string(buf, s);
        }
    }
}

pub(super) fn encode_create_table(
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
        match col.column_type {
            ColumnType::Int => buf.push(COLUMN_TYPE_INT),
            ColumnType::Varchar => buf.push(COLUMN_TYPE_VARCHAR),
            ColumnType::Date => buf.push(COLUMN_TYPE_DATE),
            // DECIMAL carries its scale as data, unlike the other types —
            // one extra byte right after the tag.
            ColumnType::Decimal(scale) => {
                buf.push(COLUMN_TYPE_DECIMAL);
                buf.push(scale);
            }
        }
        let mut flags = 0u8;
        if col.nullable {
            flags |= COLUMN_FLAG_NULLABLE;
        }
        if col.auto_increment {
            flags |= COLUMN_FLAG_AUTO_INCREMENT;
        }
        buf.push(flags);
    }
    buf
}

pub(super) fn encode_insert_row(table: &str, row: &[Value]) -> Vec<u8> {
    let mut buf = vec![TAG_INSERT_ROW];
    write_string(&mut buf, table);
    write_u32(&mut buf, row.len() as u32);
    for value in row {
        write_value(&mut buf, value);
    }
    buf
}

pub(super) fn encode_transaction(rows: &[(String, Vec<Value>)]) -> Vec<u8> {
    let mut buf = vec![TAG_TRANSACTION];
    write_u32(&mut buf, rows.len() as u32);
    for (table, row) in rows {
        write_string(&mut buf, table);
        write_u32(&mut buf, row.len() as u32);
        for value in row {
            write_value(&mut buf, value);
        }
    }
    buf
}

fn corrupt(context: &str) -> Error {
    Error::Execution(format!("corrupt data file: {context}"))
}

/// CRC-32 (the IEEE 802.3/zlib/gzip/PNG variant: reflected input and
/// output, polynomial `0xEDB88320`, initial value and final XOR both
/// `0xFFFFFFFF`). Hand-rolled rather than a dependency — matching
/// `auth::sha1`/`auth::sha256`, this is small, well-known, and fully
/// verifiable against a published test vector (see the module's tests):
/// CRC-32(`"123456789"`) = `0xCBF43926` is the standard "check value"
/// essentially every CRC-32 implementation is tested against.
fn crc32(data: &[u8]) -> u32 {
    const POLY: u32 = 0xEDB88320;
    let mut crc = 0xFFFFFFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            if crc & 1 == 1 {
                crc = (crc >> 1) ^ POLY;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// The checksum stored in a record's header: covers `len`'s own four bytes
/// *and* the payload together, not just the payload — see the module doc
/// comment for why that's what makes torn-tail recovery safe.
fn record_crc(len: u32, payload: &[u8]) -> u32 {
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(payload);
    crc32(&buf)
}

/// Frame one entry's payload as `[len: u32][crc: u32][payload]` (see the
/// module doc comment on record framing). Pure and allocation-only —
/// [`Log::append`] uses it for a single record, and
/// [`crate::storage::log_writer::LogWriter`] uses it on the *calling* task
/// to prepare a record before handing the already-framed bytes to the
/// dedicated writer thread, so only the actual file write — not this cheap
/// encoding step — has to happen on that one thread.
pub(super) fn frame_record(entry_bytes: &[u8]) -> Vec<u8> {
    let len = entry_bytes.len() as u32;
    let crc = record_crc(len, entry_bytes);
    let mut framed = Vec::with_capacity(8 + entry_bytes.len());
    write_u32(&mut framed, len);
    write_u32(&mut framed, crc);
    framed.extend_from_slice(entry_bytes);
    framed
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
        VALUE_DECIMAL => {
            let unscaled = read_i64(bytes, pos)?;
            let scale = read_byte(bytes, pos)?;
            Ok(Value::Decimal(unscaled, scale))
        }
        VALUE_DATE => Ok(Value::Date(read_string(bytes, pos)?)),
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
                    COLUMN_TYPE_DATE => ColumnType::Date,
                    COLUMN_TYPE_DECIMAL => ColumnType::Decimal(read_byte(bytes, &mut pos)?),
                    other => return Err(corrupt(&format!("unknown column type tag {other}"))),
                };
                let flags = read_byte(bytes, &mut pos)?;
                columns.push(ColumnSchema {
                    name,
                    column_type,
                    nullable: flags & COLUMN_FLAG_NULLABLE != 0,
                    auto_increment: flags & COLUMN_FLAG_AUTO_INCREMENT != 0,
                });
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
        TAG_TRANSACTION => {
            let row_count = read_u32(bytes, &mut pos)? as usize;
            let mut rows = Vec::with_capacity(row_count);
            for _ in 0..row_count {
                let table = read_string(bytes, &mut pos)?;
                let value_count = read_u32(bytes, &mut pos)? as usize;
                let mut row = Vec::with_capacity(value_count);
                for _ in 0..value_count {
                    row.push(read_value(bytes, &mut pos)?);
                }
                rows.push((table, row));
            }
            Ok(Entry::Transaction { rows })
        }
        other => Err(corrupt(&format!("unknown entry tag {other}"))),
    }
}

/// The outcome of trying to read one record starting at a given position —
/// see the module doc comment for the reasoning behind each case.
enum RecordRead<'a> {
    /// A complete, checksum-verified record.
    Ok { payload: &'a [u8], next_pos: usize },
    /// Nothing decodable follows: an incomplete header, a header whose
    /// claimed length runs past the end of the file, or a checksum
    /// mismatch on what is (per `next_valid_after` below) the last record
    /// in the file. Every one of these is what an interrupted write looks
    /// like — recoverable by discarding from here on.
    TornTail,
    /// A checksum mismatch with more (structured-looking) data still
    /// following — not explainable by a crash, since a torn write can only
    /// ever damage the tail. Not recoverable.
    Corrupt,
}

/// Try to read one record at `pos`. `file_len` is `bytes.len()`, passed
/// separately so this needs only the current record's own bytes rather
/// than the whole file — a checksum mismatch is only a torn tail if this
/// record's claimed end is the physical end of the file.
fn read_record(bytes: &[u8], pos: usize, file_len: usize) -> RecordRead<'_> {
    let Some(header) = bytes.get(pos..pos.saturating_add(8)) else {
        return RecordRead::TornTail;
    };
    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let crc = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let payload_start = pos + 8;
    let Some(payload_end) = payload_start.checked_add(len as usize) else {
        return RecordRead::TornTail;
    };
    let Some(payload) = bytes.get(payload_start..payload_end) else {
        return RecordRead::TornTail;
    };
    if record_crc(len, payload) != crc {
        return if payload_end == file_len {
            RecordRead::TornTail
        } else {
            RecordRead::Corrupt
        };
    }
    RecordRead::Ok {
        payload,
        next_pos: payload_end,
    }
}

/// Force `path`'s parent directory entry to durable storage
/// (PERFORMANCE_DURABILITY_PLAN.md D7) — a no-op with no directory handle
/// to open on platforms without this concept (Windows doesn't need it: an
/// `NTFS` directory entry update isn't cached the same way).
#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> Result<()> {
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> Result<()> {
    Ok(())
}

/// An open, append-only log file.
pub struct Log {
    file: File,
    sync_policy: SyncPolicy,
    /// Test-only: counts real `sync_data()` calls, so a test can prove the
    /// policy is actually wired through without depending on genuinely
    /// observing power-loss durability (which isn't testable at all) or on
    /// flaky timing comparisons.
    #[cfg(test)]
    sync_calls: usize,
}

impl Log {
    /// Open (creating if necessary) the log at `path` and replay every
    /// entry in order, feeding each to `apply`. A torn trailing record —
    /// what a crash produces — is discarded and truncated away on disk;
    /// checksum-failed corruption with valid data still following it is
    /// refused (see the module doc comment). `sync_policy` governs how
    /// aggressively subsequent [`Self::append`] calls are forced durable
    /// (PERFORMANCE_DURABILITY_PLAN.md D1).
    pub fn open(
        path: &Path,
        sync_policy: SyncPolicy,
        mut apply: impl FnMut(Entry),
    ) -> Result<Self> {
        let existing = std::fs::read(path);
        let is_new_file = matches!(&existing, Err(e) if e.kind() == std::io::ErrorKind::NotFound);
        match existing {
            Ok(bytes) => {
                let mut pos = 0;
                while pos < bytes.len() {
                    match read_record(&bytes, pos, bytes.len()) {
                        RecordRead::Ok { payload, next_pos } => {
                            apply(decode_entry(payload)?);
                            pos = next_pos;
                        }
                        RecordRead::TornTail => break,
                        RecordRead::Corrupt => {
                            return Err(corrupt(
                                "checksum mismatch with valid data still following it",
                            ));
                        }
                    }
                }
                if pos < bytes.len() {
                    // A torn tail was discarded above: truncate it away on
                    // disk too, so the next append lands right after the
                    // last good record rather than after orphaned garbage
                    // (which would make this crash's torn tail look like
                    // mid-file corruption after the *next* one).
                    let file = OpenOptions::new().write(true).open(path)?;
                    file.set_len(pos as u64)?;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::Io(e)),
        }

        let file = OpenOptions::new().create(true).append(true).open(path)?;
        if is_new_file {
            // PERFORMANCE_DURABILITY_PLAN.md D7: the file's *content* being
            // durable (once D1's sync_data lands below) doesn't help if the
            // directory entry pointing at it never made it to disk — on
            // power loss shortly after first creating it, the file itself
            // can vanish even though its blocks were synced. Only needed
            // once, for a freshly-created file: on every later restart the
            // directory entry already exists (and was already synced when
            // it was first created).
            sync_parent_dir(path)?;
        }
        Ok(Log {
            file,
            sync_policy,
            #[cfg(test)]
            sync_calls: 0,
        })
    }

    /// Test-only: how many times [`Self::append`] has actually called
    /// `sync_data()` so far.
    #[cfg(test)]
    fn sync_call_count(&self) -> usize {
        self.sync_calls
    }

    fn append(&mut self, entry_bytes: &[u8]) -> Result<()> {
        self.write_framed_batch(&frame_record(entry_bytes))
    }

    /// Write an already-framed batch of one or more records (see
    /// [`frame_record`]) as a single `write_all`, then sync once per
    /// `sync_policy` — the primitive [`crate::storage::log_writer::LogWriter`]'s
    /// dedicated thread uses to implement group commit
    /// (PERFORMANCE_DURABILITY_PLAN.md PD-2): many queued appends become one
    /// write and one fsync instead of one each, regardless of how many
    /// records `framed` actually contains.
    pub(super) fn write_framed_batch(&mut self, framed: &[u8]) -> Result<()> {
        self.file.write_all(framed)?;
        if self.sync_policy == SyncPolicy::Always {
            // `fdatasync`, not `fsync`: this log's own length change is
            // exactly the metadata that needs to hit disk for the new
            // bytes to be recoverable; other metadata (e.g. mtime) doesn't,
            // and skipping it is what makes fdatasync the right choice
            // over a full fsync here.
            self.file.sync_data()?;
            #[cfg(test)]
            {
                self.sync_calls += 1;
            }
        }
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

    /// Append a batch of `(table, row)` inserts as one atomic record (see
    /// [`TAG_TRANSACTION`]).
    pub fn append_transaction(&mut self, rows: &[(String, Vec<Value>)]) -> Result<()> {
        self.append(&encode_transaction(rows))
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
            let mut log = Log::open(&path, SyncPolicy::Never, |_| {}).unwrap();
            log.append_create_table(
                "t",
                &[
                    ColumnSchema {
                        name: "a".to_string(),
                        column_type: ColumnType::Int,
                        nullable: false,
                        auto_increment: false,
                    },
                    ColumnSchema {
                        name: "b".to_string(),
                        column_type: ColumnType::Varchar,
                        nullable: true,
                        auto_increment: false,
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

        let _log = Log::open(&path, SyncPolicy::Never, |entry| replayed.push(entry)).unwrap();

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
    fn round_trips_decimal_and_date_columns_and_values() {
        let path = temp_path("round-trip-decimal-date");
        let mut replayed = Vec::new();
        {
            let mut log = Log::open(&path, SyncPolicy::Never, |_| {}).unwrap();
            log.append_create_table(
                "t",
                &[
                    ColumnSchema {
                        name: "price".to_string(),
                        column_type: ColumnType::Decimal(2),
                        nullable: true,
                        auto_increment: false,
                    },
                    ColumnSchema {
                        name: "d".to_string(),
                        column_type: ColumnType::Date,
                        nullable: true,
                        auto_increment: false,
                    },
                ],
                None,
            )
            .unwrap();
            log.append_insert_row(
                "t",
                &[
                    Value::Decimal(1999, 2),
                    Value::Date("2024-01-15".to_string()),
                ],
            )
            .unwrap();
            log.append_insert_row("t", &[Value::Decimal(-500, 2), Value::Null])
                .unwrap();
        }

        let _log = Log::open(&path, SyncPolicy::Never, |entry| replayed.push(entry)).unwrap();

        match &replayed[0] {
            Entry::CreateTable { columns, .. } => {
                assert_eq!(columns[0].column_type, ColumnType::Decimal(2));
                assert_eq!(columns[1].column_type, ColumnType::Date);
            }
            _ => panic!("expected CreateTable"),
        }
        match &replayed[1] {
            Entry::InsertRow { row, .. } => {
                assert_eq!(
                    row,
                    &vec![
                        Value::Decimal(1999, 2),
                        Value::Date("2024-01-15".to_string())
                    ]
                );
            }
            _ => panic!("expected InsertRow"),
        }
        match &replayed[2] {
            Entry::InsertRow { row, .. } => {
                assert_eq!(row, &vec![Value::Decimal(-500, 2), Value::Null]);
            }
            _ => panic!("expected InsertRow"),
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn opening_a_missing_file_starts_empty_and_creates_it() {
        let path = temp_path("missing");
        std::fs::remove_file(&path).ok();

        let mut seen = 0;
        let _log = Log::open(&path, SyncPolicy::Never, |_| seen += 1).unwrap();
        assert_eq!(seen, 0);
        assert!(path.exists());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn recovers_from_a_torn_trailing_record_by_truncating_it_away() {
        let path = temp_path("torn-tail");
        let mut log = Log::open(&path, SyncPolicy::Never, |_| {}).unwrap();
        log.append_insert_row("t", &[Value::Int(1)]).unwrap();
        let good_len = std::fs::metadata(&path).unwrap().len();
        log.append_insert_row("t", &[Value::Int(2)]).unwrap();
        let full_len = std::fs::metadata(&path).unwrap().len();
        drop(log);
        let full_bytes = std::fs::read(&path).unwrap();

        for truncate_at in good_len..full_len {
            std::fs::write(&path, &full_bytes[..truncate_at as usize]).unwrap();
            let mut replayed = Vec::new();
            Log::open(&path, SyncPolicy::Never, |e| replayed.push(e)).unwrap_or_else(|e| {
                panic!(
                    "truncating the trailing record at byte {truncate_at} of {full_len} \
                     should recover, not error: {e}"
                )
            });
            assert_eq!(
                replayed.len(),
                1,
                "only the fully-written first record should replay (truncated at {truncate_at})"
            );
            // The garbage tail should be truncated away on disk too, so a
            // later append doesn't leave a gap of orphaned bytes before it.
            assert_eq!(std::fs::metadata(&path).unwrap().len(), good_len);
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_file_too_short_for_even_one_record_header_recovers_as_empty() {
        let path = temp_path("truncated-header");
        // Not even a full 8-byte [len][crc] header -- exactly what a crash
        // right after the very first write ever looks like.
        std::fs::write(&path, [5, 0, 0, 0, 1, 2, 3]).unwrap();

        let mut seen = 0;
        Log::open(&path, SyncPolicy::Never, |_| seen += 1)
            .expect("a torn header should recover, not error");
        assert_eq!(seen, 0);
        assert_eq!(
            std::fs::read(&path).unwrap().len(),
            0,
            "the garbage should be truncated away"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn mid_file_checksum_mismatch_with_valid_data_after_it_is_refused() {
        let path = temp_path("mid-file-corrupt");
        let mut log = Log::open(&path, SyncPolicy::Never, |_| {}).unwrap();
        log.append_insert_row("t", &[Value::Int(1)]).unwrap(); // record A
        let before_record_b = std::fs::metadata(&path).unwrap().len();
        log.append_insert_row("t", &[Value::Int(2)]).unwrap(); // record B -- corrupted below
        log.append_insert_row("t", &[Value::Int(3)]).unwrap(); // record C -- stays valid, still follows
        drop(log);

        let mut bytes = std::fs::read(&path).unwrap();
        // Flip a byte right after record B's 8-byte header (its payload's
        // first byte). Record C's bytes are still intact and present after
        // it -- exactly what makes this mid-file damage, not a torn tail.
        let record_b_payload_start = before_record_b as usize + 8;
        bytes[record_b_payload_start] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        assert!(
            Log::open(&path, SyncPolicy::Never, |_| {}).is_err(),
            "checksum-failed corruption with valid data still following it must be refused"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        // The canonical CRC-32 (IEEE 802.3) test vector, used to validate
        // essentially every implementation of this algorithm.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn crc32_of_empty_input_is_zero() {
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn crc32_detects_a_single_flipped_bit() {
        let original = crc32(b"mysql-rust");
        let mut corrupted = *b"mysql-rust";
        corrupted[3] ^= 0x01;
        assert_ne!(original, crc32(&corrupted));
    }

    #[test]
    fn always_policy_calls_sync_data_once_per_append() {
        let path = temp_path("sync-always");
        let mut log = Log::open(&path, SyncPolicy::Always, |_| {}).unwrap();
        assert_eq!(log.sync_call_count(), 0);

        log.append_insert_row("t", &[Value::Int(1)]).unwrap();
        assert_eq!(log.sync_call_count(), 1);

        log.append_insert_row("t", &[Value::Int(2)]).unwrap();
        log.append_insert_row("t", &[Value::Int(3)]).unwrap();
        assert_eq!(
            log.sync_call_count(),
            3,
            "Always must sync durably on every single append, not batch them"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn never_policy_never_calls_sync_data() {
        let path = temp_path("sync-never");
        let mut log = Log::open(&path, SyncPolicy::Never, |_| {}).unwrap();

        log.append_insert_row("t", &[Value::Int(1)]).unwrap();
        log.append_insert_row("t", &[Value::Int(2)]).unwrap();
        assert_eq!(
            log.sync_call_count(),
            0,
            "Never must not pay any sync_data cost"
        );

        std::fs::remove_file(&path).ok();
    }
}
