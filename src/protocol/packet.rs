//! Framing for MySQL protocol packets.
//!
//! Every MySQL packet is a 3-byte little-endian payload length, a 1-byte
//! sequence id, then that many payload bytes.

use crate::{Error, Result};

/// Size of a packet header: 3 length bytes + 1 sequence byte.
pub const HEADER_LEN: usize = 4;

/// Maximum payload that fits in a single wire packet (2^24 - 1). Payloads at
/// or above this size are split across multiple wire packets by the MySQL
/// protocol; that stream-level splitting/reassembly is layered on top of this
/// single-frame type (see the roadmap's protocol-correctness gate).
pub const MAX_PAYLOAD: usize = 0xFF_FF_FF;

/// A single, decoded protocol packet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Packet {
    /// Sequence id, used to detect out-of-order or dropped packets.
    pub sequence_id: u8,
    /// The raw payload, excluding the header.
    pub payload: Vec<u8>,
}

impl Packet {
    /// Build a packet from a sequence id and payload.
    pub fn new(sequence_id: u8, payload: Vec<u8>) -> Self {
        Packet {
            sequence_id,
            payload,
        }
    }

    /// Serialize the packet (header + payload) into bytes.
    ///
    /// The header is a 3-byte little-endian payload length followed by the
    /// 1-byte sequence id. The payload must fit in a single wire frame
    /// ([`MAX_PAYLOAD`]); larger payloads are the writer's responsibility to
    /// split before calling this.
    pub fn encode(&self) -> Vec<u8> {
        debug_assert!(
            self.payload.len() <= MAX_PAYLOAD,
            "payload of {} bytes exceeds single-packet maximum; caller must split",
            self.payload.len()
        );

        let len_bytes = (self.payload.len() as u32).to_le_bytes();
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.extend_from_slice(&len_bytes[..3]);
        out.push(self.sequence_id);
        out.extend_from_slice(&self.payload);
        out
    }

    /// Parse a single complete packet from the front of `bytes`.
    ///
    /// Trailing bytes beyond the first packet are ignored; use [`Packet::parse`]
    /// when you need to know how many bytes were consumed or to handle a buffer
    /// that may not yet hold a full packet.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        match Self::parse(bytes)? {
            Some((packet, _consumed)) => Ok(packet),
            None => Err(Error::Protocol(format!(
                "incomplete packet: have {} byte(s), need a full header plus payload",
                bytes.len()
            ))),
        }
    }

    /// Try to parse one packet from the front of `buf`.
    ///
    /// Returns:
    /// - `Ok(None)` if `buf` does not yet contain a full packet (the caller
    ///   should read more bytes and try again — this is the fragmented-read
    ///   path),
    /// - `Ok(Some((packet, consumed)))` with the packet and the number of bytes
    ///   it occupied, so the caller can advance past it.
    pub fn parse(buf: &[u8]) -> Result<Option<(Packet, usize)>> {
        if buf.len() < HEADER_LEN {
            return Ok(None);
        }

        let length = u32::from_le_bytes([buf[0], buf[1], buf[2], 0]) as usize;
        let sequence_id = buf[3];
        let total = HEADER_LEN + length;

        if buf.len() < total {
            // Header says there is more payload than we have buffered yet.
            return Ok(None);
        }

        let payload = buf[HEADER_LEN..total].to_vec();
        Ok(Some((Packet::new(sequence_id, payload), total)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_typical_payload() {
        let packet = Packet::new(7, b"hello world".to_vec());
        let bytes = packet.encode();
        let decoded = Packet::decode(&bytes).expect("decode");
        assert_eq!(decoded, packet);
    }

    #[test]
    fn round_trip_empty_payload() {
        let packet = Packet::new(0, Vec::new());
        let bytes = packet.encode();
        // Header only: length 0, seq 0.
        assert_eq!(bytes, vec![0, 0, 0, 0]);
        let decoded = Packet::decode(&bytes).expect("decode");
        assert_eq!(decoded, packet);
    }

    #[test]
    fn header_encodes_length_little_endian() {
        // 0x010203 = 66051 bytes of payload -> LE header bytes 03 02 01.
        let len = 0x01_02_03;
        let packet = Packet::new(0xAB, vec![0u8; len]);
        let bytes = packet.encode();
        assert_eq!(&bytes[..4], &[0x03, 0x02, 0x01, 0xAB]);
    }

    #[test]
    fn preserves_sequence_id() {
        for seq in [0u8, 1, 42, 200, 255] {
            let packet = Packet::new(seq, vec![1, 2, 3]);
            let decoded = Packet::decode(&packet.encode()).expect("decode");
            assert_eq!(decoded.sequence_id, seq);
        }
    }

    #[test]
    fn boundary_max_payload_round_trips() {
        // The largest payload expressible in a single 3-byte length field.
        let packet = Packet::new(9, vec![0xEE; MAX_PAYLOAD]);
        let bytes = packet.encode();
        assert_eq!(bytes.len(), HEADER_LEN + MAX_PAYLOAD);
        assert_eq!(&bytes[..3], &[0xFF, 0xFF, 0xFF]);
        let decoded = Packet::decode(&bytes).expect("decode");
        assert_eq!(decoded.payload.len(), MAX_PAYLOAD);
        assert_eq!(decoded, packet);
    }

    #[test]
    fn parse_reports_incomplete_header() {
        // Fewer than HEADER_LEN bytes: need more data, not an error.
        assert_eq!(Packet::parse(&[]).unwrap(), None);
        assert_eq!(Packet::parse(&[0x05, 0x00]).unwrap(), None);
    }

    #[test]
    fn parse_reports_incomplete_payload() {
        // Header claims 5 payload bytes but only 2 are present.
        let buf = [0x05, 0x00, 0x00, 0x01, 0xAA, 0xBB];
        assert_eq!(Packet::parse(&buf).unwrap(), None);
    }

    #[test]
    fn parse_handles_fragmented_reads() {
        // Simulate a socket delivering one packet across several reads.
        let packet = Packet::new(3, b"fragmented".to_vec());
        let full = packet.encode();

        let mut buf: Vec<u8> = Vec::new();
        for (i, byte) in full.iter().enumerate() {
            // Before the last byte arrives, the packet is not yet parseable.
            assert_eq!(Packet::parse(&buf).unwrap(), None, "premature parse at {i}");
            buf.push(*byte);
        }
        let (decoded, consumed) = Packet::parse(&buf).unwrap().expect("complete now");
        assert_eq!(decoded, packet);
        assert_eq!(consumed, full.len());
    }

    #[test]
    fn parse_consumes_only_first_of_several_packets() {
        let first = Packet::new(1, b"one".to_vec());
        let second = Packet::new(2, b"twotwo".to_vec());

        let mut buf = first.encode();
        buf.extend_from_slice(&second.encode());

        let (p1, consumed1) = Packet::parse(&buf).unwrap().expect("first");
        assert_eq!(p1, first);

        let (p2, consumed2) = Packet::parse(&buf[consumed1..]).unwrap().expect("second");
        assert_eq!(p2, second);
        assert_eq!(consumed1 + consumed2, buf.len());
    }

    #[test]
    fn decode_errors_on_incomplete_buffer() {
        let buf = [0x05, 0x00, 0x00, 0x01, 0xAA]; // needs 5 payload bytes, has 1
        assert!(matches!(Packet::decode(&buf), Err(Error::Protocol(_))));
    }
}
