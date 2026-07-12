//! The initial handshake exchange.
//!
//! On connect the server sends a `HandshakeV10` packet; the client replies
//! with a `HandshakeResponse41`. This module builds the former and parses
//! the latter.
//!
//! Reference: <https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_connection_phase_packets_protocol_handshake_v10.html>

use crate::protocol::capabilities::{
    CLIENT_CONNECT_WITH_DB, CLIENT_DEPRECATE_EOF, CLIENT_LONG_PASSWORD, CLIENT_MULTI_STATEMENTS,
    CLIENT_PLUGIN_AUTH, CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA, CLIENT_PROTOCOL_41,
    CLIENT_SECURE_CONNECTION, CLIENT_SSL,
};
use crate::protocol::lenenc::read_lenenc_int;
use crate::protocol::packet::{Packet, MAX_PAYLOAD};
use crate::{Error, Result};

/// Length in bytes of the `mysql_native_password` auth challenge ("scramble").
pub const SCRAMBLE_LEN: usize = 20;

/// `utf8mb4_general_ci`; a broadly-compatible default collation id to report
/// in the handshake before the client picks its own charset.
const DEFAULT_CHARACTER_SET: u8 = 45;
/// `SERVER_STATUS_AUTOCOMMIT`.
const DEFAULT_STATUS_FLAGS: u16 = 0x0002;

/// The server's initial handshake (protocol version 10).
#[derive(Debug, Clone)]
pub struct Handshake {
    pub protocol_version: u8,
    pub server_version: String,
    pub connection_id: u32,
    /// The random auth challenge sent to the client ("scramble").
    pub auth_plugin_data: [u8; SCRAMBLE_LEN],
    pub capability_flags: u32,
    pub character_set: u8,
    pub status_flags: u16,
    pub auth_plugin_name: String,
}

impl Handshake {
    /// Build a handshake with the server's standard capabilities and a fresh
    /// random scramble. `tls_available` adds `CLIENT_SSL` so clients may
    /// request a TLS upgrade. `auth_plugin_name` is the plugin the server
    /// advertises as its default (see `Config::default_auth_plugin`); clients
    /// compute their first auth-response with it.
    pub fn new(
        connection_id: u32,
        server_version: String,
        tls_available: bool,
        auth_plugin_name: &str,
    ) -> Self {
        let mut capability_flags = CLIENT_LONG_PASSWORD
            | CLIENT_CONNECT_WITH_DB
            | CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
            | CLIENT_DEPRECATE_EOF
            | CLIENT_MULTI_STATEMENTS;
        if tls_available {
            capability_flags |= CLIENT_SSL;
        }
        Handshake {
            protocol_version: 10,
            server_version,
            connection_id,
            auth_plugin_data: generate_scramble(),
            capability_flags,
            character_set: DEFAULT_CHARACTER_SET,
            status_flags: DEFAULT_STATUS_FLAGS,
            auth_plugin_name: auth_plugin_name.to_string(),
        }
    }

    /// Encode this handshake into a protocol packet with the given sequence id.
    pub fn to_packet(&self, sequence_id: u8) -> Result<Packet> {
        let mut payload = Vec::new();

        payload.push(self.protocol_version);
        payload.extend_from_slice(self.server_version.as_bytes());
        payload.push(0); // NUL terminator

        payload.extend_from_slice(&self.connection_id.to_le_bytes());

        payload.extend_from_slice(&self.auth_plugin_data[..8]);
        payload.push(0); // filler

        let cap_lower = (self.capability_flags & 0xFFFF) as u16;
        let cap_upper = ((self.capability_flags >> 16) & 0xFFFF) as u16;

        payload.extend_from_slice(&cap_lower.to_le_bytes());
        payload.push(self.character_set);
        payload.extend_from_slice(&self.status_flags.to_le_bytes());
        payload.extend_from_slice(&cap_upper.to_le_bytes());

        if self.capability_flags & CLIENT_PLUGIN_AUTH != 0 {
            // Total auth-plugin-data length, including its own NUL terminator.
            payload.push((self.auth_plugin_data.len() + 1) as u8);
        } else {
            payload.push(0);
        }

        payload.extend_from_slice(&[0u8; 10]); // reserved

        if self.capability_flags & CLIENT_SECURE_CONNECTION != 0 {
            payload.extend_from_slice(&self.auth_plugin_data[8..]);
            payload.push(0); // NUL terminator for part 2
        }

        if self.capability_flags & CLIENT_PLUGIN_AUTH != 0 {
            payload.extend_from_slice(self.auth_plugin_name.as_bytes());
            payload.push(0);
        }

        if payload.len() > MAX_PAYLOAD {
            return Err(Error::Protocol(format!(
                "handshake payload of {} bytes exceeds single-packet maximum",
                payload.len()
            )));
        }

        Ok(Packet::new(sequence_id, payload))
    }
}

