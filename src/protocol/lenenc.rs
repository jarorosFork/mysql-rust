//! Length-encoded integers, as used throughout the MySQL wire protocol.
//!
//! Reference: <https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_basic_dt_int_fixed.html>

use crate::{Error, Result};

/// Read a length-encoded integer from the start of `buf`.
///
/// Returns the decoded value and the number of bytes it occupied. Never
/// panics, even on truncated or hostile input.
pub fn read_lenenc_int(buf: &[u8]) -> Result<(u64, usize)> {
    let first = *buf
        .first()
        .ok_or_else(|| Error::Protocol("lenenc-int: empty buffer".to_string()))?;

    match first {
        0..=0xfa => Ok((first as u64, 1)),
        0xfb => Err(Error::Protocol(
            "lenenc-int: unexpected NULL marker (0xfb)".to_string(),
        )),
        0xfc => {
            let b = buf
                .get(1..3)
                .ok_or_else(|| Error::Protocol("lenenc-int: truncated 2-byte value".to_string()))?;
            Ok((u16::from_le_bytes([b[0], b[1]]) as u64, 3))
        }
        0xfd => {
            let b = buf
                .get(1..4)
                .ok_or_else(|| Error::Protocol("lenenc-int: truncated 3-byte value".to_string()))?;
            Ok((u32::from_le_bytes([b[0], b[1], b[2], 0]) as u64, 4))
        }
        0xfe => {
            let b = buf
                .get(1..9)
                .ok_or_else(|| Error::Protocol("lenenc-int: truncated 8-byte value".to_string()))?;
            let arr = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
            Ok((u64::from_le_bytes(arr), 9))
        }
        0xff => Err(Error::Protocol(
            "lenenc-int: unexpected error marker (0xff)".to_string(),
        )),
    }
}

/// Encode `value` as a length-encoded integer.
pub fn write_lenenc_int(value: u64) -> Vec<u8> {
    if value < 0xfb {
        vec![value as u8]
    } else if value <= 0xFFFF {
        let mut out = vec![0xfc];
        out.extend_from_slice(&(value as u16).to_le_bytes());
        out
    } else if value <= 0xFF_FFFF {
        let mut out = vec![0xfd];
        out.extend_from_slice(&(value as u32).to_le_bytes()[..3]);
        out
    } else {
        let mut out = vec![0xfe];
        out.extend_from_slice(&value.to_le_bytes());
        out
    }
}

/// Encode `bytes` as a length-encoded string (lenenc-int length + raw bytes,
/// no terminator).
pub fn write_lenenc_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = write_lenenc_int(bytes.len() as u64);
    out.extend_from_slice(bytes);
    out
}

/// Encode `s` as a length-encoded string.
pub fn write_lenenc_str(s: &str) -> Vec<u8> {
    write_lenenc_bytes(s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_byte_values() {
        for v in [0u8, 1, 127, 250] {
            let (value, consumed) = read_lenenc_int(&[v]).unwrap();
            assert_eq!(value, v as u64);
            assert_eq!(consumed, 1);
        }
    }

    #[test]
    fn two_byte_prefix() {
        let (value, consumed) = read_lenenc_int(&[0xfc, 0x2c, 0x01]).unwrap();
        assert_eq!(value, 300);
        assert_eq!(consumed, 3);
    }

    #[test]
    fn three_byte_prefix() {
        let (value, consumed) = read_lenenc_int(&[0xfd, 0x00, 0x00, 0x01]).unwrap();
        assert_eq!(value, 65536);
        assert_eq!(consumed, 4);
    }

    #[test]
    fn eight_byte_prefix() {
        let (value, consumed) = read_lenenc_int(&[0xfe, 1, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(value, 1);
        assert_eq!(consumed, 9);
    }

    #[test]
    fn rejects_null_marker() {
        assert!(read_lenenc_int(&[0xfb]).is_err());
    }

    #[test]
    fn rejects_error_marker() {
        assert!(read_lenenc_int(&[0xff]).is_err());
    }

    #[test]
    fn rejects_empty_buffer() {
        assert!(read_lenenc_int(&[]).is_err());
    }

    #[test]
    fn rejects_truncated_multibyte() {
        assert!(read_lenenc_int(&[0xfc, 0x01]).is_err());
        assert!(read_lenenc_int(&[0xfd, 0x01, 0x02]).is_err());
        assert!(read_lenenc_int(&[0xfe, 1, 2, 3, 4, 5, 6, 7]).is_err());
    }

    #[test]
    fn write_matches_boundary_encodings() {
        assert_eq!(write_lenenc_int(0), vec![0]);
        assert_eq!(write_lenenc_int(250), vec![250]);
        assert_eq!(write_lenenc_int(251), vec![0xfc, 251, 0]);
        assert_eq!(write_lenenc_int(0xFFFF), vec![0xfc, 0xff, 0xff]);
        assert_eq!(write_lenenc_int(0x1_0000), vec![0xfd, 0x00, 0x00, 0x01]);
        assert_eq!(write_lenenc_int(0xFF_FFFF), vec![0xfd, 0xff, 0xff, 0xff]);
        assert_eq!(
            write_lenenc_int(0x100_0000),
            vec![0xfe, 0, 0, 0, 1, 0, 0, 0, 0]
        );
    }

    #[test]
    fn write_then_read_round_trips() {
        for value in [
            0u64,
            1,
            250,
            251,
            300,
            0xFFFF,
            0x1_0000,
            0xFF_FFFF,
            0x100_0000,
            u64::MAX,
        ] {
            let encoded = write_lenenc_int(value);
            let (decoded, consumed) = read_lenenc_int(&encoded).unwrap();
            assert_eq!(decoded, value);
            assert_eq!(consumed, encoded.len());
        }
    }

    #[test]
    fn write_lenenc_str_prefixes_with_length() {
        assert_eq!(write_lenenc_str(""), vec![0]);
        assert_eq!(write_lenenc_str("hi"), vec![2, b'h', b'i']);
    }

    #[test]
    fn write_lenenc_str_handles_long_strings_with_multibyte_prefix() {
        let s = "a".repeat(300);
        let encoded = write_lenenc_str(&s);
        assert_eq!(&encoded[..3], &[0xfc, 0x2c, 0x01]); // lenenc-int 300
        assert_eq!(&encoded[3..], s.as_bytes());
    }
}
