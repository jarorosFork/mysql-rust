//! Server configuration.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

use crate::auth::{caching_sha2, native_password, AuthPlugin};
use crate::observability::LogLevel;
use crate::{Error, Result};

/// TLS configuration: an opaque holder for a built rustls acceptor. When set
/// on [`Config::tls`], the server advertises `CLIENT_SSL` and upgrades
/// connections that request it. Clone is cheap (an `Arc` inside the
/// acceptor); `Debug` deliberately reveals nothing about the key material.
#[derive(Clone)]
pub struct TlsConfig {
    acceptor: TlsAcceptor,
}

impl std::fmt::Debug for TlsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TlsConfig(<rustls server config>)")
    }
}

impl TlsConfig {
    /// Build from a DER-encoded certificate chain and private key.
    pub fn from_der(
        cert_chain: Vec<CertificateDer<'static>>,
        private_key: PrivateKeyDer<'static>,
    ) -> Result<Self> {
        // Use the ring provider explicitly rather than relying on a
        // process-global default provider being installed.
        let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::Protocol(format!("TLS: bad protocol versions: {e}")))?
            .with_no_client_auth()
            .with_single_cert(cert_chain, private_key)
            .map_err(|e| Error::Protocol(format!("TLS: invalid certificate/key: {e}")))?;
        Ok(TlsConfig {
            acceptor: TlsAcceptor::from(Arc::new(config)),
        })
    }

    /// The rustls acceptor used to upgrade a connection to TLS.
    pub(crate) fn acceptor(&self) -> &TlsAcceptor {
        &self.acceptor
    }
}

/// Runtime configuration for the server.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address the server listens on.
    pub listen_addr: SocketAddr,
    /// Value reported to clients in the initial handshake.
    pub server_version: String,
    /// Maximum number of concurrent client connections (0 = unlimited).
    pub max_connections: usize,
    /// Accounts the server will authenticate against. Empty by default: no
    /// default/hardcoded credentials are shipped.
    pub users: Vec<UserCredential>,
    /// Where table data is persisted. `None` (the default) keeps everything
    /// in memory only — nothing survives a restart.
    pub data_dir: Option<PathBuf>,
    /// Largest client packet payload (in bytes) the server will accept. A
    /// packet declaring more than this is rejected before its payload is
    /// buffered, bounding how much memory one client can force the server to
    /// allocate. Defaults to 64 MiB, matching MySQL 8.0's `max_allowed_packet`.
    pub max_allowed_packet: usize,
    /// Minimum severity for structured log lines emitted to stderr. Defaults
    /// to `Info`.
    pub log_level: LogLevel,
    /// TLS configuration. `None` (the default) disables TLS: the server does
    /// not advertise `CLIENT_SSL` and all traffic is plaintext. When set, the
    /// server offers TLS and upgrades connections that request it.
    pub tls: Option<TlsConfig>,
    /// The auth plugin advertised in the initial handshake. Clients compute
    /// their first auth-response with it; accounts configured for the other
    /// plugin are moved onto theirs with an auth-switch. Defaults to
    /// `caching_sha2_password`, matching MySQL 8.0.
    pub default_auth_plugin: AuthPlugin,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            // MySQL's default port is 3306.
            listen_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3306),
            server_version: format!("8.0.0-mysql-rust-{}", env!("CARGO_PKG_VERSION")),
            max_connections: 0,
            users: Vec::new(),
            data_dir: None,
            max_allowed_packet: 64 * 1024 * 1024,
            log_level: LogLevel::Info,
            tls: None,
            default_auth_plugin: AuthPlugin::CachingSha2Password,
        }
    }
}

/// A configured account: a username, its auth plugin, and the plugin-specific
/// stored password verifier.
#[derive(Debug, Clone)]
pub struct UserCredential {
    pub username: String,
    /// Which auth plugin this account authenticates with.
    pub plugin: AuthPlugin,
    /// The stored password verifier (`None` = no password required):
    /// `mysql_native_password` → `SHA1(SHA1(password))` (20 bytes);
    /// `caching_sha2_password` → `SHA256(SHA256(password))` (32 bytes). The
    /// plaintext is hashed at construction and never retained.
    pub(crate) verifier: Option<Vec<u8>>,
}

impl UserCredential {
    /// Build a `mysql_native_password` account from a plaintext password; the
    /// plaintext is hashed immediately and not retained. An empty password
    /// means the account requires no password.
    pub fn with_password(username: impl Into<String>, password: &str) -> Self {
        let verifier = if password.is_empty() {
            None
        } else {
            Some(native_password::hash_password(password.as_bytes()).to_vec())
        };
        UserCredential {
            username: username.into(),
            plugin: AuthPlugin::MysqlNativePassword,
            verifier,
        }
    }

    /// Build a `caching_sha2_password` account (the MySQL 8.0 default plugin)
    /// from a plaintext password; the plaintext is hashed immediately and not
    /// retained. An empty password means the account requires no password.
    pub fn with_caching_sha2_password(username: impl Into<String>, password: &str) -> Self {
        let verifier = if password.is_empty() {
            None
        } else {
            Some(caching_sha2::hash_password(password.as_bytes()).to_vec())
        };
        UserCredential {
            username: username.into(),
            plugin: AuthPlugin::CachingSha2Password,
            verifier,
        }
    }
}