/// The client's reply to `HandshakeV10`, protocol 4.1 form.
#[derive(Debug, Clone)]
pub struct HandshakeResponse41 {
    pub capability_flags: u32,
    pub max_packet_size: u32,
    pub character_set: u8,
    pub username: String,
    pub auth_response: Vec<u8>,
    pub database: Option<String>,
    pub auth_plugin_name: Option<String>,
}

impl HandshakeResponse41 {
    /// Parse a `HandshakeResponse41` from a packet payload. Never panics,
    /// even on truncated or hostile input.
    pub fn parse(payload: &[u8]) -> Result<Self> {
        // 4 (capabilities) + 4 (max packet size) + 1 (charset) + 23 (reserved).
        const HEADER_LEN: usize = 32;

        if payload.len() < HEADER_LEN {
            return Err(Error::Protocol(format!(
                "handshake response too short: {} byte(s), need at least {HEADER_LEN}",
                payload.len()
            )));
        }

        let capability_flags = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
        let max_packet_size = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
        let character_set = payload[8];
        // payload[9..32]: 23 reserved bytes, ignored.

        if capability_flags & CLIENT_PROTOCOL_41 == 0 {
            return Err(Error::Protocol(
                "client did not advertise CLIENT_PROTOCOL_41; the pre-4.1 protocol is not supported"
                    .to_string(),
            ));
        }

        let (username, mut pos) = read_nul_terminated_str(payload, HEADER_LEN)?;

        let auth_response = if capability_flags & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA != 0 {
            let rest = payload.get(pos..).ok_or_else(|| {
                Error::Protocol("handshake response: missing auth-response".to_string())
            })?;
            let (len, consumed) = read_lenenc_int(rest)?;
            pos = pos.checked_add(consumed).ok_or_else(|| {
                Error::Protocol("handshake response: length overflow".to_string())
            })?;
            let end = pos.checked_add(len as usize).ok_or_else(|| {
                Error::Protocol("handshake response: length overflow".to_string())
            })?;
            let bytes = payload.get(pos..end).ok_or_else(|| {
                Error::Protocol("handshake response: truncated auth-response".to_string())
            })?;
            pos = end;
            bytes.to_vec()
        } else if capability_flags & CLIENT_SECURE_CONNECTION != 0 {
            let len = *payload.get(pos).ok_or_else(|| {
                Error::Protocol("handshake response: missing auth-response length".to_string())
            })? as usize;
            pos = pos.checked_add(1).ok_or_else(|| {
                Error::Protocol("handshake response: length overflow".to_string())
            })?;
            let end = pos.checked_add(len).ok_or_else(|| {
                Error::Protocol("handshake response: length overflow".to_string())
            })?;
            let bytes = payload.get(pos..end).ok_or_else(|| {
                Error::Protocol("handshake response: truncated auth-response".to_string())
            })?;
            pos = end;
            bytes.to_vec()
        } else {
            let (bytes, next_pos) = read_nul_terminated_bytes(payload, pos)?;
            pos = next_pos;
            bytes
        };

        let database = if capability_flags & CLIENT_CONNECT_WITH_DB != 0 {
            let (db, next_pos) = read_nul_terminated_str(payload, pos)?;
            pos = next_pos;
            Some(db)
        } else {
            None
        };

        let auth_plugin_name = if capability_flags & CLIENT_PLUGIN_AUTH != 0 {
            let (name, next_pos) = read_nul_terminated_str(payload, pos)?;
            pos = next_pos;
            Some(name)
        } else {
            None
        };

        // Any trailing bytes (e.g. CLIENT_CONNECT_ATTRS key/value pairs) are
        // intentionally left unparsed for now; packet framing already
        // consumed the whole payload as one unit, so ignoring the remainder
        // here does not desync the stream.
        let _ = pos;

        Ok(HandshakeResponse41 {
            capability_flags,
            max_packet_size,
            character_set,
            username,
            auth_response,
            database,
            auth_plugin_name,
        })
    }
}

