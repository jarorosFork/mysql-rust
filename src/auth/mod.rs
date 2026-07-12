//! Client authentication.
//!
//! MySQL supports several authentication plugins. This server implements the
//! two that matter for standard clients: `mysql_native_password` (SHA-1,
//! legacy) and `caching_sha2_password` (SHA-256, the MySQL 8.0 default). Each
//! account is configured for one of them; the server advertises a default in
//! the handshake and issues an auth-switch when a client's account uses the
//! other (see `server::connection`).

pub mod caching_sha2;
pub mod native_password;
mod sha1;
mod sha256;

use std::collections::HashMap;

use crate::config::UserCredential;

/// The authentication plugin an account uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthPlugin {
    /// `mysql_native_password` — SHA-1 challenge/response (legacy).
    MysqlNativePassword,
    /// `caching_sha2_password` — SHA-256 challenge/response (MySQL 8.0 default).
    CachingSha2Password,
}

impl AuthPlugin {
    /// The plugin's name as it appears on the wire (handshake / auth-switch).
    pub fn wire_name(self) -> &'static str {
        match self {
            AuthPlugin::MysqlNativePassword => "mysql_native_password",
            AuthPlugin::CachingSha2Password => "caching_sha2_password",
        }
    }

    /// Parse a wire plugin name, returning `None` for anything this server
    /// doesn't implement.
    pub fn from_wire_name(name: &str) -> Option<Self> {
        match name {
            "mysql_native_password" => Some(AuthPlugin::MysqlNativePassword),
            "caching_sha2_password" => Some(AuthPlugin::CachingSha2Password),
            _ => None,
        }
    }
}

/// The result of attempting to authenticate a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthOutcome {
    /// Credentials were accepted.
    Ok,
    /// Credentials were rejected.
    Denied,
}

/// One account as held by the authenticator: its plugin and the plugin-specific
/// stored verifier (`None` = no password).
#[derive(Debug, Clone)]
struct StoredUser {
    plugin: AuthPlugin,
    verifier: Option<Vec<u8>>,
}

/// Authenticates connecting clients against an in-memory credential table.
#[derive(Debug)]
pub struct Authenticator {
    users: HashMap<String, StoredUser>,
}

impl Authenticator {
    /// Build an authenticator from a configured user list (see `Config::users`).
    pub fn new(users: &[UserCredential]) -> Self {
        let users = users
            .iter()
            .map(|u| {
                (
                    u.username.clone(),
                    StoredUser {
                        plugin: u.plugin,
                        verifier: u.verifier.clone(),
                    },
                )
            })
            .collect();
        Authenticator { users }
    }

    /// The auth plugin configured for `username`, if the account exists.
    pub fn plugin_for(&self, username: &str) -> Option<AuthPlugin> {
        self.users.get(username).map(|u| u.plugin)
    }

    /// Verify a client's auth-response for `username`, computed against
    /// `scramble` using `plugin`. The response is rejected if the account
    /// doesn't exist, uses a different plugin than the one presented, or the
    /// verification itself fails. Unknown usernames are denied the same as a
    /// bad password, to avoid revealing which accounts exist.
    pub fn authenticate(
        &self,
        username: &str,
        plugin: AuthPlugin,
        auth_response: &[u8],
        scramble: &[u8],
    ) -> AuthOutcome {
        let Some(user) = self.users.get(username) else {
            return AuthOutcome::Denied;
        };
        // The plugin the response was computed with must be the account's own.
        if user.plugin != plugin {
            return AuthOutcome::Denied;
        }

        let ok = match plugin {
            AuthPlugin::MysqlNativePassword => match fixed_verifier::<20>(&user.verifier) {
                VerifierState::None => native_password::verify(auth_response, scramble, None),
                VerifierState::Value(hash) => {
                    native_password::verify(auth_response, scramble, Some(&hash))
                }
                VerifierState::BadLength => false,
            },
            AuthPlugin::CachingSha2Password => match fixed_verifier::<32>(&user.verifier) {
                VerifierState::None => caching_sha2::verify(auth_response, scramble, None),
                VerifierState::Value(stored) => {
                    caching_sha2::verify(auth_response, scramble, Some(&stored))
                }
                VerifierState::BadLength => false,
            },
        };

        if ok {
            AuthOutcome::Ok
        } else {
            AuthOutcome::Denied
        }
    }
}

