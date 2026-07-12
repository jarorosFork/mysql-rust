//! OK and ERR packets: the generic success/failure responses used
//! throughout the protocol (authentication result, command results, ...).
//!
//! Reference:
//! <https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_basic_ok_packet.html>
//! <https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_basic_err_packet.html>

use crate::protocol::lenenc::write_lenenc_int;
use crate::protocol::packet::{Packet, MAX_PAYLOAD};
use crate::{Error, Result};

const OK_HEADER: u8 = 0x00;
const ERR_HEADER: u8 = 0xff;
/// `AuthMoreData` packet header — extra data mid-authentication.
const AUTH_MORE_DATA_HEADER: u8 = 0x01;

/// `SERVER_STATUS_AUTOCOMMIT`.
const DEFAULT_STATUS_FLAGS: u16 = 0x0002;

/// `caching_sha2_password` fast-auth outcome: authentication succeeded (the
/// stored digest matched), and a terminal OK packet follows. (Its
/// counterpart, `0x04` "perform full authentication", is never sent — this
/// server always holds the verifier and takes the fast path.)
pub const CACHING_SHA2_FAST_AUTH_SUCCESS: u8 = 0x03;

/// A generic "success" response (protocol 4.1 form: status flags + warning count).
#[derive(Debug, Clone)]
pub struct OkPacket {
    pub affected_rows: u64,
    pub last_insert_id: u64,
    pub status_flags: u16,
    pub warnings: u16,
    pub info: String,
}

impl OkPacket {
    pub fn new() -> Self {
        OkPacket {
            affected_rows: 0,
            last_insert_id: 0,
            status_flags: DEFAULT_STATUS_FLAGS,
            warnings: 0,
            info: String::new(),
        }
    }

    pub fn to_packet(&self, sequence_id: u8) -> Result<Packet> {
        let mut payload = vec![OK_HEADER];
        payload.extend_from_slice(&write_lenenc_int(self.affected_rows));
        payload.extend_from_slice(&write_lenenc_int(self.last_insert_id));
        payload.extend_from_slice(&self.status_flags.to_le_bytes());
        payload.extend_from_slice(&self.warnings.to_le_bytes());
        payload.extend_from_slice(self.info.as_bytes());

        if payload.len() > MAX_PAYLOAD {
            return Err(Error::Protocol(format!(
                "OK payload of {} bytes exceeds single-packet maximum",
                payload.len()
            )));
        }
        Ok(Packet::new(sequence_id, payload))
    }
}

impl Default for OkPacket {
    fn default() -> Self {
        Self::new()
    }
}

/// `AuthMoreData` (header `0x01`): server-to-client data during a multi-step
/// authentication exchange. For `caching_sha2_password` the single payload
/// byte carries the fast-auth outcome (see `CACHING_SHA2_FAST_AUTH_SUCCESS`).
///
/// Reference: <https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_connection_phase_packets_protocol_auth_more_data.html>
#[derive(Debug, Clone)]
pub struct AuthMoreData {
    pub data: Vec<u8>,
}

impl AuthMoreData {
    /// The `caching_sha2_password` "fast auth succeeded" signal, sent just
    /// before the terminal OK packet.
    pub fn fast_auth_success() -> Self {
        AuthMoreData {
            data: vec![CACHING_SHA2_FAST_AUTH_SUCCESS],
        }
    }

    pub fn to_packet(&self, sequence_id: u8) -> Result<Packet> {
        let mut payload = vec![AUTH_MORE_DATA_HEADER];
        payload.extend_from_slice(&self.data);

        if payload.len() > MAX_PAYLOAD {
            return Err(Error::Protocol(format!(
                "AuthMoreData payload of {} bytes exceeds single-packet maximum",
                payload.len()
            )));
        }
        Ok(Packet::new(sequence_id, payload))
    }
}

/// A generic "failure" response (protocol 4.1 form: `#` + 5-byte SQLSTATE).
#[derive(Debug, Clone)]
pub struct ErrPacket {
    pub error_code: u16,
    pub sql_state: [u8; 5],
    pub message: String,
}

