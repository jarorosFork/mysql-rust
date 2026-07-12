//! Text-protocol result set encoding: column count, column definitions,
//! rows, and the trailing EOF/OK — the response shape for `COM_QUERY`.
//!
//! Reference: <https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_com_query_response_text_resultset.html>

use crate::protocol::lenenc::{write_lenenc_int, write_lenenc_str};
use crate::protocol::packet::{Packet, MAX_PAYLOAD};
use crate::{Error, Result};

/// A MySQL column type code (protocol `Protocol::ColumnType`). Only the
/// variants this crate currently produces are listed; extend as more SQL
/// types are supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    /// `MYSQL_TYPE_LONGLONG` — a 64-bit integer.
    LongLong,
    /// `MYSQL_TYPE_VAR_STRING` — a variable-length string.
    VarString,
}

impl ColumnType {
    fn code(self) -> u8 {
        match self {
            ColumnType::LongLong => 0x08,
            ColumnType::VarString => 0xfd,
        }
    }
}

/// A single result-set cell, protocol-side and independent of the storage
/// layer's `Value`. The variant fixes both the text and binary encoding and
/// must match its column's [`ColumnType`] (an `Int` cell in a `LongLong`
/// column, a `Text` cell in a `VarString` column) so text and binary rows
/// stay self-consistent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    Int(i64),
    Text(String),
    Null,
}

impl Cell {
    /// Text-protocol form: `None` is SQL `NULL` (the `0xFB` marker).
    fn to_text(&self) -> Option<String> {
        match self {
            Cell::Int(n) => Some(n.to_string()),
            Cell::Text(s) => Some(s.clone()),
            Cell::Null => None,
        }
    }
}

/// A single column's metadata (protocol 41 `Column Definition`).
#[derive(Debug, Clone)]
pub struct ColumnDefinition {
    pub name: String,
    pub column_type: ColumnType,
}

impl ColumnDefinition {
    pub fn new(name: impl Into<String>, column_type: ColumnType) -> Self {
        ColumnDefinition {
            name: name.into(),
            column_type,
        }
    }

    /// Encode this column definition as its own packet — used both inside a
    /// result set and standalone for prepared-statement parameter
    /// definitions (`COM_STMT_PREPARE` response).
    pub fn to_packet(&self, sequence_id: u8) -> Result<Packet> {
        let mut payload = Vec::new();
        payload.extend(write_lenenc_str("def")); // catalog
        payload.extend(write_lenenc_str("")); // schema
        payload.extend(write_lenenc_str("")); // table
        payload.extend(write_lenenc_str("")); // org_table
        payload.extend(write_lenenc_str(&self.name)); // name
        payload.extend(write_lenenc_str(&self.name)); // org_name
        payload.push(0x0c); // length of the fixed-length fields below
        payload.extend_from_slice(&45u16.to_le_bytes()); // utf8mb4_general_ci
        payload.extend_from_slice(&255u32.to_le_bytes()); // column length (display width)
        payload.push(self.column_type.code());
        payload.extend_from_slice(&0u16.to_le_bytes()); // flags
        payload.push(0); // decimals
        payload.extend_from_slice(&[0u8, 0u8]); // filler

        if payload.len() > MAX_PAYLOAD {
            return Err(Error::Protocol(format!(
                "column definition payload of {} bytes exceeds single-packet maximum",
                payload.len()
            )));
        }
        Ok(Packet::new(sequence_id, payload))
    }
}

/// A full result set: columns plus typed rows, ready to encode in either
/// the text protocol (`COM_QUERY`) or the binary protocol (prepared
/// `COM_STMT_EXECUTE`).
#[derive(Debug, Clone, Default)]
pub struct ResultSet {
    pub columns: Vec<ColumnDefinition>,
    pub rows: Vec<Vec<Cell>>,
}

impl ResultSet {
    /// Encode as a text-protocol result set (`COM_QUERY` response).
    pub fn to_text_packets(
        &self,
        deprecate_eof: bool,
        status_flags: u16,
        start: u8,
    ) -> Result<(Vec<Packet>, u8)> {
        self.to_packets(deprecate_eof, status_flags, start, encode_text_row)
    }

