//! Command-phase command byte constants.
//!
//! Reference: <https://dev.mysql.com/doc/dev/mysql-server/latest/page_protocol_command_phase.html>

/// Close the connection.
pub const COM_QUIT: u8 = 0x01;
/// Run a SQL statement (text protocol).
pub const COM_QUERY: u8 = 0x03;
/// Check the connection is alive; replies with OK.
pub const COM_PING: u8 = 0x0e;
/// Prepare a statement; replies with `COM_STMT_PREPARE_OK`.
pub const COM_STMT_PREPARE: u8 = 0x16;
/// Execute a prepared statement with bound parameters (binary protocol).
pub const COM_STMT_EXECUTE: u8 = 0x17;
/// Reset a prepared statement's accumulated state.
pub const COM_STMT_RESET: u8 = 0x1a;
/// Deallocate a prepared statement.
pub const COM_STMT_CLOSE: u8 = 0x19;
