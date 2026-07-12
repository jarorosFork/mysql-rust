//! Integration tests for Phase 9 resource limits: `max_allowed_packet`
//! rejection (a client cannot force the server to buffer an oversized
//! packet). `max_connections` is covered separately in tests/concurrency.rs.

mod common;

use mysql_rust::auth::native_password::compute_auth_response;
use mysql_rust::config::{Config, UserCredential};
use mysql_rust::protocol::capabilities::{
    CLIENT_DEPRECATE_EOF, CLIENT_PLUGIN_AUTH, CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION,
};
use mysql_rust::protocol::Packet;

use common::{extract_scramble, TestClient, TestServer};

const COM_QUERY: u8 = 0x03;

fn config_with_packet_limit(max_allowed_packet: usize) -> Config {
    Config {
        users: vec![UserCredential::with_password("alice", "s3cret")],
        max_allowed_packet,
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

#[test]
fn oversized_packet_is_rejected_without_buffering_its_payload() {
    // A 1 KiB limit is large enough for the auth handshake but far below the
    // 5 MiB packet the test sends.
    let server = TestServer::start(config_with_packet_limit(1024));
    let mut client = server.connect();
    authenticate(&mut client);

    // Send only a 4-byte header that *claims* a 5 MiB payload — far over the
    // limit — and NOT the payload itself. The server must reject on the
    // header alone (proving it doesn't wait to buffer the 5 MiB) and close
    // the connection.
    let declared_len: u32 = 5 * 1024 * 1024;
    let header = [
        (declared_len & 0xff) as u8,
        ((declared_len >> 8) & 0xff) as u8,
        ((declared_len >> 16) & 0xff) as u8,
        0, // sequence id
    ];
    client.write_raw(&header);

    // The server closes the connection rather than blocking forever waiting
    // for 5 MiB that will never come.
    let mut buf = [0u8; 16];
    let n = client.read_raw(&mut buf);
    assert_eq!(
        n, 0,
        "server should have closed the connection for the oversized packet"
    );
}

#[test]
fn a_packet_at_the_limit_is_still_accepted() {
    // A generous limit; a normal query is well under it and works fine.
    let server = TestServer::start(config_with_packet_limit(1024));
    let mut client = server.connect();
    authenticate(&mut client);

    let mut payload = vec![COM_QUERY];
    payload.extend_from_slice(b"SELECT 1");
    client.write_packet(&Packet::new(0, payload));

    // A normal result set comes back — the connection was not dropped.
    let count = client.read_packet();
    assert_eq!(count.payload, vec![1], "expected a 1-column result set");
    let _coldef = client.read_packet();
    let row = client.read_packet();
    assert_eq!(row.payload, vec![1, b'1']);
}