/// Sent when the client's chosen (or default) auth plugin isn't the one the
/// server wants to use, asking it to retry with a different plugin and a
/// fresh scramble.
///
/// Reference: <https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_connection_phase_packets_protocol_auth_switch_request.html>
#[derive(Debug, Clone)]
pub struct AuthSwitchRequest {
    pub plugin_name: String,
    pub auth_plugin_data: Vec<u8>,
}

impl AuthSwitchRequest {
    pub fn to_packet(&self, sequence_id: u8) -> Result<Packet> {
        let mut payload = vec![0xfe];
        payload.extend_from_slice(self.plugin_name.as_bytes());
        payload.push(0);
        payload.extend_from_slice(&self.auth_plugin_data);

        if payload.len() > MAX_PAYLOAD {
            return Err(Error::Protocol(format!(
                "auth switch payload of {} bytes exceeds single-packet maximum",
                payload.len()
            )));
        }
        Ok(Packet::new(sequence_id, payload))
    }
}

fn read_nul_terminated_bytes(buf: &[u8], start: usize) -> Result<(Vec<u8>, usize)> {
    let rest = buf
        .get(start..)
        .ok_or_else(|| Error::Protocol("unexpected end of handshake response".to_string()))?;
    let end = rest
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| Error::Protocol("expected NUL-terminated data, found none".to_string()))?;
    Ok((rest[..end].to_vec(), start + end + 1))
}

fn read_nul_terminated_str(buf: &[u8], start: usize) -> Result<(String, usize)> {
    let (bytes, next) = read_nul_terminated_bytes(buf, start)?;
    let s = String::from_utf8(bytes).map_err(|_| {
        Error::Protocol("expected a UTF-8 string in handshake response".to_string())
    })?;
    Ok((s, next))
}

/// Generate a fresh scramble for the auth challenge.
///
/// Bytes are mapped into the printable ASCII range `33..=126`, matching
/// MySQL's own `generate_user_salt`, so the scramble is safe to embed as a
/// NUL-terminated string component on the wire. This crate is deliberately
/// dependency-free (see CLAUDE.md), so unpredictability comes from std's
/// OS-seeded `RandomState` rather than a dedicated CSPRNG; replace this with
/// a real CSPRNG (e.g. the `rand` crate) when hardening auth in Phase 9.
pub(crate) fn generate_scramble() -> [u8; SCRAMBLE_LEN] {
    let raw = random_bytes::<SCRAMBLE_LEN>();
    let mut scramble = [0u8; SCRAMBLE_LEN];
    for (out, &b) in scramble.iter_mut().zip(raw.iter()) {
        *out = 33 + (b as u16 * 94 / 256) as u8;
    }
    scramble
}