    /// Encode as a binary-protocol result set (prepared-statement response).
    pub fn to_binary_packets(
        &self,
        deprecate_eof: bool,
        status_flags: u16,
        start: u8,
    ) -> Result<(Vec<Packet>, u8)> {
        self.to_packets(deprecate_eof, status_flags, start, encode_binary_row)
    }

    /// Shared framing for both protocols; only the per-row encoder differs.
    /// `deprecate_eof` selects the modern (`CLIENT_DEPRECATE_EOF`) or classic
    /// mid-stream/trailing markers; `status_flags` is carried in the
    /// terminator (e.g. `SERVER_MORE_RESULTS_EXISTS` between result sets in a
    /// multi-statement response). Returns the packets and the next sequence
    /// id after them.
    fn to_packets(
        &self,
        deprecate_eof: bool,
        status_flags: u16,
        start_sequence_id: u8,
        encode_row: fn(&[Cell], u8) -> Result<Packet>,
    ) -> Result<(Vec<Packet>, u8)> {
        let mut packets = Vec::with_capacity(2 + self.columns.len() + self.rows.len());
        let mut seq = start_sequence_id;

        packets.push(Packet::new(
            seq,
            write_lenenc_int(self.columns.len() as u64),
        ));
        seq = seq.wrapping_add(1);

        for column in &self.columns {
            packets.push(column.to_packet(seq)?);
            seq = seq.wrapping_add(1);
        }

        if !deprecate_eof {
            packets.push(eof_packet(status_flags, seq));
            seq = seq.wrapping_add(1);
        }

        for row in &self.rows {
            packets.push(encode_row(row, seq)?);
            seq = seq.wrapping_add(1);
        }

        // The result-set terminator. Under `CLIENT_DEPRECATE_EOF` it is an
        // OK packet — but with a `0xFE` header and length < 9, exactly so it
        // stays distinguishable from a data row (a binary row also starts
        // with `0x00`). Without the flag it's a classic EOF packet.
        packets.push(if deprecate_eof {
            deprecate_eof_terminator(status_flags, seq)
        } else {
            eof_packet(status_flags, seq)
        });
        seq = seq.wrapping_add(1);

        Ok((packets, seq))
    }

    /// Encode a text-protocol result set straight into `out` — the
    /// production write path (PERFORMANCE_DURABILITY_PLAN.md P2 step 1):
    /// unlike [`Self::to_text_packets`], this never materializes a
    /// `Vec<Packet>`, so a 1,000-row result becomes one buffer the caller
    /// can hand to the socket in a single `write_all`, not ~1,000 of them.
    /// `to_text_packets` stays as the packet-level structural test surface
    /// (and any other caller that genuinely needs individual `Packet`s);
    /// this is the byte-level equivalent for the hot path. Returns the next
    /// sequence id after the encoded packets.
    pub fn encode_text_into(
        &self,
        out: &mut Vec<u8>,
        deprecate_eof: bool,
        status_flags: u16,
        start: u8,
    ) -> Result<u8> {
        self.encode_into(out, deprecate_eof, status_flags, start, encode_text_row)
    }

    /// Binary-protocol counterpart of [`Self::encode_text_into`].
    pub fn encode_binary_into(
        &self,
        out: &mut Vec<u8>,
        deprecate_eof: bool,
        status_flags: u16,
        start: u8,
    ) -> Result<u8> {
        self.encode_into(out, deprecate_eof, status_flags, start, encode_binary_row)
    }

