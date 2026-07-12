//! Integration test: drives a full handshake + authentication exchange
//! against `server::Connection` over a real socket — the Phase 2 acceptance
//! criterion in ROADMAP.md (successful auth, rejected bad credentials, and
//! the auth-switch path).

mod common;

use mysql_rust::auth::native_password::compute_auth_response;
use mysql_rust::config::{Config, UserCredential};
use mysql_rust::protocol::capabilities::{
    CLIENT_PLUGIN_AUTH, CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION,
};
use mysql_rust::protocol::Packet;

use common::{extract_scramble, spawn_server, TestClient};

fn build_response_payload(username: &str, auth_response: &[u8], plugin_name: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    let caps = CLIENT_PROTOCOL_41 | CLIENT_PLUGIN_AUTH | CLIENT_SECURE_CONNECTION;
    payload.extend_from_slice(&caps.to_le_bytes());
    payload.extend_from_slice(&16_777_216u32.to_le_bytes());
    payload.push(45);
    payload.extend_from_slice(&[0u8; 23]);
    payload.extend_from_slice(username.as_bytes());
    payload.push(0);
    payload.push(auth_response.len() as u8);
    payload.extend_from_slice(auth_response);
    payload.extend_from_slice(plugin_name.as_bytes());
    payload.push(0);
    payload
}

/// Complete the handshake and return the connected client plus its scramble.
fn start_server_and_handshake(config: Config) -> (TestClient, [u8; 20]) {
    let mut client = spawn_server(config);
    let handshake_packet = client.read_packet();
    let scramble = extract_scramble(&handshake_packet.payload);
    (client, scramble)
}

#[test]
fn correct_password_is_accepted() {
    let config = Config {
        users: vec![UserCredential::with_password("alice", "s3cret")],
        ..Config::default()
    };
    let (mut client, scramble) = start_server_and_handshake(config);

    let auth_response = compute_auth_response(Some(b"s3cret"), &scramble);
    let payload = build_response_payload("alice", &auth_response, "mysql_native_password");
    client.write_packet(&Packet::new(1, payload));

    let verdict = client.read_packet();
    assert_eq!(
        verdict.payload[0], 0x00,
        "expected an OK packet, got header byte {:#x}",
        verdict.payload[0]
    );
}

#[test]
fn wrong_password_is_rejected() {
    let config = Config {
        users: vec![UserCredential::with_password("alice", "s3cret")],
        ..Config::default()
    };
    let (mut client, scramble) = start_server_and_handshake(config);

    let auth_response = compute_auth_response(Some(b"wrong-password"), &scramble);
    let payload = build_response_payload("alice", &auth_response, "mysql_native_password");
    client.write_packet(&Packet::new(1, payload));

    let verdict = client.read_packet();
    assert_eq!(verdict.payload[0], 0xff, "expected an ERR packet");
    assert_eq!(
        &verdict.payload[1..3],
        &1045u16.to_le_bytes(),
        "expected ER_ACCESS_DENIED_ERROR"
    );
    assert_eq!(&verdict.payload[4..9], b"28000", "expected SQLSTATE 28000");
}

#[test]
fn unknown_user_is_rejected() {
    let config = Config {
        users: vec![UserCredential::with_password("alice", "s3cret")],
        ..Config::default()
    };
    let (mut client, scramble) = start_server_and_handshake(config);

    let auth_response = compute_auth_response(Some(b"whatever"), &scramble);
    let payload = build_response_payload("bob", &auth_response, "mysql_native_password");
    client.write_packet(&Packet::new(1, payload));

    let verdict = client.read_packet();
    assert_eq!(verdict.payload[0], 0xff, "expected an ERR packet");
}

#[test]
fn passwordless_account_accepts_empty_response() {
    let config = Config {
        users: vec![UserCredential::with_password("guest", "")],
        ..Config::default()
    };
    let (mut client, _scramble) = start_server_and_handshake(config);

    let payload = build_response_payload("guest", b"", "mysql_native_password");
    client.write_packet(&Packet::new(1, payload));

    let verdict = client.read_packet();
    assert_eq!(verdict.payload[0], 0x00, "expected an OK packet");
}

#[test]
fn auth_switch_when_client_declares_different_plugin() {
    let config = Config {
        users: vec![UserCredential::with_password("alice", "s3cret")],
        ..Config::default()
    };
    let (mut client, _initial_scramble) = start_server_and_handshake(config);

    // Claim a plugin the server doesn't want; it must reply with an
    // AuthSwitchRequest instead of trying to verify this response directly.
    let payload = build_response_payload("alice", &[], "some_other_plugin");
    client.write_packet(&Packet::new(1, payload));

    let switch_packet = client.read_packet();
    assert_eq!(
        switch_packet.payload[0], 0xfe,
        "expected an AuthSwitchRequest"
    );

    let name_end = switch_packet.payload[1..]
        .iter()
        .position(|&b| b == 0)
        .unwrap()
        + 1;
    assert_eq!(
        &switch_packet.payload[1..name_end],
        b"mysql_native_password"
    );
    let new_scramble_bytes = &switch_packet.payload[name_end + 1..];
    assert_eq!(new_scramble_bytes.len(), 20);
    let mut new_scramble = [0u8; 20];
    new_scramble.copy_from_slice(new_scramble_bytes);

    let auth_response = compute_auth_response(Some(b"s3cret"), &new_scramble);
    client.write_packet(&Packet::new(switch_packet.sequence_id + 1, auth_response));

    let verdict = client.read_packet();
    assert_eq!(
        verdict.payload[0], 0x00,
        "expected an OK packet after auth switch"
    );
}