fn random_bytes<const N: usize>() -> [u8; N] {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let mut bytes = [0u8; N];
    let mut i = 0;
    while i < N {
        // A fresh `RandomState` draws new OS-seeded keys on each call.
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_usize(i);
        for b in hasher.finish().to_le_bytes() {
            if i >= N {
                break;
            }
            bytes[i] = b;
            i += 1;
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_packet_encodes_known_layout() {
        let handshake = Handshake {
            protocol_version: 10,
            server_version: "8.0.0-test".to_string(),
            connection_id: 42,
            auth_plugin_data: [
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
            ],
            capability_flags: CLIENT_PROTOCOL_41 | CLIENT_PLUGIN_AUTH | CLIENT_SECURE_CONNECTION,
            character_set: 45,
            status_flags: 0x0002,
            auth_plugin_name: "mysql_native_password".to_string(),
        };

        let packet = handshake.to_packet(0).expect("encode");
        assert_eq!(packet.sequence_id, 0);

        let payload = &packet.payload;
        let mut pos = 0;

        assert_eq!(payload[pos], 10);
        pos += 1;

        let version_end = payload[pos..].iter().position(|&b| b == 0).unwrap() + pos;
        assert_eq!(&payload[pos..version_end], b"8.0.0-test");
        pos = version_end + 1;

        assert_eq!(&payload[pos..pos + 4], &42u32.to_le_bytes());
        pos += 4;

        assert_eq!(&payload[pos..pos + 8], &[1, 2, 3, 4, 5, 6, 7, 8]);
        pos += 8;

        assert_eq!(payload[pos], 0); // filler
        pos += 1;

        let expected_caps = CLIENT_PROTOCOL_41 | CLIENT_PLUGIN_AUTH | CLIENT_SECURE_CONNECTION;
        assert_eq!(
            &payload[pos..pos + 2],
            &((expected_caps & 0xFFFF) as u16).to_le_bytes()
        );
        pos += 2;

        assert_eq!(payload[pos], 45);
        pos += 1;

        assert_eq!(&payload[pos..pos + 2], &0x0002u16.to_le_bytes());
        pos += 2;

        assert_eq!(
            &payload[pos..pos + 2],
            &(((expected_caps >> 16) & 0xFFFF) as u16).to_le_bytes()
        );
        pos += 2;

        assert_eq!(payload[pos], 21); // scramble len: 20 + NUL
        pos += 1;

        assert_eq!(&payload[pos..pos + 10], &[0u8; 10]);
        pos += 10;

        assert_eq!(
            &payload[pos..pos + 12],
            &[9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20]
        );
        pos += 12;
        assert_eq!(payload[pos], 0); // NUL terminating part 2
        pos += 1;

        let name_end = payload[pos..].iter().position(|&b| b == 0).unwrap() + pos;
        assert_eq!(&payload[pos..name_end], b"mysql_native_password");
        pos = name_end + 1;

        assert_eq!(pos, payload.len());
    }

    #[test]
    fn to_packet_omits_plugin_data_when_capabilities_disabled() {
        let handshake = Handshake {
            protocol_version: 10,
            server_version: "x".to_string(),
            connection_id: 1,
            auth_plugin_data: [0u8; SCRAMBLE_LEN],
            capability_flags: CLIENT_PROTOCOL_41, // no PLUGIN_AUTH, no SECURE_CONNECTION
            character_set: 0,
            status_flags: 0,
            auth_plugin_name: "unused".to_string(),
        };

        let packet = handshake.to_packet(0).expect("encode");
        // "x\0" + 4 (conn id) + 8 (part1) + 1 (filler) + 2 (caps lo) + 1
        // (charset) + 2 (status) + 2 (caps hi) + 1 (auth len = 0) + 10
        // (reserved) = 33 bytes, no part-2 and no plugin name.
        assert_eq!(
            packet.payload.len(),
            1 + 2 + 4 + 8 + 1 + 2 + 1 + 2 + 2 + 1 + 10
        );
        assert_eq!(*packet.payload.last().unwrap(), 0); // last reserved byte
    }

    #[test]
    fn auth_switch_request_layout() {
        const PLUGIN: &str = "mysql_native_password";
        let switch = AuthSwitchRequest {
            plugin_name: PLUGIN.to_string(),
            auth_plugin_data: vec![9u8; SCRAMBLE_LEN],
        };
        let packet = switch.to_packet(2).expect("encode");
        assert_eq!(packet.sequence_id, 2);
        assert_eq!(packet.payload[0], 0xfe);
        assert_eq!(&packet.payload[1..1 + PLUGIN.len()], PLUGIN.as_bytes());
        let name_end = 1 + PLUGIN.len();
        assert_eq!(packet.payload[name_end], 0); // NUL terminator
        assert_eq!(&packet.payload[name_end + 1..], &[9u8; SCRAMBLE_LEN][..]);
    }

    #[test]
    fn generate_scramble_is_printable_and_varies() {
        let a = generate_scramble();
        let b = generate_scramble();
        for byte in a.iter().chain(b.iter()) {
            assert!((33..=126).contains(byte), "byte {byte} not printable ASCII");
        }
        assert_ne!(a, b, "two scrambles should not collide");
    }

    fn secure_connection_caps() -> u32 {
        CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION
    }

    #[test]
    fn parse_lenenc_auth_response_and_optional_fields() {
        let mut payload = Vec::new();
        let caps = CLIENT_PROTOCOL_41
            | CLIENT_PLUGIN_AUTH
            | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
            | CLIENT_CONNECT_WITH_DB;
        payload.extend_from_slice(&caps.to_le_bytes());
        payload.extend_from_slice(&16_777_216u32.to_le_bytes());
        payload.push(45);
        payload.extend_from_slice(&[0u8; 23]);
        payload.extend_from_slice(b"root\0");
        payload.push(4); // lenenc-int single-byte form: length 4
        payload.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
        payload.extend_from_slice(b"mydb\0");
        payload.extend_from_slice(b"mysql_native_password\0");

        let parsed = HandshakeResponse41::parse(&payload).expect("parse");
        assert_eq!(parsed.capability_flags, caps);
        assert_eq!(parsed.max_packet_size, 16_777_216);
        assert_eq!(parsed.character_set, 45);
        assert_eq!(parsed.username, "root");
        assert_eq!(parsed.auth_response, vec![0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(parsed.database.as_deref(), Some("mydb"));
        assert_eq!(
            parsed.auth_plugin_name.as_deref(),
            Some("mysql_native_password")
        );
    }

    #[test]
    fn parse_secure_connection_auth_response_without_optional_fields() {
        let mut payload = Vec::new();
        let caps = secure_connection_caps();
        payload.extend_from_slice(&caps.to_le_bytes());
        payload.extend_from_slice(&1024u32.to_le_bytes());
        payload.push(8);
        payload.extend_from_slice(&[0u8; 23]);
        payload.extend_from_slice(b"alice\0");
        payload.push(3);
        payload.extend_from_slice(&[1, 2, 3]);

        let parsed = HandshakeResponse41::parse(&payload).expect("parse");
        assert_eq!(parsed.username, "alice");
        assert_eq!(parsed.auth_response, vec![1, 2, 3]);
        assert_eq!(parsed.database, None);
        assert_eq!(parsed.auth_plugin_name, None);
    }

    #[test]
    fn parse_rejects_too_short_payload() {
        let payload = vec![0u8; 10];
        assert!(matches!(
            HandshakeResponse41::parse(&payload),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn parse_rejects_missing_protocol_41() {
        let payload = vec![0u8; 32]; // capability_flags = 0 -> PROTOCOL_41 bit unset
        assert!(matches!(
            HandshakeResponse41::parse(&payload),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn parse_rejects_truncated_auth_response() {
        let mut payload = Vec::new();
        let caps = secure_connection_caps();
        payload.extend_from_slice(&caps.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.push(45);
        payload.extend_from_slice(&[0u8; 23]);
        payload.extend_from_slice(b"root\0");
        payload.push(20); // claims 20 bytes...
        payload.extend_from_slice(&[0xAA, 0xBB]); // ...but only provides 2

        assert!(matches!(
            HandshakeResponse41::parse(&payload),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn parse_rejects_non_utf8_username() {
        let mut payload = Vec::new();
        let caps = secure_connection_caps();
        payload.extend_from_slice(&caps.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.push(45);
        payload.extend_from_slice(&[0u8; 23]);
        payload.extend_from_slice(&[0xFF, 0xFE, 0x00]); // invalid UTF-8, NUL-terminated

        assert!(matches!(
            HandshakeResponse41::parse(&payload),
            Err(Error::Protocol(_))
        ));
    }
}