    /// Shared framing for [`Self::encode_text_into`]/[`Self::encode_binary_into`]
    /// — the byte-buffer twin of [`Self::to_packets`] above (kept as a
    /// separate loop rather than building on `to_packets` so the hot path
    /// never pays for the `Vec<Packet>` it doesn't need).
    fn encode_into(
        &self,
        out: &mut Vec<u8>,
        deprecate_eof: bool,
        status_flags: u16,
        start_sequence_id: u8,
        encode_row: fn(&[Cell], u8) -> Result<Packet>,
    ) -> Result<u8> {
        let mut seq = start_sequence_id;

        Packet::new(seq, write_lenenc_int(self.columns.len() as u64)).encode_into(out);
        seq = seq.wrapping_add(1);

        for column in &self.columns {
            column.to_packet(seq)?.encode_into(out);
            seq = seq.wrapping_add(1);
        }

        if !deprecate_eof {
            eof_packet(status_flags, seq).encode_into(out);
            seq = seq.wrapping_add(1);
        }

        for row in &self.rows {
            encode_row(row, seq)?.encode_into(out);
            seq = seq.wrapping_add(1);
        }

        if deprecate_eof {
            deprecate_eof_terminator(status_flags, seq).encode_into(out)
        } else {
            eof_packet(status_flags, seq).encode_into(out)
        };
        seq = seq.wrapping_add(1);

        Ok(seq)
    }
}

/// The `CLIENT_DEPRECATE_EOF` result-set terminator: an OK packet carrying
/// `0xFE` as its header (not `0x00`) and staying under 9 bytes, so clients
/// read it as the end-of-rows marker rather than another row.
fn deprecate_eof_terminator(status_flags: u16, sequence_id: u8) -> Packet {
    let mut payload = vec![0xfe];
    payload.extend(write_lenenc_int(0)); // affected_rows
    payload.extend(write_lenenc_int(0)); // last_insert_id
    payload.extend_from_slice(&status_flags.to_le_bytes());
    payload.extend_from_slice(&0u16.to_le_bytes()); // warnings
    Packet::new(sequence_id, payload)
}

/// The text-protocol row marker for SQL `NULL`, in place of a lenenc-string.
const NULL_MARKER: u8 = 0xfb;

fn encode_text_row(cells: &[Cell], sequence_id: u8) -> Result<Packet> {
    let mut payload = Vec::new();
    for cell in cells {
        match cell.to_text() {
            Some(v) => payload.extend(write_lenenc_str(&v)),
            None => payload.push(NULL_MARKER),
        }
    }
    guard_payload_size(&payload, "row")?;
    Ok(Packet::new(sequence_id, payload))
}

/// Encode a binary-protocol result row.
///
/// Layout: `0x00` header, then a NULL bitmap of `ceil((n + 2) / 8)` bytes
/// (the 2-bit offset is a fixed part of the binary row format — bits 0 and 1
/// are reserved, so column `i` uses bit `i + 2`), then each non-NULL value in
/// its type-specific binary form.
///
/// Reference: <https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_binary_resultset.html>
fn encode_binary_row(cells: &[Cell], sequence_id: u8) -> Result<Packet> {
    let mut payload = vec![0x00];

    // NULL bitmap length: `ceil((n + 2) / 8)` — n columns plus the 2-bit offset.
    let bitmap_len = (cells.len() + 2).div_ceil(8);
    let mut null_bitmap = vec![0u8; bitmap_len];
    for (i, cell) in cells.iter().enumerate() {
        if matches!(cell, Cell::Null) {
            let bit = i + 2;
            null_bitmap[bit / 8] |= 1 << (bit % 8);
        }
    }
    payload.extend_from_slice(&null_bitmap);

    for cell in cells {
        match cell {
            Cell::Null => {}
            Cell::Int(n) => payload.extend_from_slice(&n.to_le_bytes()),
            Cell::Text(s) => payload.extend(write_lenenc_str(s)),
        }
    }

    guard_payload_size(&payload, "binary row")?;
    Ok(Packet::new(sequence_id, payload))
}

fn guard_payload_size(payload: &[u8], what: &str) -> Result<()> {
    if payload.len() > MAX_PAYLOAD {
        return Err(Error::Protocol(format!(
            "{what} payload of {} bytes exceeds single-packet maximum",
            payload.len()
        )));
    }
    Ok(())
}

