//! Phase 9 hardening: an in-process fuzz harness for the packet and parser
//! layers. Real `cargo-fuzz` needs nightly + libFuzzer; this is a
//! deterministic, dependency-free substitute that runs under `cargo test` —
//! it feeds tens of thousands of pseudo-random inputs to every
//! client-reachable parsing entry point and asserts none of them panic
//! (a panic surfaces as a failed test). The parsers may return `Ok` or
//! `Err`; the only unacceptable outcome is a crash.

use mysql_rust::protocol::handshake::HandshakeResponse41;
use mysql_rust::protocol::lenenc::read_lenenc_int;
use mysql_rust::protocol::{parse_execute_params, Packet};
use mysql_rust::query::parser;

/// A tiny deterministic xorshift64 PRNG — seeded fixed so failures reproduce,
/// and dependency-free (the crate ships no `rand`).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn byte(&mut self) -> u8 {
        (self.next_u64() & 0xff) as u8
    }

    /// A random length in `0..=max`, biased toward small values (where edge
    /// cases live).
    fn len(&mut self, max: usize) -> usize {
        (self.next_u64() as usize) % (max + 1)
    }

    fn bytes(&mut self, max: usize) -> Vec<u8> {
        let n = self.len(max);
        (0..n).map(|_| self.byte()).collect()
    }
}

const ITERATIONS: usize = 20_000;

#[test]
fn packet_parsing_never_panics_on_random_bytes() {
    let mut rng = Rng::new(0x1234_5678);
    for _ in 0..ITERATIONS {
        let buf = rng.bytes(64);
        // `parse` (incremental) and `decode` (whole-packet) must both be
        // total functions over arbitrary bytes.
        let _ = Packet::parse(&buf);
        let _ = Packet::decode(&buf);
    }
}

#[test]
fn lenenc_int_reading_never_panics() {
    let mut rng = Rng::new(0x9e37_79b9);
    for _ in 0..ITERATIONS {
        let buf = rng.bytes(12);
        let _ = read_lenenc_int(&buf);
    }
}

#[test]
fn handshake_response_parsing_never_panics() {
    let mut rng = Rng::new(0xdead_beef);
    for _ in 0..ITERATIONS {
        // Mix fully-random payloads with ones that start with a plausible
        // 32-byte header, so parsing gets past the length check and exercises
        // the NUL-terminated-string and length-encoded auth-response paths.
        let mut buf = rng.bytes(80);
        if rng.byte() & 1 == 0 && buf.len() >= 4 {
            // Force CLIENT_PROTOCOL_41 so parsing proceeds past the guard.
            buf[0] = 0x00;
            buf[1] = 0x02;
        }
        let _ = HandshakeResponse41::parse(&buf);
    }
}

#[test]
fn com_stmt_execute_param_parsing_never_panics() {
    let mut rng = Rng::new(0x0badc0de);
    for _ in 0..ITERATIONS {
        let buf = rng.bytes(48);
        // Try a range of parameter counts — the count is server-controlled in
        // reality, but the payload is attacker-controlled, so vary both.
        for num_params in [0usize, 1, 2, 5] {
            let _ = parse_execute_params(&buf, num_params);
        }
    }
}

/// Characters that keep the fuzzed SQL as valid UTF-8 while covering the
/// tokenizer's interesting bytes (quotes, operators, `?`, `@`, `;`, parens).
const SQL_ALPHABET: &[u8] = b"abcSELECT INTO FROM WHERE VALUES 0123456789'(),;=<>!*?@_. \t\n";

#[test]
fn sql_parsing_never_panics_on_random_strings() {
    let mut rng = Rng::new(0xfeed_face);
    for _ in 0..ITERATIONS {
        let n = rng.len(48);
        let sql: String = (0..n)
            .map(|_| {
                let idx = (rng.next_u64() as usize) % SQL_ALPHABET.len();
                SQL_ALPHABET[idx] as char
            })
            .collect();
        // All three entry points must be total over arbitrary input.
        let _ = parser::parse(&sql);
        let _ = parser::parse_prepared(&sql);
        let _ = parser::parse_many(&sql);
    }
}

#[test]
fn sql_parsing_never_panics_on_arbitrary_utf8() {
    // Also feed lossy-decoded random bytes, so non-ASCII and control chars
    // reach the tokenizer.
    let mut rng = Rng::new(0xc0ffee11);
    for _ in 0..ITERATIONS {
        let bytes = rng.bytes(48);
        let sql = String::from_utf8_lossy(&bytes);
        let _ = parser::parse(&sql);
        let _ = parser::parse_prepared(&sql);
        let _ = parser::parse_many(&sql);
    }
}
