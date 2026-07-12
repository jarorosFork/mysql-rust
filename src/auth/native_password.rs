//! The `mysql_native_password` authentication plugin.
//!
//! Challenge/response scheme:
//! `token = SHA1(password) XOR SHA1(scramble ++ SHA1(SHA1(password)))`.
//! The server stores and ever only needs `SHA1(SHA1(password))` ("stage 2");
//! neither the plaintext nor the single-hashed password is ever persisted or
//! transmitted.
//!
//! Reference: <https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_connection_phase_authentication_methods_native_password_authentication.html>

use super::sha1;

/// Compute the stored credential for a plaintext password: `SHA1(SHA1(password))`.
pub fn hash_password(password: &[u8]) -> [u8; 20] {
    sha1::digest(&sha1::digest(password))
}

/// Compute the auth-response a client would send, given the password and the
/// server's scramble. An empty (or absent) password yields an empty
/// response, matching what a real client sends for a passwordless user.
/// Used to simulate a real client end to end in tests.
pub fn compute_auth_response(password: Option<&[u8]>, scramble: &[u8]) -> Vec<u8> {
    let password = match password {
        Some(p) if !p.is_empty() => p,
        _ => return Vec::new(),
    };

    let stage1 = sha1::digest(password);
    let stage2 = sha1::digest(&stage1);

    let mut combined = Vec::with_capacity(scramble.len() + stage2.len());
    combined.extend_from_slice(scramble);
    combined.extend_from_slice(&stage2);
    let hash_of_combined = sha1::digest(&combined);

    let mut token = [0u8; 20];
    for i in 0..20 {
        token[i] = stage1[i] ^ hash_of_combined[i];
    }
    token.to_vec()
}

/// Verify a client's auth-response against the stored password hash and the
/// scramble that was sent to them. `stored_hash: None` means the account has
/// no password, in which case the client must send an empty auth-response.
pub fn verify(auth_response: &[u8], scramble: &[u8], stored_hash: Option<&[u8; 20]>) -> bool {
    let Some(hash) = stored_hash else {
        return auth_response.is_empty();
    };
    if auth_response.len() != 20 {
        return false;
    }

    let mut combined = Vec::with_capacity(scramble.len() + hash.len());
    combined.extend_from_slice(scramble);
    combined.extend_from_slice(hash);
    let hash_of_combined = sha1::digest(&combined);

    let mut candidate_stage1 = [0u8; 20];
    for i in 0..20 {
        candidate_stage1[i] = auth_response[i] ^ hash_of_combined[i];
    }
    let candidate_stage2 = sha1::digest(&candidate_stage1);
    &candidate_stage2 == hash
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRAMBLE_A: &[u8; 20] = b"01234567890123456789";
    const SCRAMBLE_B: &[u8; 20] = b"scrambleAAAAAAAAAAAA";

    #[test]
    fn round_trip_compute_and_verify() {
        let hash = hash_password(b"hunter2");
        let response = compute_auth_response(Some(b"hunter2"), SCRAMBLE_A);
        assert!(verify(&response, SCRAMBLE_A, Some(&hash)));
    }

    #[test]
    fn wrong_password_is_rejected() {
        let hash = hash_password(b"correct-horse");
        let response = compute_auth_response(Some(b"wrong-password"), SCRAMBLE_A);
        assert!(!verify(&response, SCRAMBLE_A, Some(&hash)));
    }

    #[test]
    fn mismatched_scramble_is_rejected() {
        let hash = hash_password(b"hunter2");
        // Response computed against a different scramble than the one the
        // server actually sent (e.g. a stale/replayed response).
        let response = compute_auth_response(Some(b"hunter2"), SCRAMBLE_B);
        assert!(!verify(&response, SCRAMBLE_A, Some(&hash)));
    }

    #[test]
    fn empty_password_requires_empty_response() {
        assert!(verify(b"", SCRAMBLE_A, None));
        assert!(!verify(b"not-empty-but-wrong-len", SCRAMBLE_A, None));
    }

    #[test]
    fn compute_auth_response_is_empty_for_no_password() {
        assert!(compute_auth_response(None, SCRAMBLE_A).is_empty());
        assert!(compute_auth_response(Some(b""), SCRAMBLE_A).is_empty());
    }

    #[test]
    fn different_scrambles_produce_different_responses() {
        let r1 = compute_auth_response(Some(b"hunter2"), SCRAMBLE_A);
        let r2 = compute_auth_response(Some(b"hunter2"), SCRAMBLE_B);
        assert_ne!(r1, r2);
    }

    #[test]
    fn wrong_length_response_is_rejected_not_panicking() {
        let hash = hash_password(b"hunter2");
        assert!(!verify(&[1, 2, 3], SCRAMBLE_A, Some(&hash)));
    }
}
