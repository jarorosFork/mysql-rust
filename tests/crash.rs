//! Crash-safety harness (PERFORMANCE_DURABILITY_PLAN.md, phase PD-0).
//!
//! Every other integration test's `TestServer` (see `tests/common`) runs
//! the server **in-process**, on a background `tokio` task — perfect for
//! graceful-shutdown tests, useless for crash tests, since killing that
//! process would kill the test binary too. This file spawns the real
//! compiled `mysql-rust` binary as its own OS process so it can be
//! `SIGKILL`ed exactly like a real crash: no destructor runs, no buffered
//! writes flush, nothing gets a chance to clean up.
//!
//! Several assertions below are `#[ignore]`d with a reason naming the
//! PERFORMANCE_DURABILITY_PLAN.md task that fixes them. That is this
//! harness's whole point per the plan's PD-0 acceptance criterion: each
//! D-task's own acceptance is un-ignoring (not rewriting) its assertion.
//! Run the ignored set with `cargo test --test crash -- --ignored`.

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mysql_async::prelude::*;
use mysql_async::{Conn, OptsBuilder};

use mysql_rust::storage::{ColumnSchema, ColumnType, InMemoryStorage, Storage, Value};

const USERNAME: &str = "alice";
const PASSWORD: &str = "s3cret";

/// A `mysql-rust` server running as its own OS process. `Drop` force-kills
/// it (best-effort) so a panicking test never leaks a listening process
/// into the rest of the suite.
struct ServerProcess {
    child: Child,
    addr: SocketAddr,
}

impl ServerProcess {
    /// Spawn the real compiled binary (`env!("CARGO_BIN_EXE_...")` — the
    /// same binary `cargo run`/production use, not a test double) against
    /// `data_dir`, on a free loopback port, and block until it's actually
    /// accepting connections.
    fn spawn(data_dir: &Path) -> Self {
        let addr = SocketAddr::from(([127, 0, 0, 1], free_port()));
        let child = Command::new(env!("CARGO_BIN_EXE_mysql-rust"))
            .env("MYSQLRUST_LISTEN_ADDR", addr.to_string())
            .env("MYSQLRUST_DATA_DIR", data_dir)
            .env("MYSQLRUST_USER", USERNAME)
            .env("MYSQLRUST_PASSWORD", PASSWORD)
            .env("MYSQLRUST_LOG_LEVEL", "error")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the mysql-rust binary (did `cargo build` run first?)");

        wait_until_listening(addr);
        ServerProcess { child, addr }
    }

