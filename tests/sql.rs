//! Integration test: drives `CREATE TABLE` -> multi-row `INSERT` -> `SELECT
//! ... WHERE` over a real socket after a successful handshake + auth — the
//! Phase 4 acceptance criterion in ROADMAP.md.

mod common;

use mysql_rust::auth::native_password::compute_auth_response;
use mysql_rust::config::{Config, UserCredential};
use mysql_rust::protocol::capabilities::{
    CLIENT_DEPRECATE_EOF, CLIENT_PLUGIN_AUTH, CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION,
};
use mysql_rust::protocol::Packet;

use common::{extract_scramble, spawn_server, TestClient};

const COM_QUERY: u8 = 0x03;

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

/// Expect a bare OK packet (DDL/DML — `CREATE TABLE` / `INSERT`, not a
/// result set) and return its `affected_rows` (the first lenenc-int after
/// the 0x00 header; every value in this test fits in one byte).
fn expect_ok(client: &mut TestClient) -> u64 {
    let packet = client.read_packet();
    assert_eq!(
        packet.payload[0], 0x00,
        "expected an OK packet, got header {:#x}",
        packet.payload[0]
    );
    packet.payload[1] as u64
}

fn expect_err(client: &mut TestClient) {
    let packet = client.read_packet();
    assert_eq!(
        packet.payload[0], 0xff,
        "expected an ERR packet, got header {:#x}",
        packet.payload[0]
    );
}

/// Read a full text-protocol result set under `CLIENT_DEPRECATE_EOF` framing.
fn read_result_set(client: &mut TestClient) -> (Vec<String>, Vec<Vec<String>>) {
    let count_packet = client.read_packet();
    let column_count = count_packet.payload[0] as usize;

    let mut columns = Vec::with_capacity(column_count);
    for _ in 0..column_count {
        columns.push(parse_column_def_name(&client.read_packet().payload));
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
    (columns, rows)
}

/// Column definition packets start with catalog/schema/table/org_table
/// lenenc strings (each a single length byte here, all short) before the
/// name field.
fn parse_column_def_name(payload: &[u8]) -> String {
    let mut pos = 0;
    for _ in 0..4 {
        let len = payload[pos] as usize;
        pos += 1 + len;
    }
    let len = payload[pos] as usize;
    pos += 1;
    String::from_utf8(payload[pos..pos + len].to_vec()).expect("utf8 column name")
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

fn test_config() -> Config {
    Config {
        users: vec![UserCredential::with_password("alice", "s3cret")],
        ..Config::default()
    }
}

#[test]
fn create_insert_select_where_round_trip() {
    let mut client = connect_and_authenticate(test_config());

    send_query(&mut client, "CREATE TABLE users (id INT, name VARCHAR)");
    assert_eq!(expect_ok(&mut client), 0);

    send_query(
        &mut client,
        "INSERT INTO users (id, name) VALUES (1, 'alice'), (2, 'bob'), (3, 'carol')",
    );
    assert_eq!(
        expect_ok(&mut client),
        3,
        "expected 3 affected rows for a 3-row INSERT"
    );

    send_query(&mut client, "SELECT name FROM users WHERE id = 2");
    let (columns, rows) = read_result_set(&mut client);
    assert_eq!(columns, vec!["name".to_string()]);
    assert_eq!(rows, vec![vec!["bob".to_string()]]);

    send_query(&mut client, "SELECT * FROM users WHERE id > 1");
    let (columns, rows) = read_result_set(&mut client);
    assert_eq!(columns, vec!["id".to_string(), "name".to_string()]);
    assert_eq!(
        rows,
        vec![
            vec!["2".to_string(), "bob".to_string()],
            vec!["3".to_string(), "carol".to_string()],
        ]
    );
}

#[test]
fn insert_without_explicit_columns_and_select_star() {
    let mut client = connect_and_authenticate(test_config());

    send_query(&mut client, "CREATE TABLE t (a INT, b VARCHAR)");
    expect_ok(&mut client);

    send_query(&mut client, "INSERT INTO t VALUES (1, 'x')");
    assert_eq!(expect_ok(&mut client), 1);

    send_query(&mut client, "SELECT * FROM t");
    let (_columns, rows) = read_result_set(&mut client);
    assert_eq!(rows, vec![vec!["1".to_string(), "x".to_string()]]);
}

#[test]
fn duplicate_create_table_gets_an_err_and_connection_stays_open() {
    let mut client = connect_and_authenticate(test_config());

    send_query(&mut client, "CREATE TABLE t (a INT)");
    expect_ok(&mut client);

    send_query(&mut client, "CREATE TABLE t (a INT)");
    expect_err(&mut client);

    // The connection must still be usable after an execution error.
    send_query(&mut client, "INSERT INTO t VALUES (1)");
    assert_eq!(expect_ok(&mut client), 1);
}

#[test]
fn select_from_nonexistent_table_gets_an_err() {
    let mut client = connect_and_authenticate(test_config());

    send_query(&mut client, "SELECT * FROM nope");
    expect_err(&mut client);
}

#[test]
fn malformed_sql_gets_a_parse_error_not_a_dropped_connection() {
    let mut client = connect_and_authenticate(test_config());

    send_query(&mut client, "CREATE TALBE t (a INT)"); // typo'd keyword
    expect_err(&mut client);

    // Still usable afterward.
    send_query(&mut client, "SELECT 1");
    let (columns, rows) = read_result_set(&mut client);
    assert_eq!(columns, vec!["1".to_string()]);
    assert_eq!(rows, vec![vec!["1".to_string()]]);
}