/// A stored verifier interpreted as a fixed-size digest.
enum VerifierState<const N: usize> {
    /// No password configured.
    None,
    /// A well-formed `N`-byte verifier.
    Value([u8; N]),
    /// A verifier of the wrong length for this plugin (treated as deny).
    BadLength,
}

fn fixed_verifier<const N: usize>(verifier: &Option<Vec<u8>>) -> VerifierState<N> {
    match verifier {
        None => VerifierState::None,
        Some(bytes) => match <[u8; N]>::try_from(bytes.as_slice()) {
            Ok(arr) => VerifierState::Value(arr),
            Err(_) => VerifierState::BadLength,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scramble() -> [u8; 20] {
        *b"01234567890123456789"
    }

    #[test]
    fn accepts_correct_native_password() {
        let users = vec![UserCredential::with_password("alice", "s3cret")];
        let auth = Authenticator::new(&users);
        let response = native_password::compute_auth_response(Some(b"s3cret"), &scramble());
        assert_eq!(
            auth.authenticate(
                "alice",
                AuthPlugin::MysqlNativePassword,
                &response,
                &scramble()
            ),
            AuthOutcome::Ok
        );
    }

    #[test]
    fn rejects_wrong_native_password() {
        let users = vec![UserCredential::with_password("alice", "s3cret")];
        let auth = Authenticator::new(&users);
        let response = native_password::compute_auth_response(Some(b"wrong"), &scramble());
        assert_eq!(
            auth.authenticate(
                "alice",
                AuthPlugin::MysqlNativePassword,
                &response,
                &scramble()
            ),
            AuthOutcome::Denied
        );
    }

    #[test]
    fn rejects_unknown_user() {
        let users = vec![UserCredential::with_password("alice", "s3cret")];
        let auth = Authenticator::new(&users);
        let response = native_password::compute_auth_response(Some(b"s3cret"), &scramble());
        assert_eq!(
            auth.authenticate(
                "bob",
                AuthPlugin::MysqlNativePassword,
                &response,
                &scramble()
            ),
            AuthOutcome::Denied
        );
    }

    #[test]
    fn accepts_passwordless_user_with_empty_response() {
        let users = vec![UserCredential::with_password("guest", "")];
        let auth = Authenticator::new(&users);
        assert_eq!(
            auth.authenticate("guest", AuthPlugin::MysqlNativePassword, b"", &scramble()),
            AuthOutcome::Ok
        );
    }

    #[test]
    fn accepts_correct_caching_sha2_password() {
        let users = vec![UserCredential::with_caching_sha2_password(
            "alice", "s3cret",
        )];
        let auth = Authenticator::new(&users);
        assert_eq!(
            auth.plugin_for("alice"),
            Some(AuthPlugin::CachingSha2Password)
        );
        let reply = caching_sha2::scramble(Some(b"s3cret"), &scramble());
        assert_eq!(
            auth.authenticate(
                "alice",
                AuthPlugin::CachingSha2Password,
                &reply,
                &scramble()
            ),
            AuthOutcome::Ok
        );
    }

    #[test]
    fn rejects_wrong_caching_sha2_password() {
        let users = vec![UserCredential::with_caching_sha2_password(
            "alice", "s3cret",
        )];
        let auth = Authenticator::new(&users);
        let reply = caching_sha2::scramble(Some(b"nope"), &scramble());
        assert_eq!(
            auth.authenticate(
                "alice",
                AuthPlugin::CachingSha2Password,
                &reply,
                &scramble()
            ),
            AuthOutcome::Denied
        );
    }

    /// Presenting the wrong plugin for an account is denied even with an
    /// otherwise-valid response — the server must switch the client to the
    /// account's plugin first.
    #[test]
    fn rejects_plugin_mismatch() {
        let users = vec![UserCredential::with_caching_sha2_password(
            "alice", "s3cret",
        )];
        let auth = Authenticator::new(&users);
        let native = native_password::compute_auth_response(Some(b"s3cret"), &scramble());
        assert_eq!(
            auth.authenticate(
                "alice",
                AuthPlugin::MysqlNativePassword,
                &native,
                &scramble()
            ),
            AuthOutcome::Denied
        );
    }

    #[test]
    fn plugin_for_unknown_user_is_none() {
        let auth = Authenticator::new(&[]);
        assert_eq!(auth.plugin_for("ghost"), None);
    }
}