impl ErrPacket {
    pub fn new(error_code: u16, sql_state: &[u8; 5], message: impl Into<String>) -> Self {
        ErrPacket {
            error_code,
            sql_state: *sql_state,
            message: message.into(),
        }
    }

    /// `ER_ACCESS_DENIED_ERROR` (1045, SQLSTATE 28000).
    pub fn access_denied(username: &str) -> Self {
        ErrPacket::new(
            1045,
            b"28000",
            format!("Access denied for user '{username}'"),
        )
    }

    /// Map a crate-wide [`Error`] onto a reasonable MySQL error
    /// code/SQLSTATE, so any layer's failure can reach the client as a
    /// well-formed ERR packet.
    pub fn from_error(err: &Error) -> Self {
        match err {
            Error::Parse(msg) => ErrPacket::new(
                1064, // ER_PARSE_ERROR
                b"42000",
                format!("You have an error in your SQL syntax: {msg}"),
            ),
            Error::Execution(msg) => ErrPacket::new(1105, b"HY000", msg.clone()),
            Error::Unsupported(msg) => ErrPacket::new(
                1235, // ER_NOT_SUPPORTED_YET
                b"42000",
                format!("This version of mysql-rust doesn't yet support '{msg}'"),
            ),
            Error::Auth(msg) => ErrPacket::new(1045, b"28000", msg.clone()),
            Error::Protocol(msg) => ErrPacket::new(1047, b"08S01", msg.clone()), // ER_UNKNOWN_COM_ERROR
            Error::Io(e) => ErrPacket::new(1105, b"HY000", format!("I/O error: {e}")),
        }
    }

    pub fn to_packet(&self, sequence_id: u8) -> Result<Packet> {
        let mut payload = vec![ERR_HEADER];
        payload.extend_from_slice(&self.error_code.to_le_bytes());
        payload.push(b'#');
        payload.extend_from_slice(&self.sql_state);
        payload.extend_from_slice(self.message.as_bytes());

        if payload.len() > MAX_PAYLOAD {
            return Err(Error::Protocol(format!(
                "ERR payload of {} bytes exceeds single-packet maximum",
                payload.len()
            )));
        }
        Ok(Packet::new(sequence_id, payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_packet_layout() {
        let ok = OkPacket {
            affected_rows: 1,
            last_insert_id: 42,
            status_flags: 0x0002,
            warnings: 0,
            info: String::new(),
        };
        let packet = ok.to_packet(3).expect("encode");
        assert_eq!(packet.sequence_id, 3);
        assert_eq!(
            packet.payload,
            vec![0x00, 0x01, 0x2a, 0x02, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn err_packet_layout() {
        let err = ErrPacket::access_denied("alice");
        let packet = err.to_packet(2).expect("encode");
        assert_eq!(packet.sequence_id, 2);
        assert_eq!(packet.payload[0], 0xff);
        assert_eq!(&packet.payload[1..3], &1045u16.to_le_bytes());
        assert_eq!(packet.payload[3], b'#');
        assert_eq!(&packet.payload[4..9], b"28000");
        assert_eq!(&packet.payload[9..], b"Access denied for user 'alice'");
    }

    #[test]
    fn from_error_maps_unsupported_to_not_supported_yet() {
        let err = ErrPacket::from_error(&Error::Unsupported("foo"));
        assert_eq!(err.error_code, 1235);
        assert_eq!(&err.sql_state, b"42000");
    }

    #[test]
    fn from_error_maps_parse_to_syntax_error() {
        let err = ErrPacket::from_error(&Error::Parse("bad token".to_string()));
        assert_eq!(err.error_code, 1064);
        assert_eq!(&err.sql_state, b"42000");
        assert!(err.message.contains("bad token"));
    }

    #[test]
    fn auth_more_data_fast_auth_success_layout() {
        let packet = AuthMoreData::fast_auth_success()
            .to_packet(2)
            .expect("encode");
        assert_eq!(packet.sequence_id, 2);
        // Header 0x01, then the fast-auth-success marker 0x03.
        assert_eq!(packet.payload, vec![0x01, CACHING_SHA2_FAST_AUTH_SUCCESS]);
    }
}
