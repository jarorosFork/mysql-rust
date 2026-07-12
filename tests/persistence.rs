//! Integration test: data written in one connection is present in a later,
//! independent connection against the same `data_dir` — the Phase 5
//! acceptance criterion in ROADMAP.md ("data written before shutdown is
//! present after restart"). Each `spawn_server` call here opens a fresh
//! `TcpListener` and `Connection`, standing in for a separate server run.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};

use mysql_rust::auth::native_password::compute_auth_response;
use mysql_rust::config::{Config, UserCredential};
use mysql_rust::protocol::capabilities::{
    CLIENT_DEPRECATE_EOF, CLIENT_PLUGIN_AUTH, CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION,
};
use mysql_rust::protocol::Packet;

use common::{extract_scramble, spawn_server, TestClient};

const COM_QUERY: u8 = 0x03;

fn temp_dir(name: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "mysql-rust-persistence-test-{name}-{}-{n}",
        std::process::id()
    ))
}

fn connect_and_authenticate(config: Config) -> TestClient {
    let mut client = spawn_server(config);
    let handshake_packet = client.read_packet();
    let scramble = extract_scramble(&handshake_packet.payload);

    let auth_response = compute_auth_response(Some(b"s3cret"), &scramble);
    let caps =
        CLIENT_PROTOCOL_41 | CLIENT_PLUGIN_AUTH | CLIENT_SECURE_CONNECTION | CLIENT_DEPRECATE_EOF;
    let mut payload = Vec::new();
    payload.extend_from_slice(&caps.to_le_bytes());
    payload.extend_from_slice(&16_777_216u32.to_le_bytes());
    payload.push(45);
    payload.extend_from_slice(&[0u8; 23]);
    payload.extend_from_slice(b"alice\0");
    payload.push(auth_response.len() as u8);
    payload.extend_from_slice(&auth_response);
    payload.extend_from_slice(b"mysql_native_password\0");
    client.write_packet(&Packet::new(1, payload));

    let verdict = client.read_packet();
    assert_eq!(
        verdict.payload[0], 0x00,
        "expected auth to succeed with an OK packet"
    );

    client
}

fn send_query(client: &mut TestClient, sql: &str) {
    let mut payload = vec![COM_QUERY];
    payload.extend_from_slice(sql.as_bytes());
    client.write_packet(&Packet::new(0, payload));
}

fn expect_ok(client: &mut TestClient) -> u64 {
    let packet = client.read_packet();
    assert_eq!(
        packet.payload[0], 0x00,
        "expected an OK packet, got header {:#x}",
        packet.payload[0]
    );
    packet.payload[1] as u64
}

/// Read a full text-protocol result set under `CLIENT_DEPRECATE_EOF` framing.
/// `None` in a row is SQL `NULL` (the wire's `0xfb` marker).
fn read_result_set(client: &mut TestClient) -> Vec<Vec<Option<String>>> {
    let count_packet = client.read_packet();
    let column_count = count_packet.payload[0] as usize;

    for _ in 0..column_count {
        let _column_def = client.read_packet();
    }

    let mut rows = Vec::new();
    loop {
        let packet = client.read_packet();
        // CLIENT_DEPRECATE_EOF terminator: OK packet with the 0xFE header.
        if packet.payload.first() == Some(&0xfe) && packet.payload.len() < 9 {
            break;
        }
        rows.push(parse_text_row(&packet.payload, column_count));
    }
    rows
}

fn parse_text_row(payload: &[u8], column_count: usize) -> Vec<Option<String>> {
    const NULL_MARKER: u8 = 0xfb;
    let mut values = Vec::with_capacity(column_count);
    let mut pos = 0;
    for _ in 0..column_count {
        if payload[pos] == NULL_MARKER {
            values.push(None);
            pos += 1;
            continue;
        }
        let len = payload[pos] as usize;
        pos += 1;
        values.push(Some(
            String::from_utf8(payload[pos..pos + len].to_vec()).expect("utf8 value"),
        ));
        pos += len;
    }
    values
}

fn some(s: &str) -> Option<String> {
    Some(s.to_string())
}

fn config_with_data_dir(data_dir: std::path::PathBuf) -> Config {
    Config {
        users: vec![UserCredential::with_password("alice", "s3cret")],
        data_dir: Some(data_dir),
        ..Config::default()
    }
}