impl Config {
    /// Build a [`Config`] from process environment variables, layered over
    /// [`Config::default`]. This is what the `mysql-rust` binary uses. Nothing
    /// is hard-coded: with no variables set the server has **no accounts** and
    /// denies every login. Recognized variables:
    ///
    /// - `MYSQLRUST_LISTEN_ADDR` — `host:port` to bind (default `127.0.0.1:3306`).
    /// - `MYSQLRUST_DATA_DIR` — directory for the on-disk log (default: in-memory).
    /// - `MYSQLRUST_USER` — a single account's username; if set, that account
    ///   is created.
    /// - `MYSQLRUST_PASSWORD` — the account's password (unset or empty means a
    ///   passwordless account).
    /// - `MYSQLRUST_AUTH_PLUGIN` — `caching_sha2_password` (default) or
    ///   `mysql_native_password` (aliases `caching_sha2` / `native` accepted,
    ///   case-insensitive).
    ///
    /// Returns [`Error::Config`] on an unparseable address or an unknown plugin.
    pub fn from_env() -> Result<Self> {
        Self::from_env_with(|key| std::env::var(key).ok())
    }

    /// Like [`from_env`](Config::from_env) but reads each variable through
    /// `get`, which returns `None` for an unset variable. This lets the parsing
    /// be unit-tested without mutating the process environment (which is global
    /// and racy across threads).
    pub fn from_env_with(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let mut config = Config::default();

        if let Some(addr) = get("MYSQLRUST_LISTEN_ADDR") {
            config.listen_addr = addr.parse().map_err(|e| {
                Error::Config(format!(
                    "MYSQLRUST_LISTEN_ADDR '{addr}' is not a valid host:port ({e})"
                ))
            })?;
        }

        if let Some(dir) = get("MYSQLRUST_DATA_DIR") {
            if !dir.is_empty() {
                config.data_dir = Some(PathBuf::from(dir));
            }
        }

        if let Some(username) = get("MYSQLRUST_USER").filter(|u| !u.is_empty()) {
            let password = get("MYSQLRUST_PASSWORD").unwrap_or_default();
            let plugin = get("MYSQLRUST_AUTH_PLUGIN");
            let credential = match plugin.as_deref().map(str::trim) {
                None | Some("") => UserCredential::with_caching_sha2_password(username, &password),
                Some(p)
                    if p.eq_ignore_ascii_case("caching_sha2_password")
                        || p.eq_ignore_ascii_case("caching_sha2") =>
                {
                    UserCredential::with_caching_sha2_password(username, &password)
                }
                Some(p)
                    if p.eq_ignore_ascii_case("mysql_native_password")
                        || p.eq_ignore_ascii_case("native") =>
                {
                    UserCredential::with_password(username, &password)
                }
                Some(other) => {
                    return Err(Error::Config(format!(
                        "MYSQLRUST_AUTH_PLUGIN '{other}' is not recognized; use \
                         'caching_sha2_password' or 'mysql_native_password'"
                    )));
                }
            };
            config.users.push(credential);
        }

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `get` closure backed by a fixed set of key/value pairs.
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    #[test]
    fn no_vars_yields_default_with_no_accounts() {
        let config = Config::from_env_with(env(&[])).expect("parse");
        assert!(config.users.is_empty(), "no accounts by default");
        assert_eq!(config.listen_addr, Config::default().listen_addr);
        assert!(config.data_dir.is_none());
    }

    #[test]
    fn user_and_password_create_a_caching_sha2_account_by_default() {
        let config = Config::from_env_with(env(&[
            ("MYSQLRUST_USER", "alice"),
            ("MYSQLRUST_PASSWORD", "s3cret"),
        ]))
        .expect("parse");
        assert_eq!(config.users.len(), 1);
        assert_eq!(config.users[0].username, "alice");
        assert_eq!(config.users[0].plugin, AuthPlugin::CachingSha2Password);
        assert!(config.users[0].verifier.is_some());
    }

    #[test]
    fn native_plugin_selected_and_passwordless_when_password_absent() {
        let config = Config::from_env_with(env(&[
            ("MYSQLRUST_USER", "guest"),
            ("MYSQLRUST_AUTH_PLUGIN", "Mysql_Native_Password"), // case-insensitive
        ]))
        .expect("parse");
        assert_eq!(config.users[0].plugin, AuthPlugin::MysqlNativePassword);
        assert!(
            config.users[0].verifier.is_none(),
            "absent password => passwordless"
        );
    }

    #[test]
    fn listen_addr_and_data_dir_are_parsed() {
        let config = Config::from_env_with(env(&[
            ("MYSQLRUST_LISTEN_ADDR", "0.0.0.0:13306"),
            ("MYSQLRUST_DATA_DIR", "/var/lib/mysqlrust"),
        ]))
        .expect("parse");
        assert_eq!(config.listen_addr.to_string(), "0.0.0.0:13306");
        assert_eq!(
            config.data_dir.as_deref(),
            Some(std::path::Path::new("/var/lib/mysqlrust"))
        );
    }

    #[test]
    fn bad_listen_addr_is_a_config_error() {
        let err = Config::from_env_with(env(&[("MYSQLRUST_LISTEN_ADDR", "not-an-addr")]))
            .expect_err("should reject");
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn unknown_auth_plugin_is_a_config_error() {
        let err = Config::from_env_with(env(&[
            ("MYSQLRUST_USER", "a"),
            ("MYSQLRUST_AUTH_PLUGIN", "bogus_plugin"),
        ]))
        .expect_err("should reject");
        assert!(matches!(err, Error::Config(_)));
    }
}
