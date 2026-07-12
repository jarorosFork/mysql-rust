//! Prepared-statement protocol: the `COM_STMT_PREPARE_OK` response and
//! decoding of `COM_STMT_EXECUTE` parameter values (binary protocol).
//!
//! Reference:
//! <https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_com_stmt_prepare.html>
//! <https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_com_stmt_execute.html>

use crate::protocol::lenenc::read_lenenc_int;
use crate::protocol::packet::Packet;
use crate::protocol::resultset::Cell;
use crate::{Error, Result};

// MySQL column type codes seen in COM_STMT_EXECUTE parameter type bytes.
const MYSQL_TYPE_TINY: u8 = 0x01;
const MYSQL_TYPE_SHORT: u8 = 0x02;
const MYSQL_TYPE_LONG: u8 = 0x03;
const MYSQL_TYPE_LONGLONG: u8 = 0x08;
const MYSQL_TYPE_STRING: u8 = 0xfe;
const MYSQL_TYPE_VAR_STRING: u8 = 0xfd;
const MYSQL_TYPE_VARCHAR: u8 = 0x0f;
const MYSQL_TYPE_BLOB: u8 = 0xfc;
const MYSQL_TYPE_NULL: u8 = 0x06;

/// The `COM_STMT_PREPARE_OK` response header (the packet that precedes any
/// parameter- and column-definition packets).
pub struct StmtPrepareOk {
    pub statement_id: u32,
    pub num_columns: u16,
    pub num_params: u16,
}

impl StmtPrepareOk {
    pub fn to_packet(&self, sequence_id: u8) -> Result<Packet> {
        let mut payload = vec![0x00]; // status: OK
        payload.extend_from_slice(&self.statement_id.to_le_bytes());
        payload.extend_from_slice(&self.num_columns.to_le_bytes());
        payload.extend_from_slice(&self.num_params.to_le_bytes());
        payload.push(0x00); // reserved / filler
        payload.extend_from_slice(&0u16.to_le_bytes()); // warning_count
        Ok(Packet::new(sequence_id, payload))
    }
}

/// Decode the parameter section of a `COM_STMT_EXECUTE` payload into
/// [`Cell`]s (the connection maps these onto SQL literals to bind).
///
/// `payload` is the whole command payload; the parameter section starts
/// after the fixed 10-byte header (command byte, statement id, flags, and
/// iteration count). `num_params` comes from the prepared statement. Never
/// panics on truncated or hostile input.
pub fn parse_execute_params(payload: &[u8], num_params: usize) -> Result<Vec<Cell>> {
    if num_params == 0 {
        return Ok(Vec::new());
    }

    // Skip the fixed header: statement_id(4) + flags(1) + iteration_count(4),
    // relative to the payload *after* the command byte. The caller passes the
    // full payload including the command byte at [0], so the header is 10 bytes.
    let mut pos = 10;

    let bitmap_len = num_params.div_ceil(8);
    let null_bitmap = payload
        .get(pos..pos + bitmap_len)
        .ok_or_else(|| Error::Protocol("COM_STMT_EXECUTE: truncated NULL bitmap".to_string()))?
        .to_vec();
    pos += bitmap_len;

    let new_params_bound = *payload.get(pos).ok_or_else(|| {
        Error::Protocol("COM_STMT_EXECUTE: missing new-params-bound flag".to_string())
    })?;
    pos += 1;

    // Parameter types are only present when the client (re)binds them. A
    // client that reuses a previous binding without resending types isn't
    // supported here (we don't cache per-statement type state) — require the
    // types on every execute, which every mainstream driver sends.
    if new_params_bound != 1 {
        return Err(Error::Protocol(
            "COM_STMT_EXECUTE without freshly-bound parameter types is not supported".to_string(),
        ));
    }

    let mut types = Vec::with_capacity(num_params);
    for _ in 0..num_params {
        // Each parameter type is 2 bytes (type + unsigned flag); check both
        // are present so a payload truncated mid-type can't overrun `pos`.
        let pair = payload.get(pos..pos + 2).ok_or_else(|| {
            Error::Protocol("COM_STMT_EXECUTE: truncated parameter type".to_string())
        })?;
        types.push(pair[0]); // pair[1] is the unsigned flag, unused here
        pos += 2;
    }

    let mut params = Vec::with_capacity(num_params);
    for (i, &type_byte) in types.iter().enumerate() {
        if is_null(&null_bitmap, i) {
            params.push(Cell::Null);
            continue;
        }
        // `.get(pos..)` rather than `&payload[pos..]` — never panics even if
        // a previous value's length ran `pos` to the very end.
        let rest = payload.get(pos..).ok_or_else(|| {
            Error::Protocol("COM_STMT_EXECUTE: truncated parameter value".to_string())
        })?;
        let (cell, consumed) = decode_binary_value(rest, type_byte)?;
        pos += consumed;
        params.push(cell);
    }

    Ok(params)
}

fn is_null(bitmap: &[u8], i: usize) -> bool {
    bitmap
        .get(i / 8)
        .is_some_and(|byte| byte & (1 << (i % 8)) != 0)
}