#[test]
fn data_written_before_shutdown_is_present_after_restart() {
    let dir = temp_dir("basic");
    std::fs::remove_dir_all(&dir).ok();

    // "Before shutdown": create a table and insert rows in one connection.
    {
        let mut client = connect_and_authenticate(config_with_data_dir(dir.clone()));
        send_query(
            &mut client,
            "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR)",
        );
        expect_ok(&mut client);
        send_query(
            &mut client,
            "INSERT INTO users VALUES (1, 'alice'), (2, 'bob')",
        );
        assert_eq!(expect_ok(&mut client), 2);
    } // connection (and its listener) dropped — nothing further happens on this socket.

    // "After restart": a completely independent connection, same data_dir.
    {
        let mut client = connect_and_authenticate(config_with_data_dir(dir.clone()));
        send_query(&mut client, "SELECT * FROM users");
        let rows = read_result_set(&mut client);
        assert_eq!(
            rows,
            vec![vec![some("1"), some("alice")], vec![some("2"), some("bob")]]
        );

        // The primary-key index is correctly rebuilt too, not just the rows.
        send_query(&mut client, "SELECT name FROM users WHERE id = 2");
        assert_eq!(read_result_set(&mut client), vec![vec![some("bob")]]);
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn primary_key_uniqueness_still_enforced_after_restart() {
    let dir = temp_dir("pk-uniqueness");
    std::fs::remove_dir_all(&dir).ok();

    {
        let mut client = connect_and_authenticate(config_with_data_dir(dir.clone()));
        send_query(&mut client, "CREATE TABLE t (id INT PRIMARY KEY)");
        expect_ok(&mut client);
        send_query(&mut client, "INSERT INTO t VALUES (1)");
        expect_ok(&mut client);
    }

    {
        let mut client = connect_and_authenticate(config_with_data_dir(dir.clone()));
        send_query(&mut client, "INSERT INTO t VALUES (1)"); // duplicate of the persisted row
        let packet = client.read_packet();
        assert_eq!(
            packet.payload[0], 0xff,
            "expected an ERR packet for the duplicate primary key"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn null_values_survive_a_restart() {
    let dir = temp_dir("nulls");
    std::fs::remove_dir_all(&dir).ok();

    {
        let mut client = connect_and_authenticate(config_with_data_dir(dir.clone()));
        send_query(&mut client, "CREATE TABLE t (a INT, b VARCHAR)");
        expect_ok(&mut client);
        send_query(&mut client, "INSERT INTO t VALUES (1, NULL)");
        expect_ok(&mut client);
    }

    {
        let mut client = connect_and_authenticate(config_with_data_dir(dir.clone()));
        send_query(&mut client, "SELECT * FROM t");
        assert_eq!(read_result_set(&mut client), vec![vec![some("1"), None]]);
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn no_data_dir_means_nothing_persists() {
    // Two connections with plain in-memory (no data_dir) config; the second
    // must NOT see the first's table, proving persistence is genuinely
    // opt-in rather than accidentally always-on.
    let cfg = || Config {
        users: vec![UserCredential::with_password("alice", "s3cret")],
        ..Config::default()
    };

    {
        let mut client = connect_and_authenticate(cfg());
        send_query(&mut client, "CREATE TABLE t (a INT)");
        expect_ok(&mut client);
    }

    {
        let mut client = connect_and_authenticate(cfg());
        send_query(&mut client, "SELECT * FROM t");
        let packet = client.read_packet();
        assert_eq!(
            packet.payload[0], 0xff,
            "expected an ERR: table shouldn't exist without a data_dir"
        );
    }
}

/// A crash can only ever truncate the *end* of the data file — an
/// incomplete trailing record is exactly what that looks like, and
/// PERFORMANCE_DURABILITY_PLAN.md D4 makes it recoverable: the incomplete
/// record is discarded rather than the whole server refusing to start.
/// (Superseded the file's previous version of this test, which — before
/// D4/D5's torn-tail recovery landed — expected this same byte pattern to
/// be rejected outright; see `reopening_a_file_with_mid_file_corruption_
/// errors_instead_of_panicking` below for the case that's still refused.)
#[test]
fn reopening_a_file_with_a_torn_trailing_record_recovers_without_panicking() {
    use mysql_rust::storage::Storage;

    let dir = temp_dir("torn-tail");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    // A record header claiming far more payload than the file actually
    // has — exactly what a crash mid-write of the very first record ever
    // looks like.
    std::fs::write(dir.join("data.log"), [255, 255, 255, 255, 1, 2]).unwrap();

    let storage = mysql_rust::storage::InMemoryStorage::open_in_dir(
        &dir,
        mysql_rust::config::SyncPolicy::Never,
    )
    .expect("a torn trailing record should recover, not error or panic");
    assert!(storage.tables().unwrap().is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

/// The complement of the torn-tail case: corruption that is *not* confined
/// to the final record — here, the first of two insert records is
/// damaged while the second, fully-valid one still follows it — can never
/// be explained by a crash (which only ever damages the tail), so it must
/// still be refused, not silently truncated away.
#[tokio::test]
async fn reopening_a_file_with_mid_file_corruption_errors_instead_of_panicking() {
    use mysql_rust::storage::{ColumnSchema, ColumnType, Storage, Value};

    let dir = temp_dir("mid-file-corrupt");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("data.log");

    let storage =
        mysql_rust::storage::InMemoryStorage::open(&path, mysql_rust::config::SyncPolicy::Never)
            .unwrap();
    storage
        .create_table(
            "t",
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
    let before_inserts = std::fs::metadata(&path).unwrap().len(); // end of the CREATE TABLE record
    storage.insert_row("t", vec![Value::Int(1)]).await.unwrap(); // record: gets corrupted below
    storage.insert_row("t", vec![Value::Int(2)]).await.unwrap(); // record: stays valid, still follows
    drop(storage);

    let mut bytes = std::fs::read(&path).unwrap();
    // Corrupt the first payload byte of the row-1 record (right after its
    // 8-byte header). The row-2 record is still fully intact after it.
    let corrupt_at = before_inserts as usize + 8;
    bytes[corrupt_at] ^= 0xFF;
    std::fs::write(&path, &bytes).unwrap();

    let result = mysql_rust::storage::InMemoryStorage::open_in_dir(
        &dir,
        mysql_rust::config::SyncPolicy::Never,
    );
    assert!(
        result.is_err(),
        "expected mid-file corruption (with valid data still following it) to be rejected, not panic"
    );

    std::fs::remove_dir_all(&dir).ok();
}
