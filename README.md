# mysql-rust

A MySQL-compatible database server written from scratch in Rust.

> **Status: Phases 0-9 of 10 complete** (see [PROGRESS.md](PROGRESS.md) for
> the exact current line). The server performs a real handshake, authenticates
> via either `caching_sha2_password` (the MySQL 8.0 default) or
> `mysql_native_password` — over TLS or plaintext — and runs a genuine SQL
> subset (`CREATE TABLE` / `INSERT` / `SELECT ... WHERE`) with typed columns, a
> primary-key index, and on-disk persistence, plus transactions and prepared
> statements, serving many concurrent clients on a `tokio` runtime with
> graceful shutdown, connection/packet limits, and structured logs/metrics.
> Remaining: the Phase 10 [production-readiness bar](PRODUCTION_READINESS.md)
> — an end-to-end conformance suite against a stock client and a CI pipeline.

## Goals

- Speak enough of the MySQL client/server protocol that standard MySQL
  clients and drivers can connect and run queries.
- A clean, modular, well-tested Rust codebase.
- Pluggable storage engines behind a single trait.

## Quickstart

```bash
# Create an account and start the server — no credentials are hard-coded.
MYSQLRUST_USER=alice MYSQLRUST_PASSWORD=s3cret cargo run
# -> mysql-rust listening on 127.0.0.1:3306 (version 8.0.0-mysql-rust-0.1.0)
```

Then connect with any standard MySQL client or driver — e.g. the `mysql` CLI:

```bash
mysql -h 127.0.0.1 -P 3306 -u alice -ps3cret
```

Without `MYSQLRUST_USER` the server starts with **no accounts** and denies
every login (by design — no default credentials are shipped) and prints a hint
pointing at these variables. Compatibility isn't just self-asserted: a real
third-party driver (`mysql_async`) connects, authenticates with both auth
plugins, and runs a full text + prepared-statement + transaction workload
against the server in `tests/conformance.rs`.

### Environment variables (read by the `mysql-rust` binary)

| Variable | Default | Purpose |
|----------|---------|---------|
| `MYSQLRUST_USER` | *(unset)* | Username of the account to create. Unset ⇒ no accounts (every login denied). |
| `MYSQLRUST_PASSWORD` | *(unset)* | That account's password. Unset or empty ⇒ passwordless account. |
| `MYSQLRUST_AUTH_PLUGIN` | `caching_sha2_password` | `caching_sha2_password` or `mysql_native_password` (case-insensitive). |
| `MYSQLRUST_LISTEN_ADDR` | `127.0.0.1:3306` | `host:port` to bind. |
| `MYSQLRUST_DATA_DIR` | *(unset)* | Directory for the on-disk log (persistence). Unset ⇒ in-memory only. |

`Config::from_env()` builds this configuration (the injectable
`Config::from_env_with` makes it unit-testable); the `Config` struct below is
the full programmatic surface.

## Configuration

The server is configured through the `Config` struct (`src/config.rs`), passed
to `Server::new`. `Config::default()` is a sensible starting point; override
fields as needed.

| Field | Default | Purpose |
|-------|---------|---------|
| `listen_addr` | `127.0.0.1:3306` | TCP address/port to bind. |
| `server_version` | `8.0.0-mysql-rust-<crate version>` | Version string reported in the handshake. |
| `users` | *(empty)* | Accounts to authenticate (`UserCredential::with_password`); no default/hardcoded credentials are shipped. |
| `data_dir` | `None` | Directory for the on-disk append-only log. `None` = in-memory only (nothing survives restart). |
| `max_connections` | `0` (unlimited) | Cap on concurrent clients; extras get `ER_CON_COUNT_ERROR`. |
| `max_allowed_packet` | 64 MiB | Largest accepted packet payload; larger is rejected on the header before buffering. |
| `log_level` | `Info` | Minimum severity for structured stderr logs (`Debug`/`Info`/`Warn`/`Error`). |
| `tls` | `None` | `Some(TlsConfig)` enables TLS: the server advertises `CLIENT_SSL` and upgrades connections that request it. Build with `TlsConfig::from_der(cert_chain, key)`. |

Structured logs are emitted to stderr as `<unix_secs> <LEVEL> <event> key=value …`
lines (connection lifecycle, query errors, shutdown). Runtime counters
(connections total/active, queries, errors) live in `Server::observability()`'s
`Metrics` and can be snapshotted for export.

Both MySQL auth plugins are implemented: `caching_sha2_password` (the 8.0
default, advertised by default — `Config::default_auth_plugin`) and
`mysql_native_password`. Each account picks one
(`UserCredential::with_caching_sha2_password` / `with_password`); a client
presenting the other plugin is moved onto the account's via an auth-switch.
Over plaintext, `caching_sha2_password` uses its fast-auth challenge/response
(the password never crosses the wire); the RSA/full-authentication fallback is
not implemented (unnecessary here — the server always holds the verifier).

## Project layout

See `CLAUDE.md` for the full module map and developer commands, and
`ARCHITECTURE.md` for the intended design.

```
src/
  server/    TCP accept loop + per-connection lifecycle
  protocol/  MySQL wire-protocol packets and handshake
  auth/      client authentication
  query/     SQL parsing and execution
  storage/   pluggable storage engines
```

## Development

```bash
cargo build     # compile
cargo check     # fast type-check
cargo clippy    # lints
cargo fmt       # format
cargo test      # tests
```

## Roadmap

The phased plan is in `ROADMAP.md`; live status and the finish-line criteria
are tracked in `PROGRESS.md` and `PRODUCTION_READINESS.md`. The first milestone
is a working connection handshake so a MySQL client can complete authentication.
Development is intended to run continuously against those trackers until every
production-readiness gate passes.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
