//! Integration test: drives a real TCP handshake exchange against
//! `server::Connection`, exercising packet framing, `HandshakeV10` encoding,
//! and `HandshakeResponse41` parsing together over a real socket — the
//! Phase 1 acceptance criterion in ROADMAP.md.

mod common;

use std::net::TcpListener;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use mysql_rust::config::Config;
use mysql_rust::observability::{LogLevel, Observability};
use mysql_rust::protocol::capabilities::{
    CLIENT_CONNECT_WITH_DB, CLIENT_PLUGIN_AUTH, CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA,
    CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION,
};
use mysql_rust::protocol::Packet;
use mysql_rust::server::Connection;
use mysql_rust::storage::InMemoryStorage;

use common::TestClient;

/// Accept one connection on a plain (blocking) std listener, hand it to a
/// fresh tokio runtime as an async stream, and run `body` against it —
/// enough to drive `Connection` methods directly (e.g. `perform_handshake`)
/// without going through the full `Server::serve` accept loop.
fn run_one_connection<F, Fut, T>(listener: TcpListener, config: Config, body: F) -> T
where
    F: FnOnce(Connection) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T>,
    T: Send + 'static,
{
    let (std_stream, _) = listener.accept().expect("accept");
    std_stream.set_nonblocking(true).expect("set nonblocking");
    let runtime = tokio::runtime::Runtime::new().expect("build tokio runtime");
    runtime.block_on(async move {
        let stream = tokio::net::TcpStream::from_std(std_stream).expect("tokio stream from std");
        let conn = Connection::new(
            stream,
            &config,
            1,
            Arc::new(InMemoryStorage::new()),
            Arc::new(Observability::new(LogLevel::Error)),
        );
        body(conn).await
    })
}

#[test]
fn client_gets_past_the_initial_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");

    let config = Config {
        server_version: "8.0.0-mysql-rust-test".to_string(),
        ..Config::default()
    };
    let config_for_thread = config.clone();

    let (tx, rx) = mpsc::channel();
    let server_thread = thread::spawn(move || {
        let result = run_one_connection(listener, config_for_thread, |mut conn| async move {
            conn.perform_handshake().await
        });
        tx.send(result).expect("send result to test thread");
    });

    let mut client = TestClient::connect(addr);

    // Sanity-check the server's HandshakeV10; exact byte-layout is covered
    // by unit tests in protocol::handshake, so just spot-check here.
    let handshake_packet = client.read_packet();
    assert_eq!(handshake_packet.sequence_id, 0);
    assert_eq!(handshake_packet.payload[0], 10, "protocol version");
    assert!(
        handshake_packet
            .payload
            .windows(config.server_version.len())
            .any(|w| w == config.server_version.as_bytes()),
        "handshake payload should contain the configured server version"
    );

    // Build a realistic HandshakeResponse41, as a modern `mysql` client would
    // (protocol 41, plugin auth with length-encoded auth-response, a target
    // database, and mysql_native_password).
    let mut response_payload = Vec::new();
    let caps = CLIENT_PROTOCOL_41
        | CLIENT_PLUGIN_AUTH
        | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
        | CLIENT_SECURE_CONNECTION
        | CLIENT_CONNECT_WITH_DB;
    response_payload.extend_from_slice(&caps.to_le_bytes());
    response_payload.extend_from_slice(&16_777_216u32.to_le_bytes());
    response_payload.push(45);
    response_payload.extend_from_slice(&[0u8; 23]);
    response_payload.extend_from_slice(b"testuser\0");
    let auth_response = [0xAAu8; 20];
    response_payload.push(auth_response.len() as u8);
    response_payload.extend_from_slice(&auth_response);
    response_payload.extend_from_slice(b"testdb\0");
    response_payload.extend_from_slice(b"mysql_native_password\0");

    client.write_packet(&Packet::new(1, response_payload));

    let parsed = rx
        .recv()
        .expect("server thread sent a result")
        .expect("perform_handshake should succeed for a well-formed response");

    assert_eq!(parsed.username, "testuser");
    assert_eq!(parsed.auth_response, auth_response.to_vec());
    assert_eq!(parsed.database.as_deref(), Some("testdb"));
    assert_eq!(
        parsed.auth_plugin_name.as_deref(),
        Some("mysql_native_password")
    );

    server_thread.join().expect("server thread panicked");
}

#[test]
fn malformed_response_yields_protocol_error_not_a_panic() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let config = Config::default();
    let config_for_thread = config.clone();

    let (tx, rx) = mpsc::channel();
    let server_thread = thread::spawn(move || {
        let result = run_one_connection(listener, config_for_thread, |mut conn| async move {
            conn.perform_handshake().await
        });
        tx.send(result).expect("send result to test thread");
    });

    let mut client = TestClient::connect(addr);
    let _handshake_packet = client.read_packet();

    // A response with capability_flags = 0 (no CLIENT_PROTOCOL_41) should be
    // rejected cleanly, not crash the server.
    let bogus_payload = vec![0u8; 32];
    client.write_packet(&Packet::new(1, bogus_payload));

    let result = rx.recv().expect("server thread sent a result");
    assert!(result.is_err(), "expected a protocol error, got {result:?}");

    server_thread.join().expect("server thread panicked");
}
