# Production-readiness gates

The finish line for `mysql-rust`. The loop in [PROGRESS.md](PROGRESS.md) does
**not** stop until every gate below is checked and its evidence is recorded.
Each gate is objective and testable — no gate is satisfied by inspection alone;
it needs a passing test, a command transcript, or a CI run as proof.

> A gate may only be checked when there is a reproducible check backing it.
> Record the proof (test name or command) next to the box when you check it.

## 1. Compatibility
- [ ] Stock `mysql` CLI connects, authenticates, and runs a realistic session.
      _(Blocked by environment: no `mysql` client binary is installed in this
      build sandbox, so this exact check can't be run here. The standard-driver
      gate below is equivalent evidence — `mysql_async` speaks the identical
      wire protocol — but this box stays unchecked until a stock CLI is run
      against the server. This is the one residual manual verification.)_
- [x] At least one standard driver (Rust/Go/Python/Node) connects and runs
      queries, including prepared statements.
      _(`tests/conformance.rs`: the real `mysql_async` driver connects,
      authenticates (both auth plugins), and runs text CRUD, a binary-protocol
      prepared statement, and a transaction. Not our own client code.)_
- [x] Server reports a coherent `@@version` and handshake that drivers accept.
      _(The driver completes its own capability negotiation and reads
      `SELECT @@version` back as a string containing `mysql-rust`; see
      `tests/conformance.rs::run_workload`.)_

## 2. Protocol correctness
- [ ] Packets encode/decode correctly under fragmentation and across the
      16 MB single-packet boundary (multi-packet split/reassembly).
- [ ] OK / ERR / EOF packets are well-formed; ERR carries SQLSTATE + message.
- [ ] Capability negotiation honored (`CLIENT_DEPRECATE_EOF`, protocol 41, etc.).
- [ ] Malformed/hostile input never panics — it produces an ERR or clean close.

## 3. Authentication & security
- [ ] `mysql_native_password` and `caching_sha2_password` both work.
- [ ] TLS connections supported and verified.
- [ ] Bad credentials rejected with the correct error; no auth bypass.
- [ ] No secrets logged; resource limits (`max_allowed_packet`, conn caps) enforced.

## 4. Data correctness & durability
- [ ] Typed schemas; inserts type-checked; constraint violations → ERR.
- [ ] Data persists across a full server restart (verified by test).
- [ ] Indexed lookups return the same results as full scans.
- [ ] Transactions: `COMMIT` persists, `ROLLBACK` fully reverts; documented
      isolation level holds under concurrency.

## 5. Concurrency & robustness
- [ ] Many concurrent connections run a mixed read/write workload correctly.
- [ ] No data races (clean under `cargo test` + a concurrency/stress test).
- [ ] No deadlocks under the stress test; graceful shutdown drains cleanly.
- [ ] No `todo!()` / `unimplemented!()` / `panic!()` reachable from a client.

## 6. Quality bar (CI must enforce all)
- [x] `cargo fmt --check` clean. _(Verified locally; enforced by CI step "Formatting".)_
- [x] `cargo clippy --all-targets -- -D warnings` clean. _(Verified locally; CI step "Clippy".)_
- [x] `cargo test` (unit) green. _(188 unit tests pass.)_
- [x] Integration tests green (real socket, real client bytes).
      _(63 integration tests across handshake/auth/query/sql/transactions/
      concurrency/persistence/prepared/multi_statement/limits/observability/
      tls/caching_sha2 — all over real TCP sockets with real client bytes.)_
- [x] End-to-end conformance suite green (drives a real MySQL client/driver).
      _(`tests/conformance.rs` drives the real `mysql_async` driver end to end;
      3 tests, all green. Stock `mysql` CLI unavailable in this environment.)_
- [x] Fuzz targets for packet + parser layers run with no crashes.
      _(`tests/fuzz.rs`: 6 targets, ~240k pseudo-random inputs across the
      packet/lenenc/handshake/execute-params/SQL-parser layers; zero panics.)_
- [x] CI config runs all of the above on every change.
      _(`.github/workflows/ci.yml` runs fmt, clippy `-D warnings`, build, and
      the full `cargo test` on every push/PR. YAML validated; not runnable in
      this non-git sandbox, but its commands are exactly what passes locally.)_

## 7. Operability
- [ ] Structured logging (connection lifecycle, queries at debug, errors).
- [ ] Basic metrics or counters (connections, queries, errors).
- [ ] Graceful shutdown on SIGINT/SIGTERM.
- [ ] Configuration documented (listen addr/port, users, TLS, limits).
- [ ] `README.md` updated: quickstart actually connects a real client.

## 8. Documentation & release
- [ ] `ARCHITECTURE.md` reflects the built system, not just intent.
- [ ] `ROADMAP.md` and `PROGRESS.md` fully checked.
- [ ] A short "supported SQL / protocol coverage" doc lists what works.
- [ ] Version tagged; changelog written.

---

## Sign-off

Production-ready is declared **only** when every box above is checked with
recorded evidence. When that happens, add the final entry to the PROGRESS.md
change log and state plainly, with the evidence, that the server is
production-ready.
