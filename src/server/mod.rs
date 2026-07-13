//! TCP listener and the connection accept loop.

mod connection;
mod tls;

pub use connection::Connection;

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::config::Config;
use crate::observability::{LogLevel, Observability};
use crate::protocol::ErrPacket;
use crate::storage::InMemoryStorage;
use crate::Result;

/// How long to wait for in-flight connections to finish once shutdown
/// begins before giving up and exiting anyway.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// `ER_CON_COUNT_ERROR`.
const ER_CON_COUNT_ERROR: u16 = 1040;

/// The top-level server: owns the listening socket and accepts connections.
pub struct Server {
    config: Config,
    /// Source of unique per-connection ids (MySQL's "thread id"), reported
    /// in each client's handshake. Starts at 1, as real MySQL servers do.
    next_connection_id: AtomicU32,
    /// Structured logging + metrics, shared with every connection.
    observability: Arc<Observability>,
}

impl Server {
    /// Create a new server with the given configuration.
    pub fn new(config: Config) -> Self {
        let observability = Arc::new(Observability::new(config.log_level));
        Server {
            config,
            next_connection_id: AtomicU32::new(1),
            observability,
        }
    }

    /// Replace the server's observability bundle — lets a caller (e.g. a
    /// test, or an operator wiring up metrics export) hold a clone of the
    /// same `Arc` the server uses and read its counters.
    pub fn with_observability(mut self, observability: Arc<Observability>) -> Self {
        self.observability = observability;
        self
    }

    /// The server's shared logger + metrics.
    pub fn observability(&self) -> &Arc<Observability> {
        &self.observability
    }

    /// Bind `config.listen_addr` and serve connections until an OS shutdown
    /// signal arrives (Ctrl+C, or SIGTERM too on Unix), draining in-flight
    /// connections before returning.
    pub async fn run(&self) -> Result<()> {
        self.run_until(shutdown_signal()).await
    }

    /// Like [`Server::run`], but shuts down when `shutdown` resolves instead
    /// of listening for OS signals.
    pub async fn run_until(&self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let listener = TcpListener::bind(self.config.listen_addr).await?;
        self.serve(listener, shutdown).await
    }

