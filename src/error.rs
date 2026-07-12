//! Crate-wide error type and result alias.

use std::fmt;

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// The top-level error type for the server.
#[derive(Debug)]
pub enum Error {
    /// Wraps a lower-level I/O error.
    Io(std::io::Error),
    /// The client sent a malformed or unexpected protocol packet.
    Protocol(String),
    /// Authentication failed for a connecting client.
    Auth(String),
    /// A query could not be parsed.
    Parse(String),
    /// A query failed during execution.
    Execution(String),
    /// A feature that has not been implemented yet was requested.
    Unsupported(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Protocol(m) => write!(f, "protocol error: {m}"),
            Error::Auth(m) => write!(f, "authentication error: {m}"),
            Error::Parse(m) => write!(f, "parse error: {m}"),
            Error::Execution(m) => write!(f, "execution error: {m}"),
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
