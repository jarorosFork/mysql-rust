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
