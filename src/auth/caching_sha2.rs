//! The `caching_sha2_password` authentication plugin — MySQL 8.0's default.
//!
//! Fast-auth challenge/response (the only path this server uses):
//! the client sends
//! `reply = SHA256(password) XOR SHA256( SHA256(SHA256(password)) ++ nonce )`.
//! The server stores only `SHA256(SHA256(password))` and verifies without ever
//! seeing the password: recover the candidate `SHA256(password)` by XORing the
//! reply with `SHA256(stored ++ nonce)`, then check that its SHA-256 equals
//! `stored`. Note the concat order (`stored` then `nonce`) is the reverse of
//! `mysql_native_password`'s `SHA1(nonce ++ SHA1(SHA1(password)))`.
//!
//! MySQL also defines a "full authentication" path (the plaintext password
//! over a secure channel, or RSA-encrypted over plaintext) used when the
//! server's in-memory cache is cold. This server always holds the verifier —
//! it is derived from the configured password at startup — so it can always
//! take the fast path and signal `fast_auth_success`. Real clients accept that
//! on their very first connection exactly as they would after a cache warm-up;
//! the fast/full decision is the server's to make.
//!
//! Reference: <https://dev.mysql.com/doc/dev/mysql-server/latest/page_caching_sha2_authentication_exchange.html>

use super::sha256;

/// Length of a `caching_sha2_password` fast-auth reply: one SHA-256 digest.
pub const FAST_AUTH_RESPONSE_LEN: usize = 32;

/// Compute the stored credential for a plaintext password: `SHA256(SHA256(password))`.
pub fn hash_password(password: &[u8]) -> [u8; 32] {
    sha256::digest(&sha256::digest(password))
}

/// Compute the fast-auth reply a client sends, given the password and the
/// server's nonce. An empty (or absent) password yields an empty reply,
/// matching what a real client sends for a passwordless account. Used to
/// simulate a real client end to end in tests.
pub fn scramble(password: Option<&[u8]>, nonce: &[u8]) -> Vec<u8> {
    let password = match password {
        Some(p) if !p.is_empty() => p,
        _ => return Vec::new(),
    };

    let digest1 = sha256::digest(password);
    let digest2 = sha256::digest(&digest1);

    let mut combined = Vec::with_capacity(digest2.len() + nonce.len());
    combined.extend_from_slice(&digest2);
    combined.extend_from_slice(nonce);
    let mask = sha256::digest(&combined);

    let mut reply = [0u8; FAST_AUTH_RESPONSE_LEN];
    for (out, (&d, &m)) in reply.iter_mut().zip(digest1.iter().zip(mask.iter())) {
        *out = d ^ m;
    }
    reply.to_vec()
}

/// Verify a client's fast-auth reply against the stored `SHA256(SHA256(password))`
/// and the nonce sent to them. `stored: None` means the account has no
/// password, in which case the client must send an empty reply.
pub fn verify(reply: &[u8], nonce: &[u8], stored: Option<&[u8; 32]>) -> bool {
    let Some(stored) = stored else {
        return reply.is_empty();
    };
    if reply.len() != FAST_AUTH_RESPONSE_LEN {
        return false;
    }

    let mut combined = Vec::with_capacity(stored.len() + nonce.len());
    combined.extend_from_slice(stored);
    combined.extend_from_slice(nonce);
    let mask = sha256::digest(&combined);

    let mut candidate_digest1 = [0u8; FAST_AUTH_RESPONSE_LEN];
    for (out, (&r, &m)) in candidate_digest1
        .iter_mut()
        .zip(reply.iter().zip(mask.iter()))
    {
        *out = r ^ m;
    }
    let candidate_digest2 = sha256::digest(&candidate_digest1);

    // Constant-time compare so verification doesn't leak how close a guess was.
    constant_time_eq(&candidate_digest2, stored)
}

/// Compare two byte slices without short-circuiting on the first mismatch.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE_A: &[u8; 20] = b"01234567890123456789";
    const NONCE_B: &[u8; 20] = b"scrambleAAAAAAAAAAAA";

    #[test]
    fn round_trip_compute_and_verify() {
        let stored = hash_password(b"hunter2");
        let reply = scramble(Some(b"hunter2"), NONCE_A);
        assert_eq!(reply.len(), FAST_AUTH_RESPONSE_LEN);
        assert!(verify(&reply, NONCE_A, Some(&stored)));
    }

    #[test]
    fn wrong_password_is_rejected() {
        let stored = hash_password(b"correct-horse");
        let reply = scramble(Some(b"wrong-password"), NONCE_A);
        assert!(!verify(&reply, NONCE_A, Some(&stored)));
    }

    #[test]
    fn mismatched_nonce_is_rejected() {
        let stored = hash_password(b"hunter2");
        // Reply computed against a different nonce than the server sent
        // (e.g. a stale/replayed response) must not verify.
        let reply = scramble(Some(b"hunter2"), NONCE_B);
        assert!(!verify(&reply, NONCE_A, Some(&stored)));
    }

    #[test]
    fn empty_password_requires_empty_reply() {
        assert!(verify(b"", NONCE_A, None));
        assert!(!verify(b"not-empty", NONCE_A, None));
    }

    #[test]
    fn compute_reply_is_empty_for_no_password() {
        assert!(scramble(None, NONCE_A).is_empty());
        assert!(scramble(Some(b""), NONCE_A).is_empty());
    }

    #[test]
    fn different_nonces_produce_different_replies() {
        let r1 = scramble(Some(b"hunter2"), NONCE_A);
        let r2 = scramble(Some(b"hunter2"), NONCE_B);
        assert_ne!(r1, r2);
    }

    #[test]
    fn wrong_length_reply_is_rejected_not_panicking() {
        let stored = hash_password(b"hunter2");
        assert!(!verify(&[1, 2, 3], NONCE_A, Some(&stored)));
        assert!(!verify(&[0u8; 64], NONCE_A, Some(&stored)));
    }

    /// The stored verifier is the double SHA-256 of the password, and differs
    /// from the single hash — a sanity check on which digest we persist.
    #[test]
    fn stored_is_double_sha256() {
        let once = sha256::digest(b"hunter2");
        let twice = sha256::digest(&once);
        assert_eq!(hash_password(b"hunter2"), twice);
        assert_ne!(hash_password(b"hunter2"), once);
    }
}
