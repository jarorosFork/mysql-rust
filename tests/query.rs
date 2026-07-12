//! Integration test: drives the command phase (`COM_QUERY`, `COM_PING`,
//! `COM_QUIT`) over a real socket after a successful handshake + auth — the
//! Phase 3 acceptance criterion in ROADMAP.md.

mod common;

use mysql_rust::auth::native_password::compute_auth_response;
use mysql_rust::config::{Config, UserCredential};
use mysql_rust::protocol::capabilities::{
    CLIENT_DEPRECATE_EOF, CLIENT_PLUGIN_AUTH, CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION,
};
use mysql_rust::protocol::Packet;

use common::{extract_scramble, spawn_server, TestClient};

const COM_QUIT: u8 = 0x01;
const COM_QUERY: u8 = 0x03;
const COM_PING: u8 = 0x0e;

/// Complete the handshake and authenticate as `username`, negotiating
/// `CLIENT_DEPRECATE_EOF` (as a real 8.0 client would). Returns the
/// connected client, ready for the command phase.
fn connect_and_authenticate(config: Config, username: &str, password: &str) -> TestClient {
    let mut client = spawn_server(config);
    let handshake_packet = client.read_packet();
    let scramble = extract_scramble(&handshake_packet.payload);

    let auth_response = compute_auth_response(Some(password.as_bytes()), &scramble);
    let caps =
        CLIENT_PROTOCOL_41 | CLIENT_PLUGIN_AUTH | CLIENT_SECURE_CONNECTION | CLIENT_DEPRECATE_EOF;
    let mut payload = Vec::new();
    payload.extend_from_slice(&caps.to_le_bytes());
    payload.extend_from_slice(&16_777_216u32.to_le_bytes());
    payload.push(45);
    payload.extend_from_slice(&[0u8; 23]);
    payload.extend_from_slice(username.as_bytes());
    payload.push(0);
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

/// Read a full text-protocol result set (column count, column defs, rows,
/// trailing OK) under `CLIENT_DEPRECATE_EOF` framing, returning row values.
fn read_result_set(client: &mut TestClient) -> Vec<Vec<String>> {
    let count_packet = client.read_packet();
    let column_count = count_packet.payload[0] as usize; // small counts fit in one lenenc byte

    for _ in 0..column_count {
        let _column_def = client.read_packet();
    }

    let mut rows = Vec::new();
    loop {
        let packet = client.read_packet();
        // Under CLIENT_DEPRECATE_EOF the result-set terminator is an OK
        // packet carrying the 0xFE header (length < 9).
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
        let len = payload[pos] as usize; // every value in these tests is short
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
fn select_1_returns_a_single_row() {
    let mut client = connect_and_authenticate(test_config(), "alice", "s3cret");

    send_query(&mut client, "SELECT 1");
    assert_eq!(read_result_set(&mut client), vec![vec!["1".to_string()]]);
}

#[test]
fn select_version_returns_the_configured_server_version() {
    let config = Config {
        server_version: "8.0.0-mysql-rust-itest".to_string(),
        ..test_config()
    };
    let mut client = connect_and_authenticate(config, "alice", "s3cret");

    send_query(&mut client, "SELECT @@version");
    assert_eq!(
        read_result_set(&mut client),
        vec![vec!["8.0.0-mysql-rust-itest".to_string()]]
    );
}

#[test]
fn queries_are_case_and_whitespace_insensitive() {
    let mut client = connect_and_authenticate(test_config(), "alice", "s3cret");

    send_query(&mut client, "  select 1 ; ");
    assert_eq!(read_result_set(&mut client), vec![vec!["1".to_string()]]);
}

#[test]
fn ping_succeeds() {
    let mut client = connect_and_authenticate(test_config(), "alice", "s3cret");

    client.write_packet(&Packet::new(0, vec![COM_PING]));
    let verdict = client.read_packet();
    assert_eq!(
        verdict.payload[0], 0x00,
        "expected an OK packet for COM_PING"
    );
}

#[test]
fn unsupported_query_gets_an_err_packet_and_connection_stays_open() {
    let mut client = connect_and_authenticate(test_config(), "alice", "s3cret");

    send_query(&mut client, "SELECT * FROM some_table");
    let verdict = client.read_packet();
    assert_eq!(verdict.payload[0], 0xff, "expected an ERR packet");

    // A bad query must not drop the connection; the next command still works.
    send_query(&mut client, "SELECT 1");
    assert_eq!(read_result_set(&mut client), vec![vec!["1".to_string()]]);
}

#[test]
fn quit_closes_the_connection_cleanly() {
    let mut client = connect_and_authenticate(test_config(), "alice", "s3cret");

    client.write_packet(&Packet::new(0, vec![COM_QUIT]));

    let mut buf = [0u8; 16];
    let n = client.read_raw(&mut buf);
    assert_eq!(
        n, 0,
        "expected the server to close the connection after COM_QUIT"
    );
}