    /// `SIGKILL` on Unix (`TerminateProcess` elsewhere via `Child::kill`) —
    /// no graceful shutdown, no flush, no destructor: exactly what a real
    /// crash looks like from the outside. Blocks until the OS has actually
    /// reaped the process, so a subsequent `spawn` never races a half-dead
    /// predecessor.
    fn crash(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

fn wait_until_listening(addr: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        if Instant::now() > deadline {
            panic!("server did not start listening on {addr} within 5s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

async fn connect(addr: SocketAddr) -> Conn {
    let opts = OptsBuilder::default()
        .ip_or_hostname(addr.ip().to_string())
        .tcp_port(addr.port())
        .user(Some(USERNAME))
        .pass(Some(PASSWORD));
    Conn::new(opts).await.expect("connect to spawned server")
}

/// A fresh, uniquely-named temp directory for one test's data files —
/// mirrors the pattern already used by `storage::engine`/`storage::log`'s
/// own tests, so parallel `cargo test` runs never collide.
fn temp_data_dir(name: &str) -> PathBuf {
    static COUNTER: Mutex<u64> = Mutex::new(0);
    let mut counter = COUNTER.lock().unwrap_or_else(|e| e.into_inner());
    *counter += 1;
    let dir = std::env::temp_dir().join(format!(
        "mysql-rust-crash-test-{name}-{}-{}",
        std::process::id(),
        *counter
    ));
    std::fs::create_dir_all(&dir).expect("create temp data dir");
    dir
}

// ---------------------------------------------------------------------
// Process-crash tests: real SIGKILL, real subprocess, real restart.
// ---------------------------------------------------------------------

/// A `SIGKILL`ed process still leaves everything the OS page cache already
/// holds — that's process-crash durability, and it works today without any
/// `fsync` at all (D1 is about *power-loss*/kernel-panic durability, which
/// a `SIGKILL` cannot simulate: the page cache survives the process dying
/// regardless of whether anyone ever called `fsync`). This test is the
/// harness's own sanity check and a regression guard, not a D1 proof.
#[tokio::test]
async fn process_crash_after_acknowledged_write_still_persists_it() {
    let dir = temp_data_dir("ack-persists");
    let server = ServerProcess::spawn(&dir);
    {
        let mut conn = connect(server.addr).await;
        conn.query_drop("CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR)")
            .await
            .expect("create table");
        conn.query_drop("INSERT INTO t VALUES (1, 'ada')")
            .await
            .expect("insert");
        // The driver only returns once it has read the server's OK packet
        // for this statement — so this row is "acknowledged" by definition.
    }
    server.crash();

    let restarted = ServerProcess::spawn(&dir);
    let mut conn = connect(restarted.addr).await;
    let rows: Vec<(i64, String)> = conn
        .query("SELECT id, name FROM t ORDER BY id")
        .await
        .expect("select after restart");
    assert_eq!(rows, vec![(1, "ada".to_string())]);
}

/// A multi-row `INSERT` is one statement with one client-visible outcome:
/// either the client got an OK for all N rows, or it got nothing. Today
/// `execute_insert` calls `Storage::insert_row` once per row, each an
/// independent apply-then-log step (PERFORMANCE_DURABILITY_PLAN.md D2/D3),
/// so a kill partway through can leave a **partial** row count on disk —
/// data no acknowledged statement ever produced. Sweeps several kill
/// delays (0..=21ms) across fresh data dirs to land the kill at different
/// points in the row loop without needing byte-exact control over it.
#[tokio::test]
#[ignore = "PERFORMANCE_DURABILITY_PLAN.md D2: multi-row INSERT is not yet an atomic log record; un-ignore once it is"]
async fn crash_mid_multi_row_insert_never_leaves_a_partial_statement() {
    const ROW_COUNT: usize = 1000;
    let mut insert_sql = String::from("INSERT INTO t VALUES ");
    for i in 0..ROW_COUNT {
        if i > 0 {
            insert_sql.push(',');
        }
        insert_sql.push_str(&format!("({i}, 'row{i}')"));
    }

    for (trial, delay_ms) in [0u64, 1, 2, 3, 5, 8, 13, 21].into_iter().enumerate() {
        let dir = temp_data_dir(&format!("mid-multi-insert-{trial}"));
        let server = ServerProcess::spawn(&dir);
        {
            let mut conn = connect(server.addr).await;
            conn.query_drop("CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR)")
                .await
                .expect("create table");
            let insert = conn.query_drop(insert_sql.clone());
            // Race the insert against a short timeout, then kill regardless
            // of whether it finished — this is what makes "kill while the
            // row loop is still running" reproducible without instrumenting
            // the server itself.
            let _ = tokio::time::timeout(Duration::from_millis(delay_ms), insert).await;
        }
        server.crash();

        let restarted = ServerProcess::spawn(&dir);
        let mut conn = connect(restarted.addr).await;
        let count: Option<i64> = conn
            .query_first("SELECT COUNT(*) FROM t")
            .await
            .expect("count after restart");
        let count = count.expect("COUNT(*) always returns a row");
        assert!(
            count == 0 || count == ROW_COUNT as i64,
            "trial {trial} (delay {delay_ms}ms): expected 0 or {ROW_COUNT} rows, found {count} \
             -- a partial multi-row INSERT survived a crash"
        );
    }
}

/// Same shape as the multi-row-INSERT test, but for `BEGIN`/`COMMIT`:
/// `Transaction::commit` (storage/transaction.rs) applies its buffered rows
/// one at a time too, so a crash mid-commit can permanently resurrect a
/// prefix of the transaction — the exact atomicity violation
/// PERFORMANCE_DURABILITY_PLAN.md D2 exists to close.
#[tokio::test]
#[ignore = "PERFORMANCE_DURABILITY_PLAN.md D2: COMMIT is not yet an atomic log record; un-ignore once it is"]
async fn crash_mid_transaction_commit_never_leaves_a_partial_transaction() {
    const ROW_COUNT: usize = 500;

    for (trial, delay_ms) in [0u64, 1, 2, 3, 5, 8, 13, 21].into_iter().enumerate() {
        let dir = temp_data_dir(&format!("mid-commit-{trial}"));
        let server = ServerProcess::spawn(&dir);
        {
            let mut conn = connect(server.addr).await;
            conn.query_drop("CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR)")
                .await
                .expect("create table");
            conn.query_drop("BEGIN").await.expect("begin");
            for i in 0..ROW_COUNT {
                conn.query_drop(format!("INSERT INTO t VALUES ({i}, 'row{i}')"))
                    .await
                    .expect("insert inside transaction");
            }
            let commit = conn.query_drop("COMMIT");
            let _ = tokio::time::timeout(Duration::from_millis(delay_ms), commit).await;
        }
        server.crash();

        let restarted = ServerProcess::spawn(&dir);
        let mut conn = connect(restarted.addr).await;
        let count: Option<i64> = conn
            .query_first("SELECT COUNT(*) FROM t")
            .await
            .expect("count after restart");
        let count = count.expect("COUNT(*) always returns a row");
        assert!(
            count == 0 || count == ROW_COUNT as i64,
            "trial {trial} (delay {delay_ms}ms): expected 0 or {ROW_COUNT} rows, found {count} \
             -- a partially-committed transaction survived a crash"
        );
    }
}

// ---------------------------------------------------------------------
// Torn/corrupt log recovery: direct storage-engine tests. No subprocess
// needed here — byte-exact control over the on-disk file is the point,
// and `InMemoryStorage::open`/`open_in_dir` are the same public API a
// real restart goes through.
// ---------------------------------------------------------------------

fn int_pk_col(name: &str) -> ColumnSchema {
    ColumnSchema {
        name: name.to_string(),
        column_type: ColumnType::Int,
        nullable: false,
        auto_increment: false,
    }
}

fn varchar_col(name: &str) -> ColumnSchema {
    ColumnSchema {
        name: name.to_string(),
        column_type: ColumnType::Varchar,
        nullable: true,
        auto_increment: false,
    }
}

/// A torn final log record — the *normal* artifact of any crash, since the
/// last `write()` in flight when a process dies is exactly as likely to be
/// incomplete as complete — `InMemoryStorage::open` recovers by discarding
/// just the incomplete tail record (PERFORMANCE_DURABILITY_PLAN.md D4/D5),
/// matching Postgres/InnoDB/SQLite/RocksDB recovery behavior, rather than
/// refusing to start at all. Sweeps every possible truncation point within
/// the final record so the fix has to be correct at each byte boundary,
/// not just "mostly".
#[test]
fn torn_log_tail_recovers_by_discarding_the_incomplete_final_record() {
    let path = temp_data_dir("torn-tail").join("data.log");
    let storage = InMemoryStorage::open(&path).expect("open");
    storage
        .create_table(
            "t",
            vec![int_pk_col("id"), varchar_col("name")],
            Some("id".to_string()),
        )
        .expect("create table");
    storage
        .insert_row("t", vec![Value::Int(1), Value::Varchar("ada".to_string())])
        .expect("insert row 1");
    let before_last_record = std::fs::metadata(&path).expect("stat").len();
    storage
        .insert_row("t", vec![Value::Int(2), Value::Varchar("bob".to_string())])
        .expect("insert row 2");
    let after_last_record = std::fs::metadata(&path).expect("stat").len();
    drop(storage);

    let full_bytes = std::fs::read(&path).expect("read full log");
    assert_eq!(full_bytes.len() as u64, after_last_record);

    for truncate_at in before_last_record..after_last_record {
        std::fs::write(&path, &full_bytes[..truncate_at as usize]).expect("write truncated log");
        let reopened = InMemoryStorage::open(&path).unwrap_or_else(|e| {
            panic!(
                "truncating the final record at byte {truncate_at} of {after_last_record} \
                 should recover (discarding just that record), not error: {e}"
            )
        });
        assert_eq!(
            reopened.scan("t").expect("scan"),
            vec![vec![Value::Int(1), Value::Varchar("ada".to_string())]],
            "truncated at byte {truncate_at}: row 1 (fully written) must survive, \
             row 2 (torn) must be discarded"
        );
    }
}

/// The complement of the torn-tail test: corruption that is *not* confined
/// to the final record — here, the second of three records is damaged
/// while a third, fully-valid record still follows it on disk — must never
/// be silently accepted by truncate-the-tail recovery. This already passes
/// today (any framing break is refused) and must keep passing once D4/D5
/// land: recovering from a torn *tail* is not license to paper over
/// mid-file damage by discarding everything after the first problem.
#[test]
fn mid_file_corruption_with_valid_data_after_it_is_still_refused() {
    let path = temp_data_dir("mid-file-corrupt").join("data.log");
    let storage = InMemoryStorage::open(&path).expect("open");
    storage
        .create_table("t", vec![int_pk_col("id")], Some("id".to_string()))
        .expect("create table");
    let before_row2 = std::fs::metadata(&path).expect("stat").len();
    storage.insert_row("t", vec![Value::Int(1)]).expect("row 1");
    storage.insert_row("t", vec![Value::Int(2)]).expect("row 2");
    drop(storage);

    let mut bytes = std::fs::read(&path).expect("read log");
    // Every record is framed as `[u32 len][u32 crc][entry bytes...]`
    // (storage::log) -- flipping any payload byte breaks its checksum
    // regardless of the rest of the format, without hardcoding the tag
    // constants themselves. Row 2's record follows row 1's untouched
    // record, so this is unambiguously "damage in the middle", not "torn
    // at the end".
    let row2_payload_offset = before_row2 as usize + 8;
    bytes[row2_payload_offset] ^= 0xFF;
    std::fs::write(&path, &bytes).expect("write corrupted log");

    assert!(
        InMemoryStorage::open(&path).is_err(),
        "mid-file corruption with valid data still following it must never be silently accepted"
    );
}
