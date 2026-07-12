//! Shared helpers for integration tests: a minimal, blocking client that
//! speaks the wire protocol directly, plus a way to run the real (async)
//! `Server` in the background so tests stay plain, synchronous `#[test]`s.
//!
//! Mirrors `server::Connection`'s own read loop, including the persistent
//! read buffer — a single `read()` can return several back-to-back packets
//! coalesced by the OS (routine on loopback once a response spans more than
//! one packet, e.g. a result set), and a fresh per-call buffer would silently
//! discard everything past the first one.

#![allow(dead_code)] // not every test file uses every helper here

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

use tokio::sync::oneshot;

use mysql_rust::config::Config;
use mysql_rust::observability::Observability;
use mysql_rust::protocol::Packet;
use mysql_rust::server::Server;

const READ_CHUNK: usize = 4096;

/// A bare-bones MySQL protocol client for tests.
pub struct TestClient {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl TestClient {
    pub fn connect(addr: SocketAddr) -> Self {
        TestClient {
            stream: TcpStream::connect(addr).expect("connect"),
            buf: Vec::new(),
        }
    }

    /// Read exactly one full packet, blocking until it arrives. Bytes read
    /// past the end of that packet are kept for the next call.
    pub fn read_packet(&mut self) -> Packet {
        let mut chunk = [0u8; READ_CHUNK];
        loop {
            if let Some((packet, consumed)) = Packet::parse(&self.buf).expect("valid framing") {
                self.buf.drain(..consumed);
                return packet;
            }
            let n = self.stream.read(&mut chunk).expect("read from server");
            assert!(
                n > 0,
                "server closed the connection before sending a full packet"
            );
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    pub fn write_packet(&mut self, packet: &Packet) {
        self.stream
            .write_all(&packet.encode())
            .expect("write to server");
    }

    /// Write raw bytes straight to the socket, bypassing packet framing — for
    /// crafting a header that deliberately lies about its payload length
    /// (e.g. to exercise `max_allowed_packet`). Errors are ignored: the
    /// server may have already closed the connection in response.
    pub fn write_raw(&mut self, bytes: &[u8]) {
        let _ = self.stream.write_all(bytes);
    }

    /// Read raw bytes directly, bypassing packet framing — e.g. to observe
    /// the connection close after `COM_QUIT`. Any bytes already buffered by
    /// `read_packet` are returned first.
    pub fn read_raw(&mut self, out: &mut [u8]) -> usize {
        if !self.buf.is_empty() {
            let n = out.len().min(self.buf.len());
            out[..n].copy_from_slice(&self.buf[..n]);
            self.buf.drain(..n);
            return n;
        }
        self.stream.read(out).expect("read from server")
    }
}

/// A real [`Server`] running on its own background OS thread (with its own
/// tokio runtime), bound to an OS-assigned ephemeral port. Dropping this
/// triggers a graceful shutdown and joins the thread — the same seam
/// `Server::run_until` gives production callers, driven here by a
/// `oneshot` instead of an OS signal.
pub struct TestServer {
    pub addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl TestServer {
    /// Start a server on `127.0.0.1:0`, blocking until it's actually
    /// listening (so callers can connect immediately with no retry/race).
    pub fn start(config: Config) -> Self {
        Self::start_inner(config, None)
    }

    /// Like [`TestServer::start`], but injects a shared [`Observability`] the
    /// caller keeps a clone of — so a test can read the server's live metrics
    /// counters after driving traffic through it.
    pub fn start_with_observability(config: Config, observability: Arc<Observability>) -> Self {
        Self::start_inner(config, Some(observability))
    }

    fn start_inner(config: Config, observability: Option<Arc<Observability>>) -> Self {
        let (addr_tx, addr_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().expect("build tokio runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind ephemeral port");
                addr_tx
                    .send(listener.local_addr().expect("local addr"))
                    .expect("report bound addr");

                let mut server = Server::new(config);
                if let Some(obs) = observability {
                    server = server.with_observability(obs);
                }
                let shutdown = async {
                    let _ = shutdown_rx.await;
                };
                if let Err(err) = server.serve(listener, shutdown).await {
                    eprintln!("test server error: {err}");
                }
            });
        });

        let addr = addr_rx
            .recv()
            .expect("server thread reported its bound address");
        TestServer {
            addr,
            shutdown_tx: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    /// Connect a fresh client to this server.
    pub fn connect(&self) -> TestClient {
        TestClient::connect(self.addr)
    }

    /// Signal shutdown without waiting for the drain to finish — lets a
    /// test observe in-flight connections still working (or new ones being
    /// refused) while the server winds down. Idempotent.
    pub fn trigger_shutdown(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }

    /// Wait for the background thread to finish (implies `serve` returned,
    /// i.e. the drain completed or timed out). Panics if the thread itself
    /// panicked. Does not trigger shutdown by itself — call
    /// `trigger_shutdown` first, or rely on `Drop` for that.
    pub fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            thread.join().expect("server thread panicked");
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Start a server on an ephemeral port and return one connected client —
/// the common case for single-client tests. The server is deliberately
/// leaked to keep running for the rest of the test process: these are
/// short-lived test binaries, and single-client tests don't need explicit,
/// timed shutdown. Tests that need multiple concurrent clients (or want to
/// exercise shutdown itself) should use [`TestServer::start`] directly.
pub fn spawn_server(config: Config) -> TestClient {
    let server = TestServer::start(config);
    let client = server.connect();
    std::mem::forget(server);
    client
}

/// Pull the 20-byte scramble (part 1 + part 2) out of a raw `HandshakeV10` payload.
pub fn extract_scramble(payload: &[u8]) -> [u8; 20] {
    let mut pos = 1; // protocol_version
    let version_end = payload[pos..].iter().position(|&b| b == 0).unwrap() + pos;
    pos = version_end + 1;
    pos += 4; // connection_id

    let mut scramble = [0u8; 20];
    scramble[..8].copy_from_slice(&payload[pos..pos + 8]);
    pos += 8; // part 1
    pos += 1; // filler
    pos += 2; // capability flags (lower)
    pos += 1; // character set
    pos += 2; // status flags
    pos += 2; // capability flags (upper)
    pos += 1; // auth-plugin-data length
    pos += 10; // reserved
    scramble[8..20].copy_from_slice(&payload[pos..pos + 12]); // part 2
    scramble
}
