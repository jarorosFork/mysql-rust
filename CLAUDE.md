# CLAUDE.md

Guidance for Claude Code (and humans) working in this repository.

## Continuous-implementation protocol (read first)

This repo is meant to be driven to **production-ready** without stopping early.
The engine for that is [PROGRESS.md](PROGRESS.md):

1. Read [PROGRESS.md](PROGRESS.md) and [ROADMAP.md](ROADMAP.md).
2. Take the **first unchecked task** in roadmap order and implement it fully —
   no `todo!()` left on the path you touched.
3. Verify: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo build`, `cargo test`, plus the task's own acceptance check. All green
   before you check the box.
4. Check the box in PROGRESS.md + ROADMAP.md, add a change-log line, and
   continue to the next task.
5. **Do not stop** until every box is checked and every gate in
   [PRODUCTION_READINESS.md](PRODUCTION_READINESS.md) passes. Only pause for a
   genuine blocker that needs the user, and say exactly what you need.

To run this autonomously across turns, use `/loop` (e.g.
`/loop continue the mysql-rust implementation per PROGRESS.md`).

## What this is

`mysql-rust` is a from-scratch, MySQL-compatible database server written in
Rust. The long-term goal is a server that speaks the MySQL client/server
wire protocol well enough that standard MySQL clients and drivers can
connect, authenticate, and run queries against it.

**Status: Phases 0-6 complete (see [PROGRESS.md](PROGRESS.md) for the exact
current line).** The server does a real handshake, authenticates via
`mysql_native_password`, runs a genuine SQL subset (`CREATE TABLE` /
`INSERT` / `SELECT ... WHERE`) with typed columns, a primary-key index, and
on-disk persistence, and serves many concurrent clients on a `tokio` runtime
with graceful shutdown and connection limits. Not yet: transactions,
prepared statements, TLS/`caching_sha2_password`, or the full
production-readiness bar in [PRODUCTION_READINESS.md](PRODUCTION_READINESS.md).

## Commands

```bash
cargo build          # compile
cargo run            # run the server (binds 127.0.0.1:3306 by default)
cargo check          # fast type-check
cargo test           # run tests
cargo clippy --all-targets -- -D warnings   # lints — keep this clean
cargo fmt            # format before committing
```

The default listen port is 3306; if a local MySQL already uses it, change
`Config::listen_addr` or make the port configurable. `Config::data_dir`
(default `None`) opts into on-disk persistence; `Config::max_connections`
(default `0` = unlimited) caps concurrent clients.

## Layout

```
src/
  main.rs              binary entry point (#[tokio::main]; thin: config -> Server::run)
  lib.rs               crate root, module declarations, Error/Result re-exports
  config.rs            Config struct + defaults, UserCredential
  error.rs             crate-wide Error enum and Result alias
  server/
    mod.rs             Server: binds the socket, async accept loop, shutdown, connection limits
    connection.rs      Connection: per-client async lifecycle (handshake/auth/commands)
  protocol/
    mod.rs             wire-protocol module root
    packet.rs          Packet framing (3-byte length + seq id + payload)
    handshake.rs       HandshakeV10 / HandshakeResponse41 / AuthSwitchRequest
    capabilities.rs    CLIENT_* capability flag constants
    command.rs         COM_* command byte constants
    lenenc.rs          length-encoded integers/strings (read + write)
    response.rs        OkPacket / ErrPacket (incl. Error -> ERR mapping)
    resultset.rs        text-protocol result set encoding (incl. NULL marker)
  auth/
    mod.rs             Authenticator, AuthOutcome
    native_password.rs mysql_native_password challenge/response
    sha1.rs            hand-rolled SHA-1 (dependency-free, see Conventions)
  query/
    mod.rs             query module root
    parser.rs          tokenizer + AST + recursive-descent parser
    executor.rs        Statement -> QueryResult against Storage
  storage/
    mod.rs             Storage trait
    engine.rs          InMemoryStorage: RwLock<HashMap> tables, optional persistence
    value.rs           Value/ColumnType/ColumnSchema/TableSchema
    log.rs             append-only on-disk log (encode/decode/replay)
```

## Conventions

- **Errors:** return `crate::Result<T>` and add variants to `error::Error`
  rather than panicking. `todo!()` is acceptable for not-yet-built paths;
  replace it with real logic or a proper `Error` as you go.
- **No `unwrap()` / `expect()`** in library code on paths that can fail at
  runtime — reserve them for genuinely-impossible cases and tests. Prefer
  `.unwrap_or_else(|e| e.into_inner())` for lock-poison recovery over
  `.lock().unwrap()`.
- **Dependencies added intentionally.** Phases 0-5 were deliberately
  std-only. `tokio` was added in Phase 6 (see `Cargo.toml` for the itemized
  feature-by-feature rationale) once the single-connection path was proven —
  add further dependencies the same way: only when genuinely needed, with
  the reasoning written down. `auth::sha1` is hand-rolled rather than
  pulling in a crypto crate, since it's small, fully testable against known
  vectors, and used only for a legacy challenge-response scheme.
- Keep `main.rs` thin — logic lives in the library crate so it stays testable.
- Run `cargo fmt` and `cargo clippy --all-targets -- -D warnings` before committing.

See `ARCHITECTURE.md` for the design and `ROADMAP.md` for the phased plan.

## Reference

MySQL client/server protocol:
<https://dev.mysql.com/doc/dev/mysql-server/latest/PAGE_PROTOCOL.html>