fn decode_binary_value(buf: &[u8], type_byte: u8) -> Result<(Cell, usize)> {
    match type_byte {
        MYSQL_TYPE_NULL => Ok((Cell::Null, 0)),
        MYSQL_TYPE_TINY => {
            let b = *buf.first().ok_or_else(truncated)?;
            Ok((Cell::Int(b as i8 as i64), 1))
        }
        MYSQL_TYPE_SHORT => {
            let b = buf.get(..2).ok_or_else(truncated)?;
            Ok((Cell::Int(i16::from_le_bytes([b[0], b[1]]) as i64), 2))
        }
        MYSQL_TYPE_LONG => {
            let b = buf.get(..4).ok_or_else(truncated)?;
            Ok((
                Cell::Int(i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i64),
                4,
            ))
        }
        MYSQL_TYPE_LONGLONG => {
            let b = buf.get(..8).ok_or_else(truncated)?;
            let arr = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
            Ok((Cell::Int(i64::from_le_bytes(arr)), 8))
        }
        MYSQL_TYPE_STRING | MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR | MYSQL_TYPE_BLOB => {
            let (len, len_size) = read_lenenc_int(buf)?;
            let start = len_size;
            let end = start.checked_add(len as usize).ok_or_else(|| {
                Error::Protocol("COM_STMT_EXECUTE: string length overflow".to_string())
            })?;
            let bytes = buf.get(start..end).ok_or_else(|| {
                Error::Protocol("COM_STMT_EXECUTE: truncated string value".to_string())
            })?;
            let s = String::from_utf8(bytes.to_vec()).map_err(|_| {
                Error::Protocol("COM_STMT_EXECUTE: non-UTF-8 string parameter".to_string())
            })?;
            Ok((Cell::Text(s), end))
        }
        other => Err(Error::Protocol(format!(
            "COM_STMT_EXECUTE: unsupported parameter type {other:#x}"
        ))),
    }
}

fn truncated() -> Error {
    Error::Protocol("COM_STMT_EXECUTE: truncated parameter value".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_ok_layout() {
        let ok = StmtPrepareOk {
            statement_id: 1,
            num_columns: 0,
            num_params: 2,
        };
        let packet = ok.to_packet(1).expect("encode");
        assert_eq!(packet.sequence_id, 1);
        assert_eq!(packet.payload[0], 0x00); // OK status
        assert_eq!(&packet.payload[1..5], &1u32.to_le_bytes()); // statement id
        assert_eq!(&packet.payload[5..7], &0u16.to_le_bytes()); // num_columns
        assert_eq!(&packet.payload[7..9], &2u16.to_le_bytes()); // num_params
        assert_eq!(packet.payload.len(), 12);
    }

    /// Build a COM_STMT_EXECUTE payload: command byte + statement_id +
    /// flags + iteration_count + NULL bitmap + new-params-bound + types + values.
    fn execute_payload(bitmap: &[u8], types_and_values: Vec<(u8, Vec<u8>)>) -> Vec<u8> {
        let mut p = vec![0x17]; // COM_STMT_EXECUTE
        p.extend_from_slice(&1u32.to_le_bytes()); // statement_id
        p.push(0); // flags
        p.extend_from_slice(&1u32.to_le_bytes()); // iteration_count
        p.extend_from_slice(bitmap);
        p.push(1); // new_params_bound
        for (ty, _) in &types_and_values {
            p.push(*ty);
            p.push(0); // unsigned flag
        }
        for (_, value) in &types_and_values {
            p.extend_from_slice(value);
        }
        p
    }

    #[test]
    fn decodes_longlong_and_string_params() {
        let payload = execute_payload(
            &[0x00],
            vec![
                (MYSQL_TYPE_LONGLONG, 42i64.to_le_bytes().to_vec()),
                (MYSQL_TYPE_VAR_STRING, {
                    let mut v = vec![3];
                    v.extend_from_slice(b"abc");
                    v
                }),
            ],
        );
        let params = parse_execute_params(&payload, 2).expect("parse");
        assert_eq!(params, vec![Cell::Int(42), Cell::Text("abc".to_string())]);
    }

    #[test]
    fn respects_null_bitmap() {
        // Param 0 NULL (bit 0 set), param 1 = 7 (LONG).
        let payload = execute_payload(
            &[0x01],
            vec![
                (MYSQL_TYPE_NULL, vec![]),
                (MYSQL_TYPE_LONG, 7i32.to_le_bytes().to_vec()),
            ],
        );
        let params = parse_execute_params(&payload, 2).expect("parse");
        assert_eq!(params, vec![Cell::Null, Cell::Int(7)]);
    }

    #[test]
    fn zero_params_returns_empty() {
        let payload = vec![0x17, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(parse_execute_params(&payload, 0).unwrap(), Vec::new());
    }

    #[test]
    fn truncated_value_errors_not_panics() {
        // Declares a LONGLONG but provides only 3 bytes.
        let mut payload = vec![0x17];
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.push(0);
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.push(0x00); // bitmap
        payload.push(1); // new_params_bound
        payload.push(MYSQL_TYPE_LONGLONG);
        payload.push(0);
        payload.extend_from_slice(&[1, 2, 3]); // too short
        assert!(parse_execute_params(&payload, 1).is_err());
    }

    #[test]
    fn payload_truncated_mid_type_errors_not_panics() {
        // Header + bitmap + new_params_bound + the type byte of the single
        // parameter but NOT its unsigned-flag byte — used to overrun `pos`
        // and panic on the value slice. Must be a clean error now.
        let mut payload = vec![0x17];
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.push(0);
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.push(0x00); // bitmap
        payload.push(1); // new_params_bound
        payload.push(MYSQL_TYPE_LONGLONG); // type byte, but no unsigned flag after it
        assert!(parse_execute_params(&payload, 1).is_err());
    }

    #[test]
    fn payload_missing_all_values_errors_not_panics() {
        // Types present for one non-NULL param, but zero value bytes follow.
        let mut payload = vec![0x17];
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.push(0);
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.push(0x00); // bitmap: param not NULL
        payload.push(1); // new_params_bound
        payload.push(MYSQL_TYPE_LONGLONG);
        payload.push(0); // unsigned flag — but no value bytes at all
        assert!(parse_execute_params(&payload, 1).is_err());
    }
}
