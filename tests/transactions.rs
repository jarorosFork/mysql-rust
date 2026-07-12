//! Integration tests for Phase 7 (transactions & locking): `BEGIN` /
//! `COMMIT` / `ROLLBACK`, read-committed isolation across real connections,
//! and table-level locking preventing lost updates under concurrency — the
//! Phase 7 acceptance criterion in ROADMAP.md ("documented isolation level
//! holds under a concurrent test").

mod common;

use std::time::{Duration, Instant};

use mysql_rust::auth::native_password::compute_auth_response;
use mysql_rust::config::{Config, UserCredential};
use mysql_rust::protocol::capabilities::{
    CLIENT_DEPRECATE_EOF, CLIENT_PLUGIN_AUTH, CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION,
};
use mysql_rust::protocol::Packet;

use common::{extract_scramble, TestClient, TestServer};

const COM_QUERY: u8 = 0x03;

fn test_config() -> Config {
    Config {
        users: vec![UserCredential::with_password("alice", "s3cret")],
        ..Config::default()
    }
}

fn authenticate(client: &mut TestClient) {
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
}

fn send_query(client: &mut TestClient, sql: &str) {
    let mut payload = vec![COM_QUERY];
    payload.extend_from_slice(sql.as_bytes());
    client.write_packet(&Packet::new(0, payload));
}

fn expect_ok(client: &mut TestClient) {
    let packet = client.read_packet();
    assert_eq!(
        packet.payload[0], 0x00,
        "expected an OK packet, got header {:#x}",
        packet.payload[0]
    );
}

fn expect_err(client: &mut TestClient) {
    let packet = client.read_packet();
    assert_eq!(
        packet.payload[0], 0xff,
        "expected an ERR packet, got header {:#x}",
        packet.payload[0]
    );
}