    /// Serve connections on an already-bound `listener` until `shutdown`
    /// resolves. Split out from `run`/`run_until` so a caller that needs
    /// the actual bound address (e.g. binding an ephemeral port) can bind
    /// first and inspect `listener.local_addr()` before serving.
    pub async fn serve(
        &self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()>,
    ) -> Result<()> {
        self.observability.logger.log(
            LogLevel::Info,
            "server_listening",
            &[
                ("addr", &self.config.listen_addr.to_string()),
                ("version", &self.config.server_version),
            ],
        );

        let storage = Arc::new(match &self.config.data_dir {
            Some(dir) => InMemoryStorage::open_in_dir(
                dir,
                self.config.sync_policy,
                self.config.checkpoint_threshold_bytes,
            )?,
            None => {
                // PERFORMANCE_DURABILITY_PLAN.md D8: a *database server*
                // whose default configuration silently keeps everything in
                // RAM surprises operators in the worst possible way — this
                // project hit exactly that in practice (PROGRESS.md,
                // 2026-07-12: a dev session ran entirely in-memory before
                // anyone noticed). `None` stays the default (tests depend
                // on it; embedded/ephemeral use is legitimate), but it's
                // never silent again.
                self.observability.logger.log(
                    LogLevel::Warn,
                    "volatile_mode",
                    &[(
                        "hint",
                        "running without persistence -- data will not survive a restart; \
                         set MYSQLRUST_DATA_DIR to persist it",
                    )],
                );
                InMemoryStorage::new()
            }
        });
        let connection_limit = (self.config.max_connections > 0)
            .then(|| Arc::new(Semaphore::new(self.config.max_connections)));

        let mut tasks = JoinSet::new();
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, peer)) => {
                            // MySQL itself sets this on every session
                            // (PERFORMANCE_DURABILITY_PLAN.md P5): without
                            // it, Nagle's algorithm can hold a small write
                            // (this protocol sends plenty, e.g. an OK
                            // packet right after a result set) until the
                            // peer's delayed ACK fires, a ~40ms stall for
                            // no reason on a loopback-latency link.
                            // Best-effort -- a failure here doesn't affect
                            // correctness, only (rarely) latency, so it's
                            // not worth rejecting an otherwise-good socket
                            // over.
                            let _ = stream.set_nodelay(true);
                            let connection_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
                            self.spawn_connection(&mut tasks, stream, peer, connection_id, &storage, &connection_limit);
                        }
                        Err(err) => {
                            self.observability.metrics.error();
                            self.observability.logger.log(
                                LogLevel::Warn,
                                "accept_error",
                                &[("error", &err.to_string())],
                            );
                        }
                    }
                }
                _ = &mut shutdown => {
                    self.observability.logger.log(
                        LogLevel::Info,
                        "shutdown_started",
                        &[("active_connections", &self.observability.metrics.snapshot().connections_active.to_string())],
                    );
                    break;
                }
            }
        }

        drop(listener);
        let all_finished = tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, async {
            while tasks.join_next().await.is_some() {}
        })
        .await
        .is_ok();
        if !all_finished {
            self.observability.logger.log(
                LogLevel::Warn,
                "shutdown_drain_timeout",
                &[("still_running", &tasks.len().to_string())],
            );
        }

        let snap = self.observability.metrics.snapshot();
        self.observability.logger.log(
            LogLevel::Info,
            "shutdown_complete",
            &[
                ("connections_total", &snap.connections_total.to_string()),
                ("queries_total", &snap.queries_total.to_string()),
                ("errors_total", &snap.errors_total.to_string()),
            ],
        );

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_connection(
        &self,
        tasks: &mut JoinSet<()>,
        stream: TcpStream,
        peer: std::net::SocketAddr,
        connection_id: u32,
        storage: &Arc<InMemoryStorage>,
        connection_limit: &Option<Arc<Semaphore>>,
    ) {
        let permit = match connection_limit {
            Some(sem) => match Arc::clone(sem).try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    // At the limit: reject with a real MySQL error rather
                    // than silently dropping the socket.
                    self.observability.metrics.error();
                    self.observability.logger.log(
                        LogLevel::Warn,
                        "connection_rejected",
                        &[
                            ("reason", "too_many_connections"),
                            ("peer", &peer.to_string()),
                        ],
                    );
                    tasks.spawn(async move {
                        let _ = reject_too_many_connections(stream).await;
                    });
                    return;
                }
            },
            None => None,
        };

        let config = self.config.clone();
        let storage = Arc::clone(storage);
        let observability = Arc::clone(&self.observability);
        observability.metrics.connection_opened();
        observability.logger.log(
            LogLevel::Info,
            "connection_opened",
            &[
                ("connection_id", &connection_id.to_string()),
                ("peer", &peer.to_string()),
            ],
        );

        tasks.spawn(async move {
            let _permit = permit; // held for the connection's lifetime, released on drop
            let mut conn = Connection::new(
                stream,
                &config,
                connection_id,
                storage,
                Arc::clone(&observability),
            );
            let result = conn.handle().await;
            observability.metrics.connection_closed();
            match result {
                Ok(()) => observability.logger.log(
                    LogLevel::Info,
                    "connection_closed",
                    &[("connection_id", &connection_id.to_string())],
                ),
                Err(err) => {
                    observability.metrics.error();
                    observability.logger.log(
                        LogLevel::Warn,
                        "connection_error",
                        &[
                            ("connection_id", &connection_id.to_string()),
                            ("error", &err.to_string()),
                        ],
                    );
                }
            }
        });
    }
}

async fn reject_too_many_connections(mut stream: TcpStream) -> Result<()> {
    let err = ErrPacket::new(ER_CON_COUNT_ERROR, b"08004", "Too many connections");
    let packet = err.to_packet(0)?;
    stream.write_all(&packet.encode()).await?;
    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    // A signal handler that fails to install is a startup-time bug, not a
    // runtime condition to recover from.
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
