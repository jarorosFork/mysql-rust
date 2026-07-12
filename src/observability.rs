//! Structured logging and basic runtime metrics.
//!
//! Deliberately dependency-free (the crate ships only `tokio`): a small
//! level-filtered structured logger that emits `key=value` lines to stderr,
//! and a set of atomic counters. This is enough observability to see what a
//! running server is doing — connection lifecycle, query volume, error rate
//! — without pulling in a logging framework.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Severity levels, ordered least-to-most severe so a configured minimum
/// filters everything below it (`Info` logs `Info`/`Warn`/`Error`, drops
/// `Debug`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    fn label(self) -> &'static str {
        match self {
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }

    /// Parse a level name (case-insensitive); unknown names yield `None`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "debug" => Some(LogLevel::Debug),
            "info" => Some(LogLevel::Info),
            "warn" | "warning" => Some(LogLevel::Warn),
            "error" => Some(LogLevel::Error),
            _ => None,
        }
    }
}

/// A minimal structured logger. Immutable after construction (so it's safe to
/// share behind an `Arc` with no locking).
#[derive(Debug)]
pub struct Logger {
    min_level: LogLevel,
}

impl Logger {
    pub fn new(min_level: LogLevel) -> Self {
        Logger { min_level }
    }

    /// Emit a structured event if `level` is at or above the configured
    /// minimum. `fields` are appended as space-separated `key=value` pairs.
    pub fn log(&self, level: LogLevel, event: &str, fields: &[(&str, &str)]) {
        if level < self.min_level {
            return;
        }
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        eprintln!("{}", format_line(secs, level, event, fields));
    }
}

/// Format one structured log line: `<unix_secs> <LEVEL> <event> k=v k=v`.
/// Split out from I/O so it can be tested directly.
fn format_line(unix_secs: u64, level: LogLevel, event: &str, fields: &[(&str, &str)]) -> String {
    let mut line = format!("{unix_secs} {} {event}", level.label());
    for (key, value) in fields {
        line.push(' ');
        line.push_str(key);
        line.push('=');
        line.push_str(value);
    }
    line
}

/// Server-wide counters, incremented from connection handling. All atomic, so
/// they're updated lock-free from every connection task.
#[derive(Debug, Default)]
pub struct Metrics {
    connections_total: AtomicU64,
    connections_active: AtomicU64,
    queries_total: AtomicU64,
    errors_total: AtomicU64,
}

/// An immutable point-in-time reading of [`Metrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub connections_total: u64,
    pub connections_active: u64,
    pub queries_total: u64,
    pub errors_total: u64,
}

impl Metrics {
    pub fn new() -> Self {
        Metrics::default()
    }

    /// A new connection was accepted.
    pub fn connection_opened(&self) {
        self.connections_total.fetch_add(1, Ordering::Relaxed);
        self.connections_active.fetch_add(1, Ordering::Relaxed);
    }

    /// A connection finished (saturating, so a double-close can't underflow).
    pub fn connection_closed(&self) {
        let mut current = self.connections_active.load(Ordering::Relaxed);
        while current > 0 {
            match self.connections_active.compare_exchange_weak(
                current,
                current - 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    /// A query (text or prepared) executed successfully.
    pub fn query_executed(&self) {
        self.queries_total.fetch_add(1, Ordering::Relaxed);
    }

    /// A query or connection error occurred.
    pub fn error(&self) {
        self.errors_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            connections_total: self.connections_total.load(Ordering::Relaxed),
            connections_active: self.connections_active.load(Ordering::Relaxed),
            queries_total: self.queries_total.load(Ordering::Relaxed),
            errors_total: self.errors_total.load(Ordering::Relaxed),
        }
    }
}

/// The logger and metrics bundled together, shared across the server and all
/// its connections behind a single `Arc`.
#[derive(Debug)]
pub struct Observability {
    pub logger: Logger,
    pub metrics: Metrics,
}

impl Observability {
    pub fn new(min_level: LogLevel) -> Self {
        Observability {
            logger: Logger::new(min_level),
            metrics: Metrics::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_line_format_is_structured() {
        let line = format_line(
            1_720_000_000,
            LogLevel::Info,
            "connection_opened",
            &[("connection_id", "5"), ("peer", "127.0.0.1:5000")],
        );
        assert_eq!(
            line,
            "1720000000 INFO connection_opened connection_id=5 peer=127.0.0.1:5000"
        );
    }

    #[test]
    fn log_line_with_no_fields() {
        let line = format_line(1, LogLevel::Error, "boom", &[]);
        assert_eq!(line, "1 ERROR boom");
    }

    #[test]
    fn level_ordering_filters_correctly() {
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Error);
    }

    #[test]
    fn level_from_name() {
        assert_eq!(LogLevel::from_name("info"), Some(LogLevel::Info));
        assert_eq!(LogLevel::from_name("WARNING"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::from_name("bogus"), None);
    }

    #[test]
    fn metrics_count_connections_and_queries() {
        let m = Metrics::new();
        m.connection_opened();
        m.connection_opened();
        m.query_executed();
        m.error();
        let snap = m.snapshot();
        assert_eq!(snap.connections_total, 2);
        assert_eq!(snap.connections_active, 2);
        assert_eq!(snap.queries_total, 1);
        assert_eq!(snap.errors_total, 1);

        m.connection_closed();
        assert_eq!(m.snapshot().connections_active, 1);
        assert_eq!(m.snapshot().connections_total, 2); // total never decrements
    }

    #[test]
    fn connection_closed_saturates_at_zero() {
        let m = Metrics::new();
        m.connection_closed(); // no open connections — must not underflow
        assert_eq!(m.snapshot().connections_active, 0);
    }
}
