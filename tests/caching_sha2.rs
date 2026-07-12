//! Integration test for the `caching_sha2_password` auth plugin (MySQL 8.0's
//! default): the server advertises it in the handshake, verifies a client's
//! fast-auth reply, and signals fast-auth-success before the terminal OK. Also
//! covers the auth-switch path when an account uses the other plugin.

mod common;

use mysql_rust::auth::{caching_sha2, native_password};
use mysql_rust::config::{Config, UserCredential};
use mysql_rust::protocol::capabilities::{
    CLIENT_PLUGIN_AUTH, CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION,
};
use mysql_rust::protocol::Packet;

use common::{extract_scramble, spawn_server};

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

/// The auth plugin name is the final NUL-terminated string in a `HandshakeV10`.
fn extract_auth_plugin_name(payload: &[u8]) -> String {
    let body = &payload[..payload.len() - 1]; // drop the trailing NUL
    let start = body
        .iter()
        .rposition(|&b| b == 0)
        .map(|p| p + 1)
        .unwrap_or(0);
    String::from_utf8_lossy(&body[start..]).into_owned()
}

fn caching_sha2_user_config() -> Config {
    Config {
        users: vec![UserCredential::with_caching_sha2_password(
            "alice", "s3cret",
        )],
        ..Config::default()
    }
}

#[test]
fn handshake_advertises_caching_sha2_by_default() {
    let mut client = spawn_server(Config::default());
    let handshake = client.read_packet();
    assert_eq!(
        extract_auth_plugin_name(&handshake.payload),
        "caching_sha2_password",
        "MySQL 8.0's default plugin should be advertised"
    );
}

#[test]
fn fast_auth_succeeds_with_correct_password() {
    let mut client = spawn_server(caching_sha2_user_config());
    let handshake = client.read_packet();
    let scramble = extract_scramble(&handshake.payload);

    let reply = caching_sha2::scramble(Some(b"s3cret"), &scramble);
    let payload = build_response_payload("alice", &reply, "caching_sha2_password");
    client.write_packet(&Packet::new(1, payload));

    // The server signals fast-auth success (AuthMoreData 0x01 0x03)...
    let more = client.read_packet();
    assert_eq!(
        more.payload,
        vec![0x01, 0x03],
        "expected an AuthMoreData fast-auth-success packet"
    );
    // ...then the terminal OK.
    let ok = client.read_packet();
    assert_eq!(ok.payload[0], 0x00, "expected an OK packet after fast auth");
}

#[test]
fn wrong_password_is_denied() {
    let mut client = spawn_server(caching_sha2_user_config());
    let handshake = client.read_packet();
    let scramble = extract_scramble(&handshake.payload);

    let reply = caching_sha2::scramble(Some(b"not-the-password"), &scramble);
    let payload = build_response_payload("alice", &reply, "caching_sha2_password");
    client.write_packet(&Packet::new(1, payload));

    let verdict = client.read_packet();
    assert_eq!(verdict.payload[0], 0xff, "expected an ERR packet");
    assert_eq!(
        &verdict.payload[1..3],
        &1045u16.to_le_bytes(),
        "expected ER_ACCESS_DENIED_ERROR"
    );
}

#[test]
fn passwordless_caching_sha2_account_accepts_empty_reply() {
    let config = Config {
        users: vec![UserCredential::with_caching_sha2_password("guest", "")],
        ..Config::default()
    };
    let mut client = spawn_server(config);
    let _handshake = client.read_packet();

    let payload = build_response_payload("guest", b"", "caching_sha2_password");
    client.write_packet(&Packet::new(1, payload));

    let more = client.read_packet();
    assert_eq!(more.payload, vec![0x01, 0x03], "expected fast-auth success");
    let ok = client.read_packet();
    assert_eq!(ok.payload[0], 0x00, "expected an OK packet");
}

/// A `caching_sha2_password` account whose client first presents
/// `mysql_native_password` must be switched onto its own plugin, then verified.
#[test]
fn auth_switch_onto_caching_sha2_for_the_accounts_plugin() {
    let mut client = spawn_server(caching_sha2_user_config());
    let _handshake = client.read_packet();

    // Client claims native_password; the account is caching_sha2, so the
    // server must reply with an AuthSwitchRequest to caching_sha2_password.
    let payload = build_response_payload("alice", &[], "mysql_native_password");
    client.write_packet(&Packet::new(1, payload));

    let switch = client.read_packet();
    assert_eq!(switch.payload[0], 0xfe, "expected an AuthSwitchRequest");
    let name_end = switch.payload[1..].iter().position(|&b| b == 0).unwrap() + 1;
    assert_eq!(&switch.payload[1..name_end], b"caching_sha2_password");
    let new_scramble = &switch.payload[name_end + 1..];
    assert_eq!(new_scramble.len(), 20, "fresh 20-byte nonce");

    // Recompute against the new nonce and resend as the switch response.
    let reply = caching_sha2::scramble(Some(b"s3cret"), new_scramble);
    client.write_packet(&Packet::new(switch.sequence_id + 1, reply));

    let more = client.read_packet();
    assert_eq!(
        more.payload,
        vec![0x01, 0x03],
        "expected fast-auth success after the switch"
    );
    let ok = client.read_packet();
    assert_eq!(ok.payload[0], 0x00, "expected an OK packet");
}

/// The reverse direction still works: a `mysql_native_password` account,
/// advertised the caching_sha2 default, authenticates by declaring its own
/// plugin (no switch needed since the client presents the account's plugin).
#[test]
fn native_account_authenticates_against_caching_sha2_default() {
    let config = Config {
        users: vec![UserCredential::with_password("bob", "hunter2")],
        ..Config::default()
    };
    let mut client = spawn_server(config);
    let handshake = client.read_packet();
    let scramble = extract_scramble(&handshake.payload);

    let auth_response = native_password::compute_auth_response(Some(b"hunter2"), &scramble);
    let payload = build_response_payload("bob", &auth_response, "mysql_native_password");
    client.write_packet(&Packet::new(1, payload));

    // Native success has no AuthMoreData — the OK comes directly.
    let ok = client.read_packet();
    assert_eq!(ok.payload[0], 0x00, "expected an OK packet for native auth");
}
