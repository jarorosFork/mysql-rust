//! The MySQL client/server wire protocol.
//!
//! Reference: the MySQL internals manual, "Client/Server Protocol":
//! <https://dev.mysql.com/doc/dev/mysql-server/latest/PAGE_PROTOCOL.html>

pub mod capabilities;
pub mod command;
pub mod handshake;
pub mod lenenc;
pub mod packet;
pub mod prepared;
pub mod response;
pub mod resultset;

pub use handshake::{AuthSwitchRequest, Handshake, HandshakeResponse41, SCRAMBLE_LEN};
pub use packet::Packet;
pub use prepared::{parse_execute_params, StmtPrepareOk};
pub use response::{AuthMoreData, ErrPacket, OkPacket, CACHING_SHA2_FAST_AUTH_SUCCESS};
pub use resultset::{Cell, ColumnDefinition, ColumnType, ResultSet};