fn read_result_set(client: &mut TestClient) -> Vec<Vec<String>> {
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

fn parse_text_row(payload: &[u8], column_count: usize) -> Vec<String> {
    let mut values = Vec::with_capacity(column_count);
    let mut pos = 0;
    for _ in 0..column_count {
        let len = payload[pos] as usize;
        pos += 1;
        values.push(String::from_utf8(payload[pos..pos + len].to_vec()).expect("utf8 value"));
        pos += len;
    }
    values
}

#[test]
fn uncommitted_insert_is_invisible_to_other_connections_until_commit() {
    let server = TestServer::start(test_config());

    let mut setup = server.connect();
    authenticate(&mut setup);
    send_query(
        &mut setup,
        "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR)",
    );
    expect_ok(&mut setup);

    let mut a = server.connect();
    authenticate(&mut a);
    let mut b = server.connect();
    authenticate(&mut b);

    send_query(&mut a, "BEGIN");
    expect_ok(&mut a);
    send_query(&mut a, "INSERT INTO t VALUES (1, 'alice')");
    expect_ok(&mut a);

    // b (autocommit, a completely separate connection) must not see a's
    // uncommitted write — this is read committed isolation.
    send_query(&mut b, "SELECT * FROM t");
    assert!(
        read_result_set(&mut b).is_empty(),
        "uncommitted data leaked to another connection"
    );

    // a itself, though, sees its own pending write.
    send_query(&mut a, "SELECT * FROM t");
    assert_eq!(
        read_result_set(&mut a),
        vec![vec!["1".to_string(), "alice".to_string()]]
    );

    send_query(&mut a, "COMMIT");
    expect_ok(&mut a);

    // Now that it's committed, b sees it too.
    send_query(&mut b, "SELECT * FROM t");
    assert_eq!(
        read_result_set(&mut b),
        vec![vec!["1".to_string(), "alice".to_string()]]
    );
}

#[test]
fn rollback_restores_state() {
    let server = TestServer::start(test_config());

    let mut client = server.connect();
    authenticate(&mut client);
    send_query(
        &mut client,
        "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR)",
    );
    expect_ok(&mut client);
    send_query(&mut client, "INSERT INTO t VALUES (1, 'committed-before')");
    expect_ok(&mut client);

    send_query(&mut client, "BEGIN");
    expect_ok(&mut client);
    send_query(&mut client, "INSERT INTO t VALUES (2, 'should-vanish')");
    expect_ok(&mut client);
    send_query(&mut client, "ROLLBACK");
    expect_ok(&mut client);

    send_query(&mut client, "SELECT * FROM t");
    assert_eq!(
        read_result_set(&mut client),
        vec![vec!["1".to_string(), "committed-before".to_string()]],
        "rollback should have discarded the pending insert and left prior committed data untouched"
    );
}

#[test]
fn commit_and_rollback_with_no_active_transaction_are_harmless_no_ops() {
    let server = TestServer::start(test_config());
    let mut client = server.connect();
    authenticate(&mut client);

    send_query(&mut client, "COMMIT");
    expect_ok(&mut client);
    send_query(&mut client, "ROLLBACK");
    expect_ok(&mut client);

    // The connection is still perfectly usable afterward.
    send_query(&mut client, "SELECT 1");
    assert_eq!(read_result_set(&mut client), vec![vec!["1".to_string()]]);
}

#[test]
fn begin_while_already_in_a_transaction_implicitly_commits_the_first() {
    let server = TestServer::start(test_config());

    let mut a = server.connect();
    authenticate(&mut a);
    send_query(&mut a, "CREATE TABLE t (id INT PRIMARY KEY)");
    expect_ok(&mut a);

    send_query(&mut a, "BEGIN");
    expect_ok(&mut a);
    send_query(&mut a, "INSERT INTO t VALUES (1)");
    expect_ok(&mut a);
    send_query(&mut a, "BEGIN"); // no COMMIT in between
    expect_ok(&mut a);

    let mut b = server.connect();
    authenticate(&mut b);
    send_query(&mut b, "SELECT * FROM t");
    assert_eq!(
        read_result_set(&mut b),
        vec![vec!["1".to_string()]],
        "the first transaction's insert should have been implicitly committed by the second BEGIN"
    );
}

#[test]
fn start_transaction_is_a_synonym_for_begin() {
    let server = TestServer::start(test_config());
    let mut client = server.connect();
    authenticate(&mut client);
    send_query(&mut client, "CREATE TABLE t (id INT PRIMARY KEY)");
    expect_ok(&mut client);

    send_query(&mut client, "START TRANSACTION");
    expect_ok(&mut client);
    send_query(&mut client, "INSERT INTO t VALUES (1)");
    expect_ok(&mut client);
    send_query(&mut client, "COMMIT");
    expect_ok(&mut client);

    send_query(&mut client, "SELECT * FROM t");
    assert_eq!(read_result_set(&mut client), vec![vec!["1".to_string()]]);
}

#[test]
fn failed_statement_inside_a_transaction_does_not_abort_it() {
    let server = TestServer::start(test_config());
    let mut client = server.connect();
    authenticate(&mut client);
    send_query(&mut client, "CREATE TABLE t (id INT PRIMARY KEY)");
    expect_ok(&mut client);

    send_query(&mut client, "BEGIN");
    expect_ok(&mut client);
    send_query(&mut client, "INSERT INTO t VALUES (1)");
    expect_ok(&mut client);
    send_query(&mut client, "SELECT * FROM missing_table"); // fails
    expect_err(&mut client);
    // The transaction is still open and its earlier work still commits.
    send_query(&mut client, "COMMIT");
    expect_ok(&mut client);

    send_query(&mut client, "SELECT * FROM t");
    assert_eq!(read_result_set(&mut client), vec![vec!["1".to_string()]]);
}

/// The Phase 7 acceptance test: two connections write to the same table
/// concurrently. The table lock must serialize them (proving "sufficient to
/// prevent lost updates") rather than let them race, and — since this is
/// read committed, not a blind "last write wins" — both writes must survive.
#[test]
fn concurrent_writers_to_the_same_table_are_serialized_and_neither_update_is_lost() {
    let server = TestServer::start(test_config());

    let mut setup = server.connect();
    authenticate(&mut setup);
    send_query(
        &mut setup,
        "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR)",
    );
    expect_ok(&mut setup);

    let addr = server.addr;
    const HOLD_TIME: Duration = Duration::from_millis(300);

    // Thread A: a slow, multi-statement transaction that holds t's write
    // lock for HOLD_TIME before committing.
    let a = std::thread::spawn(move || {
        let mut client = TestClient::connect(addr);
        authenticate(&mut client);
        send_query(&mut client, "BEGIN");
        expect_ok(&mut client);
        send_query(&mut client, "INSERT INTO t VALUES (1, 'from-a')");
        expect_ok(&mut client);
        std::thread::sleep(HOLD_TIME);
        send_query(&mut client, "COMMIT");
        expect_ok(&mut client);
    });

    // Give A a moment to actually acquire the lock before B tries.
    std::thread::sleep(Duration::from_millis(50));

    // Thread B: a single autocommit INSERT into the same table, started
    // while A is still holding the lock.
    let started = Instant::now();
    let mut b = TestClient::connect(addr);
    authenticate(&mut b);
    send_query(&mut b, "INSERT INTO t VALUES (2, 'from-b')");
    expect_ok(&mut b);
    let waited = started.elapsed();

    a.join().unwrap();

    assert!(
        waited >= Duration::from_millis(200),
        "b's insert completed in {waited:?}, too fast to have waited for a's lock — writes were not serialized"
    );

    // Neither write was lost: both rows are present.
    send_query(&mut b, "SELECT id, name FROM t");
    let mut rows = read_result_set(&mut b);
    rows.sort();
    assert_eq!(
        rows,
        vec![
            vec!["1".to_string(), "from-a".to_string()],
            vec!["2".to_string(), "from-b".to_string()]
        ]
    );
}

#[test]
fn concurrent_transactions_on_different_tables_do_not_block_each_other() {
    let server = TestServer::start(test_config());

    let mut setup = server.connect();
    authenticate(&mut setup);
    send_query(&mut setup, "CREATE TABLE t1 (id INT PRIMARY KEY)");
    expect_ok(&mut setup);
    send_query(&mut setup, "CREATE TABLE t2 (id INT PRIMARY KEY)");
    expect_ok(&mut setup);

    let addr = server.addr;
    const HOLD_TIME: Duration = Duration::from_millis(300);

    let a = std::thread::spawn(move || {
        let mut client = TestClient::connect(addr);
        authenticate(&mut client);
        send_query(&mut client, "BEGIN");
        expect_ok(&mut client);
        send_query(&mut client, "INSERT INTO t1 VALUES (1)");
        expect_ok(&mut client);
        std::thread::sleep(HOLD_TIME);
        send_query(&mut client, "COMMIT");
        expect_ok(&mut client);
    });

    std::thread::sleep(Duration::from_millis(50));

    // A different table entirely: must not be blocked by a's lock on t1.
    let started = Instant::now();
    let mut b = TestClient::connect(addr);
    authenticate(&mut b);
    send_query(&mut b, "INSERT INTO t2 VALUES (1)");
    expect_ok(&mut b);
    let elapsed = started.elapsed();

    a.join().unwrap();

    assert!(
        elapsed < Duration::from_millis(200),
        "writing to an unrelated table took {elapsed:?} — it should not have waited on t1's lock at all"
    );
}
