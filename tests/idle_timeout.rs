//! Integration tests for Phase 12 PD-4 P9 (idle-connection reaping):
//! `wait_timeout`/`connect_timeout` are enforced, not just reported.

mod common;

use std::time::Duration;

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

/// Complete the handshake + auth on an already-connected client.
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

/// Read a full text-protocol result set under `CLIENT_DEPRECATE_EOF` framing.
fn read_result_set(client: &mut TestClient) -> Vec<Vec<String>> {
    let count_packet = client.read_packet();
    let column_count = count_packet.payload[0] as usize;

    for _ in 0..column_count {
        let _column_def = client.read_packet();
    }

    let mut rows = Vec::new();
    loop {
        let packet = client.read_packet();
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
fn idle_client_is_disconnected_after_wait_timeout() {
    let config = Config {
        wait_timeout: Duration::from_millis(150),
        ..test_config()
    };
    let server = TestServer::start(config);
    let mut client = server.connect();
    authenticate(&mut client);

    // Send nothing at all -- the server must notice on its own once
    // `wait_timeout` elapses, without the client prompting it. Bounded to 5s
    // so a regression hangs the test for seconds, not forever.
    client.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 16];
    let n = client.read_raw(&mut buf);
    assert_eq!(n, 0, "server should have closed the idle connection");
}

#[test]
fn active_client_survives_past_what_would_otherwise_be_the_wait_timeout() {
    let config = Config {
        wait_timeout: Duration::from_millis(150),
        ..test_config()
    };
    let server = TestServer::start(config);
    let mut client = server.connect();
    authenticate(&mut client);

    // Five round-trips spaced well under `wait_timeout` apart but totalling
    // well past it -- proving the timeout resets on every command instead
    // of capping the connection's total lifetime.
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(50));
        send_query(&mut client, "SELECT 1");
        assert_eq!(read_result_set(&mut client), vec![vec!["1".to_string()]]);
    }
}

#[test]
fn idle_connections_permit_is_released_after_wait_timeout() {
    let config = Config {
        max_connections: 1,
        wait_timeout: Duration::from_millis(150),
        ..test_config()
    };
    let server = TestServer::start(config);

    let mut first = server.connect();
    authenticate(&mut first);
    // `first` now sits idle, holding the one available connection permit.

    // Retry until the idle client is reaped and its permit released --
    // mirrors `concurrency.rs`'s connection_slot_is_released_when_a_client_
    // disconnects, just triggered by wait_timeout instead of an explicit quit.
    let mut accepted = false;
    for _ in 0..50 {
        let mut client = server.connect();
        let first_packet = client.read_packet();
        if first_packet.payload[0] == 10 {
            // A real HandshakeV10, not an immediate "too many connections" ERR.
            accepted = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        accepted,
        "connection permit was never released after the idle client's wait_timeout elapsed"
    );
}

#[test]
fn stalled_handshake_client_is_disconnected_after_connect_timeout() {
    let config = Config {
        connect_timeout: Duration::from_millis(150),
        ..test_config()
    };
    let server = TestServer::start(config);
    let mut client = server.connect();

    // Read the server's greeting but never send a HandshakeResponse41 back
    // -- the server must close the connection once `connect_timeout`
    // elapses rather than waiting forever for a response that never comes.
    let _handshake_packet = client.read_packet();
    client.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 16];
    let n = client.read_raw(&mut buf);
    assert_eq!(
        n, 0,
        "server should have closed the stalled handshake connection"
    );
}