fn eof_packet(status_flags: u16, sequence_id: u8) -> Packet {
    let mut payload = vec![0xfe];
    payload.extend_from_slice(&0u16.to_le_bytes()); // warnings
    payload.extend_from_slice(&status_flags.to_le_bytes());
    Packet::new(sequence_id, payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ResultSet {
        ResultSet {
            columns: vec![ColumnDefinition::new("1", ColumnType::LongLong)],
            rows: vec![vec![Cell::Int(1)]],
        }
    }

    /// `SERVER_STATUS_AUTOCOMMIT`.
    const AUTOCOMMIT: u16 = 0x0002;

    #[test]
    fn classic_framing_includes_two_eofs() {
        let (packets, next_seq) = sample()
            .to_text_packets(false, AUTOCOMMIT, 5)
            .expect("encode");
        // count, coldef, EOF, row, EOF = 5 packets.
        assert_eq!(packets.len(), 5);
        assert_eq!(next_seq, 10);

        assert_eq!(packets[0].payload, vec![1]); // column count = 1
        assert_eq!(packets[2].payload[0], 0xfe); // EOF after column defs
        assert_eq!(packets[4].payload[0], 0xfe); // trailing EOF
        for (i, p) in packets.iter().enumerate() {
            assert_eq!(p.sequence_id, 5 + i as u8);
        }
    }

    #[test]
    fn deprecate_eof_framing_uses_an_ok_style_terminator() {
        let (packets, next_seq) = sample()
            .to_text_packets(true, AUTOCOMMIT, 0)
            .expect("encode");
        // count, coldef, row, terminator = 4 packets (no mid-stream EOF).
        assert_eq!(packets.len(), 4);
        assert_eq!(next_seq, 4);
        assert_eq!(packets[2].payload, vec![1, b'1']); // row: lenenc "1" (text)
                                                       // Terminator uses the 0xFE header (< 9 bytes) so it can't be mistaken
                                                       // for a data row.
        let terminator = &packets.last().unwrap().payload;
        assert_eq!(terminator[0], 0xfe);
        assert!(terminator.len() < 9);
    }

    /// PERFORMANCE_DURABILITY_PLAN.md P2 step 1: `encode_text_into`/
    /// `encode_binary_into` must produce exactly the bytes a client would
    /// see today, byte for byte — these are a new *encoding path* for the
    /// same wire format, not a new format. Cross-checked against
    /// `to_*_packets` (each packet's own `encode()`, concatenated) across
    /// both classic and `CLIENT_DEPRECATE_EOF` framing, and both protocols,
    /// so a divergence in either encoder would fail here.
    fn assert_encode_into_matches_to_packets(
        result_set: &ResultSet,
        deprecate_eof: bool,
        status_flags: u16,
        start: u8,
    ) {
        let (text_packets, text_next_seq) = result_set
            .to_text_packets(deprecate_eof, status_flags, start)
            .expect("to_text_packets");
        let mut expected_text = Vec::new();
        for p in &text_packets {
            expected_text.extend(p.encode());
        }
        let mut actual_text = Vec::new();
        let actual_text_next_seq = result_set
            .encode_text_into(&mut actual_text, deprecate_eof, status_flags, start)
            .expect("encode_text_into");
        assert_eq!(actual_text, expected_text);
        assert_eq!(actual_text_next_seq, text_next_seq);

        let (binary_packets, binary_next_seq) = result_set
            .to_binary_packets(deprecate_eof, status_flags, start)
            .expect("to_binary_packets");
        let mut expected_binary = Vec::new();
        for p in &binary_packets {
            expected_binary.extend(p.encode());
        }
        let mut actual_binary = Vec::new();
        let actual_binary_next_seq = result_set
            .encode_binary_into(&mut actual_binary, deprecate_eof, status_flags, start)
            .expect("encode_binary_into");
        assert_eq!(actual_binary, expected_binary);
        assert_eq!(actual_binary_next_seq, binary_next_seq);
    }

    #[test]
    fn encode_into_matches_to_packets_classic_framing() {
        assert_encode_into_matches_to_packets(&sample(), false, AUTOCOMMIT, 5);
    }

    #[test]
    fn encode_into_matches_to_packets_deprecate_eof_framing() {
        assert_encode_into_matches_to_packets(&sample(), true, AUTOCOMMIT, 0);
    }

    #[test]
    fn encode_into_matches_to_packets_multi_row_multi_column_with_nulls() {
        let result_set = ResultSet {
            columns: vec![
                ColumnDefinition::new("id", ColumnType::LongLong),
                ColumnDefinition::new("name", ColumnType::VarString),
            ],
            rows: vec![
                vec![Cell::Int(1), Cell::Text("ada".to_string())],
                vec![Cell::Int(2), Cell::Null],
                vec![Cell::Null, Cell::Text("carol".to_string())],
            ],
        };
        assert_encode_into_matches_to_packets(&result_set, false, AUTOCOMMIT, 3);
        assert_encode_into_matches_to_packets(&result_set, true, AUTOCOMMIT, 3);
    }

    #[test]
    fn encode_into_appends_rather_than_overwriting_a_nonempty_buffer() {
        let mut out = vec![0xAA, 0xBB];
        let next_seq = sample()
            .encode_text_into(&mut out, true, AUTOCOMMIT, 0)
            .expect("encode");
        assert_eq!(
            &out[..2],
            &[0xAA, 0xBB],
            "must append, not clear, the buffer"
        );
        assert!(out.len() > 2);
        assert_eq!(next_seq, 4);
    }

    #[test]
    fn column_definition_layout() {
        let column = ColumnDefinition::new("@@version", ColumnType::VarString);
        let packet = column.to_packet(1).expect("encode");
        // 4 empty/short lenenc strings (def=3+1, ""=1, ""=1, ""=1) then name
        // twice (9 chars each, lenenc-prefixed), then the fixed 13-byte tail.
        let expected_len = (1 + 3) + 1 + 1 + 1 + (1 + 9) + (1 + 9) + 1 + 2 + 4 + 1 + 2 + 1 + 2;
        assert_eq!(packet.payload.len(), expected_len);
        assert_eq!(packet.payload[0], 3); // lenenc length of "def"
        assert_eq!(&packet.payload[1..4], b"def");
    }

    #[test]
    fn text_row_encodes_values_as_lenenc_strings() {
        let packet =
            encode_text_row(&[Cell::Int(1), Cell::Text("hi".to_string())], 2).expect("encode");
        assert_eq!(packet.sequence_id, 2);
        assert_eq!(packet.payload, vec![1, b'1', 2, b'h', b'i']);
    }

    #[test]
    fn text_row_encodes_null_as_the_dedicated_marker() {
        let packet =
            encode_text_row(&[Cell::Null, Cell::Text("x".to_string())], 3).expect("encode");
        assert_eq!(packet.payload, vec![0xfb, 1, b'x']);
    }

    #[test]
    fn binary_row_encodes_int_as_eight_le_bytes() {
        let packet = encode_binary_row(&[Cell::Int(1)], 2).expect("encode");
        // header 0x00, 1-byte null bitmap (all zero), then 8-byte LE int.
        assert_eq!(packet.payload, vec![0x00, 0x00, 1, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn binary_row_encodes_text_as_lenenc_string() {
        let packet = encode_binary_row(&[Cell::Text("hi".to_string())], 0).expect("encode");
        assert_eq!(packet.payload, vec![0x00, 0x00, 2, b'h', b'i']);
    }

    #[test]
    fn binary_row_marks_null_in_the_offset_bitmap() {
        // Two columns: col 0 NULL, col 1 = 7. Binary NULL bitmap has a 2-bit
        // offset, so col 0 is bit 2 -> byte value 0b0000_0100 = 0x04.
        let packet = encode_binary_row(&[Cell::Null, Cell::Int(7)], 0).expect("encode");
        assert_eq!(packet.payload[0], 0x00); // header
        assert_eq!(packet.payload[1], 0x04); // null bitmap: only col 0 (bit 2) set
        assert_eq!(&packet.payload[2..], &[7, 0, 0, 0, 0, 0, 0, 0]); // col 1 only
    }
}
