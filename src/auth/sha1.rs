//! A from-scratch SHA-1 implementation (FIPS 180-4).
//!
//! `mysql_native_password` is built entirely from SHA-1, and this crate is
//! deliberately dependency-free (see CLAUDE.md); SHA-1 is small and fully
//! testable against known vectors, so it's implemented here rather than
//! pulling in a crypto crate. SHA-1 is cryptographically broken for
//! collision resistance, but that's irrelevant to this use (a
//! challenge-response scramble, not certificate signing) — and
//! `mysql_native_password` itself is legacy, superseded by
//! `caching_sha2_password` (Phase 9).

const H0: u32 = 0x67452301;
const H1: u32 = 0xEFCDAB89;
const H2: u32 = 0x98BADCFE;
const H3: u32 = 0x10325476;
const H4: u32 = 0xC3D2E1F0;

/// Compute the SHA-1 digest of `input`.
pub fn digest(input: &[u8]) -> [u8; 20] {
    let mut h = [H0, H1, H2, H3, H4];

    let padded = pad_message(input);
    for chunk in padded.chunks_exact(64) {
        process_chunk(chunk, &mut h);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn pad_message(input: &[u8]) -> Vec<u8> {
    let bit_len = (input.len() as u64) * 8;
    let mut padded = input.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    padded
}

fn process_chunk(chunk: &[u8], h: &mut [u32; 5]) {
    let mut w = [0u32; 80];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([
            chunk[i * 4],
            chunk[i * 4 + 1],
            chunk[i * 4 + 2],
            chunk[i * 4 + 3],
        ]);
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }

    let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);

    for (i, &word) in w.iter().enumerate() {
        let (f, k) = match i {
            0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
            20..=39 => (b ^ c ^ d, 0x6ED9EBA1u32),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDCu32),
            _ => (b ^ c ^ d, 0xCA62C1D6u32),
        };

        let temp = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(word);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = temp;
    }

    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn empty_input() {
        assert_eq!(
            hex(&digest(b"")),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
    }

    #[test]
    fn short_input() {
        assert_eq!(
            hex(&digest(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    /// The standard NIST two-block test vector (56 bytes forces padding to
    /// spill into a second 64-byte block), exercising the multi-block path.
    #[test]
    fn two_block_input() {
        assert_eq!(
            hex(&digest(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }
}
