//! MySQL client/server capability flags.
//!
//! Reference: `mysql_com.h` /
//! <https://dev.mysql.com/doc/dev/mysql-server/latest/group__group__cs__capabilities__flags.html>
//!
//! Only the flags currently negotiated by this crate are defined here; add
//! more as later phases need them (e.g. `CLIENT_SSL` for TLS, Phase 9).

/// Use the 4.1+ (improved) password hashing.
pub const CLIENT_LONG_PASSWORD: u32 = 0x0000_0001;
/// Client may send multiple `;`-separated statements in one `COM_QUERY`.
pub const CLIENT_MULTI_STATEMENTS: u32 = 0x0001_0000;
/// Client may send a database name to switch to after connecting.
pub const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
/// Use the 4.1 protocol (`HandshakeResponse41` instead of the legacy reply).
pub const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
/// Client wants to switch to TLS after the initial handshake (it sends an
/// SSLRequest, then the real handshake response over the encrypted channel).
pub const CLIENT_SSL: u32 = 0x0000_0800;
/// Use the 4.1+ 20-byte auth scramble.
pub const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
/// Client/server support pluggable authentication methods.
pub const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;
/// The auth-response in `HandshakeResponse41` is length-encoded rather than
/// a single length byte.
pub const CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 0x0020_0000;
/// Result sets omit the trailing EOF packet after the last row.
pub const CLIENT_DEPRECATE_EOF: u32 = 0x0100_0000;
