# Architecture

This document describes the design of `mysql-rust` as built through Phase 6
(see [ROADMAP.md](ROADMAP.md)). Later phases (transactions, prepared
statements, TLS, replication) will extend it further.

## Overview

A client connects over TCP. The server performs a handshake, authenticates
the client, then enters a command loop where it reads queries, parses them,
executes them against a storage engine, and streams back result sets — all
framed in the MySQL wire protocol.

```
client ── TCP ──> Server (accept loop)
                    └── Connection (per client)
                          ├── protocol   (packet framing, handshake)
                          ├── auth        (verify credentials)
                          └── command loop
                                ├── query::parser    (SQL -> Statement)
                                ├── query::executor  (Statement -> QueryResult)
                                └── storage::Storage (data access)
```

## Layers

### Server (`server/`)
Owns the listening socket and the async accept loop (`tokio`, multi-threaded
runtime). `Server::serve` accepts a pre-bound `TcpListener` and a `shutdown`
future, so `run`/`run_until` (OS signals or a caller-supplied future,
respectively) are thin wrappers over one seam that both production and
tests use. Each accepted connection becomes its own `tokio::task`, tracked
in a `JoinSet` so shutdown can drain them (bounded by
`SHUTDOWN_DRAIN_TIMEOUT`) instead of dropping them mid-request.
`Config::max_connections`, when set, is enforced with a `tokio::sync::Semaphore`;
a connection that arrives with no free permit gets a real `ER_CON_COUNT_ERROR`
(1040) instead of a silently dropped socket.

### Connection (`server/connection.rs`)
Holds per-client state and drives the async lifecycle: handshake -> auth ->
command phase. Each connection owns its socket, a sequence-id counter for
protocol framing, and an `Arc` clone of the server's storage (see below) —
the `Arc` is what lets simultaneously-open connections see each other's
writes in real time, not just after a reconnect.

### Protocol (`protocol/`)
The MySQL wire protocol. `packet` handles framing: a 3-byte little-endian
payload length, a 1-byte sequence id, then the payload. `handshake` builds
`HandshakeV10`/parses `HandshakeResponse41`/encodes `AuthSwitchRequest`.
`capabilities` and `command` hold the `CLIENT_*`/`COM_*` constants. `lenenc`
implements length-encoded integers and strings (read and write). `response`
builds OK/ERR packets, including the mapping from crate-wide `Error` to a
MySQL error code/SQLSTATE. `resultset` encodes text-protocol result sets,
respecting `CLIENT_DEPRECATE_EOF` and representing SQL `NULL` as the wire's
dedicated marker rather than the text "NULL".

### Auth (`auth/`)
Validates the client's handshake response. `native_password` implements
`mysql_native_password` (SHA1-based challenge/response) on top of a
hand-rolled `sha1` module (kept dependency-free deliberately — see
CLAUDE.md). `caching_sha2_password` (the 8.0 default) is still Phase 9.
`Authenticator` looks up credentials from `Config::users` and verifies.

### Query (`query/`)
`parser` tokenizes SQL text and runs a recursive-descent parser over a small
but real grammar (`CREATE TABLE`, `INSERT`, `SELECT` with an optional
single-predicate `WHERE`), producing a `Statement` AST; parse errors name
the offending token, MySQL-style. `executor` runs a `Statement` against a
`Storage` backend: it validates/coerces literals against each column's
declared type, routes an equality `WHERE` on the primary key through the
storage layer's indexed lookup instead of a scan, and produces a
`QueryResult` (columns + rows, or an affected-row count).

### Storage (`storage/`)
A `Storage` trait abstracts the data layer so the executor stays
engine-agnostic; its methods take `&self` (not `&mut self`) specifically so
one instance can be shared via `Arc` across every connection. `value.rs`
defines the typed `Value`/`ColumnType`/schema types. `engine.rs`'s
`InMemoryStorage` holds tables behind a `RwLock` (concurrent reads, exclusive
writes) plus a primary-key `HashMap` index, and optionally mirrors every
mutation to an on-disk append-only log (`log.rs`) that's replayed on
`InMemoryStorage::open`/`open_in_dir` — simple, hand-rolled, and explicitly
not a page/B-tree engine; that remains a longer-term goal if persistence
needs outgrow it.

## Error handling

One crate-wide `Error` enum (`error.rs`) with a `Result<T>` alias. Layers
return typed errors that map cleanly onto MySQL ERR packets at the protocol
boundary (`protocol::response::ErrPacket::from_error`).

## Concurrency

`tokio`, multi-threaded runtime, one task per connection (Phase 6). Storage
is shared via `Arc<InMemoryStorage>` constructed once in `Server::serve` and
cloned into each `Connection`; the `RwLock`/`Mutex` inside it are never held
across an `.await` point, since `Storage`'s trait methods are synchronous
and called-and-released within a single non-yielding expression. This is
genuine parallelism (not just async interleaving on one thread), verified by
`tests/concurrency.rs`'s many-simultaneous-clients stress test.
