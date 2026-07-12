//! Integration test for Phase 9 observability: the metrics counters wired
//! into the server actually move as connections open, queries run, errors
//! occur, and connections close.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use mysql_rust::auth::native_password::compute_auth_response;
use mysql_rust::config::{Config, UserCredential};
use mysql_rust::observability::{LogLevel, Observability};
use mysql_rust::protocol::capabilities::{
    CLIENT_DEPRECATE_EOF, CLIENT_PLUGIN_AUTH, CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION,
};
use mysql_rust::protocol::Packet;

use common::{extract_scramble, TestClient, TestServer};

const COM_QUERY: u8 = 0x03;

fn test_config() -> Config {
    Config {
        users: vec![UserCredential::with_password("alice", "s3cret")],
        log_level: LogLevel::Error, // keep test output quiet
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
    assert_eq!(verdict.payload[0], 0x00, "auth should succeed");
}

fn send_query(client: &mut TestClient, sql: &str) {
    let mut payload = vec![COM_QUERY];
    payload.extend_from_slice(sql.as_bytes());
    client.write_packet(&Packet::new(0, payload));
}

fn read_one_result_set(client: &mut TestClient) {
    let count = client.read_packet();
    let column_count = count.payload[0] as usize;
    for _ in 0..column_count {
        let _def = client.read_packet();
    }
    loop {
        let packet = client.read_packet();
        if packet.payload.first() == Some(&0xfe) && packet.payload.len() < 9 {
            break;
        }
    }
}

/// Poll `predicate` until it holds or a short deadline passes — metrics
/// updates on the server thread aren't synchronous with the client's reads.
fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    predicate()
}

#[test]
fn metrics_track_connections_queries_and_errors() {
    let obs = Arc::new(Observability::new(LogLevel::Error));
    let server = TestServer::start_with_observability(test_config(), Arc::clone(&obs));

    {
        let mut client = server.connect();
        authenticate(&mut client);

        // Two successful queries and one that errors.
        send_query(&mut client, "SELECT 1");
        read_one_result_set(&mut client);
        send_query(&mut client, "SELECT 2");
        read_one_result_set(&mut client);

        send_query(&mut client, "SELECT * FROM does_not_exist");
        let err = client.read_packet();
        assert_eq!(err.payload[0], 0xff);

        // With the connection still open, we've seen at least 1 connection and
        // 2 successful queries.
        assert!(wait_until(|| {
            let s = obs.metrics.snapshot();
            s.connections_total >= 1 && s.queries_total >= 2 && s.errors_total >= 1
        }));
        assert!(obs.metrics.snapshot().connections_active >= 1);
    } // client dropped -> connection closes

    // After the client disconnects, active connections falls back to 0 while
    // the cumulative total stays put.
    assert!(
        wait_until(|| obs.metrics.snapshot().connections_active == 0),
        "active connections should return to 0 after the client disconnects"
    );
    let final_snap = obs.metrics.snapshot();
    assert!(final_snap.connections_total >= 1);
    assert!(final_snap.queries_total >= 2);
    assert!(final_snap.errors_total >= 1);
}
