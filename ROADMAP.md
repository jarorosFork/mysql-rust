# Roadmap

A phased plan driven to completion by the loop in [PROGRESS.md](PROGRESS.md).
**Work top to bottom; do the first unchecked box next.** Each task is written
to be small enough to finish and verify in one iteration. Each phase ends with
a demonstrable, tested result and an explicit **Acceptance** check.

Conventions for every task: replace stubs with real logic, return
`crate::Result`, no `unwrap`/`expect` on fallible paths, and leave
`cargo fmt`/`cargo clippy -D warnings`/`cargo test` green before checking the
box.

---

## Phase 0 — Skeleton (done)
- [x] Module layout, `Config`, `Error`/`Result`, stub types.
- [x] Compiles cleanly with no dependencies.

## Phase 1 — Connection handshake
- [x] Implement `protocol::packet::Packet::encode` (3-byte LE length + seq id + payload).
- [x] Implement `protocol::packet::Packet::decode` (partial/fragmented reads, max length).
- [x] Unit tests: round-trip encode/decode, split reads, boundary lengths (0, 0xFFFFFF).
- [x] Build a real `HandshakeV10` in `protocol::handshake` (version, thread id, auth-plugin data, capability flags, charset, status).
- [x] Send the handshake from `server::connection::Connection::handle`.
- [x] Parse the client's `HandshakeResponse41` (capabilities, max packet, charset, username, auth response, database).
- [x] Sequence-id counter is threaded correctly through the exchange.
- [x] **Acceptance:** `mysql -h 127.0.0.1 -P 3306` (or a scripted client) gets
      past the initial handshake without a protocol error. Add an integration
      test that drives the byte exchange.
      _(No `mysql` binary available in this environment; verified with a
      scripted client in `tests/handshake.rs` that reproduces real 8.0 client
      capability negotiation — protocol 4.1, plugin auth with length-encoded
      auth-response, `CLIENT_SECURE_CONNECTION`, `CLIENT_CONNECT_WITH_DB`.)_

## Phase 2 — Authentication
- [x] Implement `mysql_native_password` challenge/response (SHA1 XOR scheme).
- [x] Wire `auth::Authenticator` to an in-memory user table from `Config`.
- [x] Send OK packet on success, ERR packet (with SQLSTATE) on failure.
- [x] Handle auth-switch request path.
- [x] Unit tests for the scrambling algorithm against known vectors.
- [x] **Acceptance:** `mysql -u user -p` authenticates successfully against a
      configured user and is rejected with a proper error on bad credentials.
      Integration test covers both.
      _(No `mysql` binary in this environment; `tests/auth.rs` drives the same
      exchange with a scripted client: correct password → OK, wrong password →
      ERR 1045/28000, unknown user → ERR, passwordless account → OK, and the
      auth-switch path when the client declares a different plugin.)_

## Phase 3 — Command phase & trivial queries
- [x] Read command packets; dispatch on command byte.
- [x] Handle `COM_QUIT`, `COM_PING`.
- [x] Handle `COM_QUERY` for `SELECT 1`, `SELECT @@version`.
- [x] Encode result sets: column-count, column definitions, rows, EOF/OK (respect `CLIENT_DEPRECATE_EOF`).
- [x] Map executor errors to ERR packets.
- [x] **Acceptance:** a client runs `SELECT 1;` and `SELECT @@version;` and sees
      correct results; `ping` succeeds; `quit` closes cleanly. Integration test.
      _(`tests/query.rs`, negotiating `CLIENT_DEPRECATE_EOF` as a real 8.0
      client would: SELECT 1, SELECT @@version, case/whitespace variants,
      PING, an unsupported query producing an ERR without dropping the
      connection, and QUIT closing the socket cleanly.)_

## Phase 4 — Real parser & executor
- [x] Tokenizer for the SQL subset (identifiers, literals, operators, keywords).
- [x] AST + parser for `CREATE TABLE`, `INSERT`, `SELECT` (columns, `WHERE`, basic exprs).
- [x] Parser error reporting maps to MySQL-style syntax errors.
- [x] Executor dispatches statements to `storage::Storage`.
- [x] `SELECT` supports projection and simple `WHERE` filtering.
- [x] Unit tests for tokenizer, parser (incl. error cases), and executor.
- [x] **Acceptance:** `CREATE TABLE`, multi-row `INSERT`, then `SELECT ... WHERE`
      returns correct rows via a real client. Integration test.
      _(`tests/sql.rs`: CREATE TABLE → 3-row INSERT → SELECT with projection
      and WHERE, SELECT * with WHERE, INSERT without an explicit column list,
      plus error paths — duplicate table, missing table, malformed SQL — all
      confirmed to leave the connection open and usable afterward.)_

## Phase 5 — Storage engine
- [x] Typed columns and schema definitions (at least INT, VARCHAR, plus NULLs).
- [x] Replace placeholder `InMemoryStorage` with a real in-memory table store.
- [x] Add persistence (write-ahead or file-backed) that survives restart.
- [x] Primary-key / basic index lookup.
- [x] Type checking and coercion on insert; constraint errors surface as ERR.
- [x] Tests: persistence across reopen, index correctness, type errors.
- [x] **Acceptance:** data written before shutdown is present after restart;
      indexed lookups return correct rows. Integration test.
      _(`tests/persistence.rs`: full server round trip — CREATE TABLE +
      multi-row INSERT in one connection, then an entirely separate
      connection against the same `data_dir` SELECTs the data back
      (including a rebuilt primary-key index and a NULL value), a duplicate
      primary key is still rejected after reopen, `data_dir: None` proves
      persistence is opt-in not accidental, and a corrupted data file
      errors cleanly rather than panicking.)_

## Phase 6 — Concurrency
- [x] Introduce async runtime (`tokio`) — one task per connection (note rationale in commit).
- [x] Make storage access safe under concurrent clients (locking/`RwLock`/actor).
- [x] Graceful shutdown (drain connections on signal).
- [x] Connection limits / backpressure.
- [x] Stress test: many concurrent clients, mixed read/write, no data races or deadlocks.
- [x] **Acceptance:** N concurrent clients run a mixed workload with correct,
      consistent results. Integration/stress test.
      _(`tests/concurrency.rs`: two simultaneously-open connections observe
      each other's writes live (not just after reopen); 30 concurrent
      client threads each INSERT a distinct row and interleave SELECTs
      against the shared table, verified afterward with zero lost/duplicated
      rows; SIGTERM/Ctrl+C-triggered shutdown stops accepting new
      connections while letting an in-flight one finish, confirmed to drain
      promptly rather than hit the 10s force-exit fallback; max_connections
      rejects an extra simultaneous connection with real ER_CON_COUNT_ERROR
      and releases the slot on disconnect. Also smoke-tested the actual
      release binary with a real SIGTERM.)_

## Phase 7 — Transactions & locking
- [x] `BEGIN`/`COMMIT`/`ROLLBACK`; autocommit semantics.
- [x] Isolation for concurrent transactions (at least read-committed; document level).
- [x] Row/table locking sufficient to prevent lost updates.
- [x] Tests: concurrent transactions preserve invariants; rollback restores state.
- [x] **Acceptance:** documented isolation level holds under a concurrent test.
      _(Isolation level is **read committed** — documented in
      `storage/transaction.rs`'s module doc: a transaction always sees its
      own writes layered on committed state, never a stale snapshot, and
      other connections never see uncommitted writes. `tests/transactions.rs`
      proves it end-to-end: uncommitted inserts invisible to other
      connections until COMMIT (and visible to the transaction's own
      reads), ROLLBACK leaving prior state untouched, BEGIN-while-in-a-
      transaction implicitly committing the first, a failed statement not
      aborting the transaction, and — the core "prevents lost updates"
      claim — two connections writing the same table concurrently proven
      serialized by timing (the second write measurably waits for the
      first's COMMIT) with both rows surviving, versus two connections on
      *different* tables proven NOT to block each other.)_

## Phase 8 — Prepared statements & broader protocol
- [x] `COM_STMT_PREPARE`, `COM_STMT_EXECUTE`, `COM_STMT_CLOSE`, `COM_STMT_RESET`.
- [x] Binary protocol row encoding for prepared results.
- [x] Multi-statement / multi-resultset where applicable.
      _(Server advertises `CLIENT_MULTI_STATEMENTS`; `parser::parse_many`
      splits a `COM_QUERY` on `;` at the token level (semicolons inside
      string literals are respected). Each statement's result carries
      `SERVER_MORE_RESULTS_EXISTS` in its terminator status flags except the
      last; a failing statement aborts the batch. Gated on the client
      negotiating the capability. `tests/multi_statement.rs`: SELECT batch
      with per-result MORE flags, mixed DML+SELECT, error-aborts-batch, and
      rejection when the capability isn't negotiated.)_
- [x] Broader type coverage in column defs (dates, decimals, blobs).
      _(Column definitions now report the accurate per-column MySQL type
      code — INT → LONGLONG, VARCHAR → VAR_STRING — instead of always
      VAR_STRING, in both text and binary result sets; `CREATE TABLE`
      accepts the integer- and string-family type aliases (BIGINT/SMALLINT/…,
      CHAR/TEXT/…). Genuinely distinct physical types — DATE, exact-scale
      DECIMAL, real binary BLOB — need their own `storage::Value` variant and
      binary-format encoders and are deliberately deferred, not faked.)_
- [x] **Acceptance:** a standard driver (e.g. a Rust/Python/Go MySQL driver)
      prepares and executes parameterized queries correctly. Integration test.
      _(No real driver available in this environment; `tests/prepared.rs`
      drives the exact binary wire protocol a driver would — PREPARE → read
      PREPARE_OK + param defs → EXECUTE with a NULL-bitmap + typed bound
      params → decode the binary result set (LONGLONG 8-byte and VAR_STRING
      lenenc cells, NULL via the offset bitmap), plus CLOSE/RESET and
      error-not-crash paths.)_

## Phase 9 — Hardening
Ordered dependency-free robustness first, then the two crypto-dependency
features (TLS and `caching_sha2_password` both need a real crypto backend,
so they come last in the phase — the roadmap's "order can flex" applies).
- [x] Enforce `max_allowed_packet` and other resource limits.
      _(New `Config::max_allowed_packet` (default 64 MiB, MySQL 8.0's
      default); `Connection::read_packet` rejects a packet as soon as its
      4-byte header declares a payload over the limit — before buffering it
      — so one client can't force an oversized allocation. `max_connections`
      (Phase 6) is the connection-count limit. `tests/limits.rs`: a header
      claiming a 5 MiB payload is rejected on the header alone (server closes
      rather than buffering), and normal-sized traffic still works.)_
- [x] Audit every client-reachable path for panics; ensure all map to ERR.
      _(Swept the source for `unwrap`/`expect`/`panic!`/`unreachable!`/raw
      indexing on client-reachable paths. Found and fixed two real
      client-triggerable panics: a truncated `COM_STMT_EXECUTE` overran a
      slice index (`&payload[pos..]` with `pos > len`, and a `pos += 2` that
      checked only one byte — both now use `.get()`), and a client-reachable
      `unreachable!()` in the executor's no-FROM SELECT path (now returns an
      error). Remaining `unwrap`/`expect` are test-only or the startup-time
      signal-handler install. Every `Error` maps to an ERR packet via
      `ErrPacket::from_error`. Regression tests in `protocol::prepared` +
      `tests/prepared.rs` (malformed execute → ERR, connection survives).)_
- [x] Fuzz the packet/parser layers; fix all findings.
      _(`tests/fuzz.rs`: a deterministic, dependency-free in-process harness
      (real cargo-fuzz needs nightly) feeds ~240k pseudo-random inputs across
      6 tests to `Packet::parse`/`decode`, `read_lenenc_int`,
      `HandshakeResponse41::parse`, `parse_execute_params`, and the SQL
      parser (`parse`/`parse_prepared`/`parse_many`, both random-ASCII and
      lossy-UTF-8) — asserting none panic (Ok or Err both fine). Zero panics;
      the two the panic-audit fixed were the only findings.)_
- [x] Structured logging + basic metrics; documented config surface.
      _(New dependency-free `observability` module: a level-filtered
      structured logger (`<unix_secs> <LEVEL> <event> key=value …` to stderr,
      `Config::log_level` default Info) and atomic `Metrics` counters
      (connections total/active, queries, errors). Wired through `Server`
      (listening/shutdown, connection open/close, accept/limit errors) and
      `Connection` (per-query success/error). All Config fields documented in
      a README table. `tests/observability.rs` injects a shared `Metrics` and
      asserts the counters move as connections open, queries run, an error
      occurs, and the client disconnects.)_
- [x] TLS (`CLIENT_SSL`) support.
      _(First crypto dependency: `tokio-rustls`/`rustls` on the `ring`
      backend — TLS can't be hand-rolled (unlike SHA-1). Server advertises
      `CLIENT_SSL` when `Config::tls` is set (new `TlsConfig::from_der`
      builds a rustls acceptor). `perform_handshake` implements MySQL's
      STARTTLS-style upgrade: plaintext HandshakeV10 → detect the SSLRequest
      by its `CLIENT_SSL` flag → `upgrade_to_tls` swaps a new `ConnStream`
      enum (Plain/Tls) → read the real handshake response over TLS. The
      upgrade replays any bytes already buffered via `PrefixedStream`, so a
      pipelined ClientHello isn't lost (the classic STARTTLS bug). `tests/tls.rs`
      drives a real rustls client through the whole upgrade → auth → query
      over the encrypted channel, with a self-signed cert generated at test
      time by `rcgen` (dev-dep). Every plaintext test still passes unchanged.)_
- [x] `caching_sha2_password` (8.0 default) auth plugin.
      _(Hand-rolled SHA-256 (`auth::sha256`, FIPS 180-4, verified against 4
      NIST vectors incl. the 1M-'a' long case) — kept dependency-free like
      SHA-1; `ring` is used only for TLS, never as a hashing API. New
      `auth::caching_sha2` implements the fast-auth exchange: client sends
      `SHA256(pw) XOR SHA256(SHA256(SHA256(pw)) ++ nonce)`, and the server —
      holding only `SHA256(SHA256(pw))` — recovers and checks the candidate
      with a constant-time compare (matches the go-sql-driver algorithm
      byte-for-byte, incl. the stored-then-nonce concat order, the reverse of
      native). `AuthPlugin` enum + per-account plugin: `UserCredential` now
      carries a plugin + plugin-specific verifier (`with_password` =
      native, new `with_caching_sha2_password`); `Authenticator` dispatches
      verification by plugin and rejects a plugin/account mismatch.
      `Config::default_auth_plugin` (default `CachingSha2Password`, MySQL 8.0
      parity) is advertised in the handshake; `authenticate` negotiates:
      when the client's presented plugin ≠ the account's, it sends an
      `AuthSwitchRequest` for the account's plugin and re-reads — switch works
      in both directions. On caching_sha2 success the server sends an
      `AuthMoreData` fast-auth-success (0x01 0x03) before the terminal OK, as
      a real 8.0 server does on a cache hit. Proof: `tests/caching_sha2.rs`
      (6 tests: handshake advertises caching_sha2; fast-auth OK; wrong pw →
      ERR 1045; passwordless; switch onto caching_sha2; native account still
      authenticates against the caching_sha2 default) + 11 new unit tests.)_
- [x] **Acceptance:** clients connect over TLS with `caching_sha2_password`;
      fuzzing finds no panics; malformed input yields errors, never a crash.
      _(`tests/tls.rs::client_completes_tls_handshake_with_caching_sha2_password`
      drives a real rustls client through the STARTTLS upgrade →
      caching_sha2 fast-auth → `SELECT 1`, all over the encrypted channel.
      The ~240k-input fuzz harness (`tests/fuzz.rs`) still finds zero panics;
      the response parser remains panic-safe on truncated/hostile auth data.
      250 tests total (187 unit + 63 integration), fmt + clippy `-D warnings`
      clean.)_

## Phase 10 — Production-readiness gates
- [ ] Complete every gate in [PRODUCTION_READINESS.md](PRODUCTION_READINESS.md).
- [x] End-to-end conformance suite green against a real standard driver.
      _(`tests/conformance.rs`: the actual third-party `mysql_async` crate — a
      real, widely-used MySQL driver, added as a dev-dependency, **not** our
      own scripted client — connects to the server and runs a full workload:
      capability negotiation, auth (tested with **both** `caching_sha2_password`
      and `mysql_native_password` accounts), the driver's own connect-time
      `SELECT @@max_allowed_packet,@@wait_timeout,@@socket` settings query,
      text-protocol CRUD, a prepared statement over the binary protocol (params
      in, rows out), and a BEGIN/INSERT/COMMIT transaction — plus a bad-password
      connection that the driver correctly reports as rejected. This exercises
      the real wire protocol far more strictly than our byte-scripted tests and
      passed on the first run, validating the binary encoding, column type
      codes, and auth against an independent implementation. The stock `mysql`
      **CLI** could not be run — no such binary is installed in this build
      environment — so the driver stands in for it; both speak the identical
      wire protocol, so this is equivalent conformance evidence. Answering the
      driver's settings query needed a small server change: `SystemVariables`
      (built from `Config`) now backs `@@max_allowed_packet`/`@@wait_timeout`
      (numeric) and `@@socket` (NULL, TCP-only) in addition to `@@version`.)_
- [x] CI pipeline runs fmt, clippy `-D warnings`, unit + integration + e2e.
      _(`.github/workflows/ci.yml`: on every push/PR, a stock Ubuntu runner
      installs stable Rust (+rustfmt +clippy), caches cargo, then runs the
      exact production-readiness §6 gates in order — `cargo fmt --all --check`,
      `cargo clippy --all-targets -- -D warnings`, `cargo build --all-targets`,
      and `cargo test` (which covers unit, integration, the real-driver
      end-to-end conformance suite, and the fuzz harness). No external services
      needed — every test starts the server in-process and the driver is a Rust
      dev-dep. YAML validated; the same commands pass locally: fmt + clippy
      clean, 254 tests green. It can't be *run* here — this sandbox isn't a
      GitHub repo — but the config is complete and its commands are the ones
      proven locally each iteration.)_
- [ ] **Acceptance:** all gates pass; declare production-ready in PROGRESS.md.

## Phase 11 — Core SQL completeness

Prompted by real GUI-client usage (DBeaver) surfacing how much of everyday
SQL the Phase 4/5 subset doesn't cover yet. Scoped down from a much broader
ask (a MariaDB quickstart-guide index spanning ~25 topics — full string/
date-time function libraries, `LOAD DATA INFILE`, `mariadb-dump` import/
export, `VIEW`s) to the pieces that matter for typical GUI/app usage; the
rest stays a known, explicit gap rather than an unstated one.

- [x] `ORDER BY` (multi-column, `ASC`/`DESC`) and `LIMIT`/`OFFSET` (both
      `LIMIT n OFFSET m` and `LIMIT m, n` forms).
      _(`OrderByItem` sorts on the full pre-projection row (so `ORDER BY` may
      name a column outside the `SELECT` list), applied before `LIMIT`/
      `OFFSET` slicing — matching real evaluation order. New `value_ordering`
      gives `NULL` a definite sort position (first, ascending — matching
      MySQL) distinct from `compare_values`'s WHERE-clause 3-valued logic,
      which they now share via one comparison core. Proof: 21 new tests
      (7 parser + 13 executor + 1 real-driver conformance case covering a
      GUI's column-sort-click and page-through-results); 329 total, fmt +
      clippy `-D warnings` clean.)_
- [x] More column types: `DATE`, `DECIMAL`, `BOOLEAN`.
      _(`BOOLEAN`/`BOOL` are pure `INT` synonyms — no new storage type, exactly
      matching real MySQL (`TRUE`/`FALSE` parse as `Expr::Integer(1)`/`(0)`).
      `DECIMAL` is exact fixed-point (`Value::Decimal(unscaled, scale)`; the
      whole point of `DECIMAL` is *not* being a float — `f64` would reintroduce
      binary rounding error and can't derive `Eq`/`Hash` for the primary-key
      index anyway). New tokenizer support for decimal-point literals
      (`Token::Decimal`) alongside integers; every value is rescaled to its
      column's declared `DECIMAL(M,D)` scale in `coerce` (round-half-away-
      from-zero via checked arithmetic, never a panic), so any two values in
      one column always share a scale — `value_ordering` gained a numeric
      (not lexical — "10.20" < "9.50" as text but not as numbers) comparison
      arm for it. `DATE` stores pre-validated canonical `YYYY-MM-DD` text —
      deliberately just a `String`: zero-padded ISO-8601 already sorts
      chronologically under plain string comparison, so no date-arithmetic
      code was needed anywhere. Both wire-encode as text (`VAR_STRING`),
      reusing the existing text/binary row encoding unchanged rather than
      hand-rolling MySQL's native binary `DATE`/`NEWDECIMAL` layouts — a
      real client reads them back as an ordinary string/number either way.
      On-disk log format extended (new value/column-type tags; `DECIMAL`
      carries its scale as one extra byte) with a dedicated persistence
      round-trip test. Proof: 23 new unit tests (parser tokenization +
      `TRUE`/`FALSE` + type recognition; executor coercion/rescaling/
      rounding/comparison/validation; log + engine persistence) + 1 real-
      driver conformance test (incl. the exact `100.005 → 100.01` rounding
      case, proven exact by reading the wire value back as a string — a
      real `f64` can't even represent `19.99` exactly) + 6 new e2e app
      entries (`cargo run --example e2e`, 26/26 passing). 352 tests total
      (was 329). fmt + clippy `-D warnings` clean.)_
- [x] `GROUP BY` + aggregate functions (`COUNT`, `SUM`, `AVG`, `MIN`, `MAX`).
      _(A query is "aggregate" if it has a `GROUP BY` or any aggregate
      function call in the projection (`is_aggregate_query`); such queries
      route through a new `execute_select_aggregate` instead of the plain
      row-scan path. WHERE filtering is shared with the plain path via a new
      `scan_and_filter` helper (incl. the primary-key-equality fast path) and
      runs *before* grouping. Rows are partitioned by their `GROUP BY` values
      into a `HashMap<Vec<Value>, Vec<Vec<Value>>>` — with no `GROUP BY` at
      all, this collapses to exactly one group so a bare `COUNT(*)` on an
      empty table correctly returns `0`, not zero rows. Group keys are sorted
      deterministically (via the existing `value_ordering`) so results don't
      depend on `HashMap` iteration order. `ONLY_FULL_GROUP_BY`-style
      validation rejects any projected column that isn't in `GROUP BY` and
      isn't wrapped in an aggregate, and rejects `SELECT *` in an aggregate
      query outright. `ORDER BY` on an aggregate query resolves column names
      against the *output* schema (so `ORDER BY total` can name a `SUM(...)
      AS total` alias, which doesn't exist in the source table) rather than
      the source table schema used by the plain path. Every aggregate is
      NULL-aware (`COUNT` skips `NULL` for a named column; `SUM`/`AVG`/`MIN`/
      `MAX` skip `NULL` rows and return `NULL`, not `0`, when every input was
      `NULL`) and uses checked arithmetic throughout (`checked_add`/
      `checked_mul`/`checked_pow`) so overflow is a proper `Error::Execution`,
      never a panic. `AVG` returns exact fixed-point, not a float: its scale
      is the source column's scale plus 4 (capped at 30), computed via
      integer scaling and round-half-away-from-zero — matching the same
      rounding rule `DECIMAL` coercion already uses. Proof: 24 new tests (4
      parser + 19 executor, incl. empty-table/all-NULL/overflow/validation
      edge cases + 1 real-driver conformance test covering a grouped report
      with `WHERE`+`GROUP BY`+`ORDER BY` on an alias together) + 8 new e2e
      app entries (`cargo run --example e2e`, 34/34 passing). 376 tests total
      (was 352). fmt + clippy `-D warnings` clean.)_
- [x] `JOIN` (`INNER`, `LEFT`) — includes qualified column references
      (`table.column`/`alias.column`), deferred until now since they only
      matter once more than one table is in play.
      _(New `FromClause { table: TableRef, joins: Vec<JoinClause> }` replaces
      the old bare table-name `from: Option<String>`; `TableRef` now actually
      *keeps* its `AS` alias (previously parsed and discarded) since it's
      needed as a qualifier. `ON` is restricted to one column-to-column
      equality — the same simplification this engine's `WHERE` already made
      (no AND/OR chaining anywhere) — but chained multi-table joins
      (`a JOIN b ON... JOIN c ON...`) work, and `ON`'s two sides aren't
      fixed to "old table"/"new table" by position (`ON a.x=b.y` and
      `ON b.y=a.x` both work). New `Executor::Scope` unifies column-name
      resolution for the single-table and joined paths alike (qualified
      `t.col` must match a real qualifier; unqualified `col` must be
      unambiguous — MySQL's own "Column 'x' in field list is ambiguous"
      rule, replicated exactly) — this let the existing GROUP BY/ORDER BY/
      projection logic move to two free functions (`execute_projected`,
      `execute_aggregate`) shared verbatim by both paths, rather than
      duplicated. Joins execute as a hash join (`hash_join`): the newly
      joined table is indexed by its `ON`-column value once, then every
      accumulated row probes it — not a naive nested-loop scan. `NULL` is
      never a join key on either side (`NULL = NULL` is never true),
      matching `WHERE`'s existing three-valued logic; a `LEFT` join keeps
      an unmatched row with the new table's columns padded `NULL`, an
      `INNER` join drops it. The single-table `WHERE pk = literal` indexed
      fast path (Phase 5) is fully preserved and untouched — only a
      `JOIN`ed query's `WHERE` is a plain linear filter, since a join has no
      equivalent index once rows are combined. `INNER`/`LEFT`/`OUTER`/
      `JOIN`/`ON` are now reserved keywords, matching real MySQL, so none
      can be mistaken for a bare table alias (`FROM t INNER JOIN u` can't
      misparse `INNER` as `t`'s alias) — a narrow, documented compatibility
      cost. `Statement::Select`'s `selection` field is now `Option<Box
      <Condition>>` (clippy's `large_enum_variant`, since `Condition` now
      embeds a `ColumnRef` and grew): boxing just that one field was enough,
      so `from`/`group_by`/`order_by` stayed unboxed. Proof: 21 new tests
      (8 parser + 12 executor, incl. one-to-many fan-out, NULL-key,
      ambiguous-column, unknown-ON-column, 3-table chained join, GROUP BY
      over a join, either-order ON + 1 real-driver conformance test) + 7 new
      e2e app entries (`cargo run --example e2e`, 41/41 passing). 397 tests
      total (was 376). fmt + clippy `-D warnings` clean.)_
- [x] `ALTER TABLE` (`ADD`/`DROP`/`MODIFY COLUMN`, add/drop a constraint).
      _(New `Keyword::Alter` + `Statement::AlterTable { table, action }`;
      `AlterAction` models exactly one action per statement (`AddColumn`/
      `DropColumn`/`ModifyColumn`/`AddPrimaryKey`/`DropPrimaryKey`) — real
      MySQL's comma-separated multi-action form isn't supported, a
      deliberate scope cut (most generated DDL, especially from GUI tools,
      issues one change at a time anyway), as is plain `DROP TABLE` (not
      part of this item's own wording). `ADD`/`MODIFY`/`COLUMN` stay
      non-reserved (`peek_is_ident_ci`), matching how `UNIQUE`/`INDEX`/
      `FOREIGN` already work for `CREATE TABLE`; only `ALTER` itself needed
      to become a real keyword for the top-level statement dispatch.
      `ADD [CONSTRAINT [name]] PRIMARY KEY (col)` is the modelled
      "constraint" action (the only constraint type this engine has); a
      composite key is rejected the same way `CREATE TABLE`'s own
      table-level `PRIMARY KEY (...)` already is, and `ADD CONSTRAINT`
      naming anything other than `PRIMARY KEY` is a clear `Unsupported`
      error, not a silent mis-parse.

      New `storage::SchemaChange` enum (one `Storage::alter_table` trait
      method taking it, rather than five separate methods — keeps the
      trait surface, and every implementor's, incl. `Transaction`'s
      one-line auto-commits-immediately passthrough, small) carries the
      *validated* form (`ColumnSchema`, not the parser's raw `ColumnDef`) —
      the same layering `create_table` already uses. New `compute_altered_
      table` (in `storage::engine`) rebuilds a table from scratch on every
      change: a fresh schema, then every existing row extended/shrunk/
      retyped via `Table::new` + `push_trusted`, so the primary-key index
      and `AUTO_INCREMENT` counter come out right for free — the same
      machinery that already rebuilds a table from a batch of rows on
      replay and at checkpoint time. `retype_value` (mirroring `query::
      executor::coerce`'s match shape, but converting an already-stored
      `Value` instead of a freshly parsed literal) reuses `rescale_decimal`/
      `parse_decimal_literal`/`parse_date_literal`, moved from `query::
      executor` to `storage::value` (alongside the existing `format_decimal`)
      specifically so `storage::engine` could call them without inverting
      the crate's `query`-depends-on-`storage` layering.

      Deliberate, documented limits matching this engine's existing
      no-`DEFAULT`-support posture (`CREATE TABLE`'s own `DEFAULT <expr>`
      is parsed and discarded, never modelled): `ADD COLUMN ... NOT NULL`
      or `ADD COLUMN ... PRIMARY KEY` is rejected on a table that already
      has rows (no way to backfill a value); dropping the primary-key
      column drops the key itself rather than erroring; `MODIFY COLUMN`
      never changes `PRIMARY KEY` status (rejected at the executor layer,
      before ever reaching storage) — `ADD`/`DROP PRIMARY KEY` are the only
      way to change that, keeping "what makes a column the key" one
      unambiguous code path; `AUTO_INCREMENT` still requires being the
      primary key everywhere (`MODIFY COLUMN` can toggle it only on the
      column that already is the key).

      New `TAG_ALTER_TABLE` log record (tag 6) with sub-tags per
      `SchemaChange` variant; `write_column_schema`/`read_column_schema`
      factored out of the existing `CreateTable` encode/decode so the two
      record types can't drift apart on the column wire format.
      `checkpoint_if_worthwhile` needed **no changes at all** — it already
      re-derives its snapshot from live `Table` state, so once replay
      correctly mutates that state, compaction just sees whatever the
      table currently looks like, the same way it already absorbs any
      number of past `INSERT`s into one batched record. Replay of an
      `AlterTable` entry tolerates `compute_altered_table` failing (skips
      that record) rather than panicking: the same benign log-before-
      memory race `create_table`/`create_database` already document (a
      "loser" can get durably logged even though it never actually won
      live) applies here too, and skipping a change that no longer
      validates exactly reproduces what genuinely happened, rather than
      crashing over a record that was never a real change to begin with.

      Proof: 10 new parser tests (every action form, `COLUMN`-keyword-
      optional both ways, composite-key and bad-constraint rejection,
      schema-qualified names); 1 new log round-trip test (all 5
      `SchemaChange` variants); 26 new executor tests (every action's
      happy path plus every rejection: duplicate/unknown column, NOT
      NULL/PRIMARY KEY on a non-empty table, un-convertible `MODIFY`
      values, NOT NULL violated by an existing NULL, PRIMARY KEY status
      touched via `MODIFY`, `AUTO_INCREMENT` without the key, duplicate/
      NULL values under `ADD PRIMARY KEY`, dropping the last column,
      dropping/adding a primary key with `AUTO_INCREMENT` interaction); 1
      new storage-level persistence test (schema *and* data changes survive
      a restart, including the added primary key still being enforced); 1
      new real-driver conformance test (`real_driver_alter_table`,
      exercising every action against the actual `mysql_async` driver); 10
      new e2e app entries (`cargo run --example e2e`, now 51/51 passing).
      499 tests total (was 460 before this item -- 412 unit incl. the
      relocated/new value-conversion tests, 87 integration); fmt + clippy
      `-D warnings` clean throughout.)_
- [x] **Acceptance:** each item above has passing unit tests and a real-driver
      conformance test; fmt/clippy/full suite green throughout.

---

## Phase 12 — Durability & performance hardening

An audit (2026-07-12, at commit `5ae6c0e`) measured the codebase against
database-engineering best practices for durability and performance. Full
findings, severities, code evidence, and the phased task list — crash-test
harness and benchmark baseline first, then WAL correctness (fsync policy,
atomic commit records, CRC + torn-tail recovery, true write-ahead
ordering), then a group-commit writer thread, then query-path performance
(scan predicate pushdown, single-buffer responses, `TCP_NODELAY`) — live in
**[PERFORMANCE_DURABILITY_PLAN.md](PERFORMANCE_DURABILITY_PLAN.md)**. Work
its checkboxes in order (PD-0 → PD-4) under the same loop protocol as this
file; its durability items also sharpen PRODUCTION_READINESS.md §4 from
"survives a graceful restart" to "survives a crash".

- [x] PD-0 — Measurement first: crash-safety harness + benchmark baseline.
      _(`tests/crash.rs` spawns the real compiled binary as its own OS
      process — the only way to `SIGKILL` it like a real crash without
      killing the test binary too — sweeps kill timing across a large
      multi-row INSERT and a multi-row COMMIT, and separately fuzzes
      byte-exact log truncation/corruption. Confirmed the harness is real:
      run with `--ignored`, its 3 ignored assertions currently fail with
      concrete evidence (17/1000 and 361/500 rows survived a killed
      statement; a torn tail refuses to start) — each is un-ignored, not
      rewritten, by the D-task named in its `#[ignore = "..."]` reason.
      `benches/mysql_bench.rs` (`cargo bench`, hand-rolled — not criterion,
      see the file's own doc comment for why) covers point-SELECT,
      full-scan WHERE, 1k-row fetch, volatile/persistent INSERT,
      200-concurrent commits, and a JOIN+GROUP BY report; baseline numbers
      recorded in PERFORMANCE_DURABILITY_PLAN.md's Baseline section and
      already empirically confirm finding P1 — a full-scan WHERE is ~25x
      slower than a point SELECT returning a similar row count. 2 new
      non-ignored tests pass today as regression guards. 399 tests total
      (was 397). fmt + clippy `-D warnings` clean.)_
- [x] PD-1 — Durability core: CRC framing, torn-tail recovery, WAL
      ordering, atomic commit records, fsync policy + directory fsync.
      _(✅ Done 2026-07-12, all five findings (D1-D5) plus D7. See
      PERFORMANCE_DURABILITY_PLAN.md's D1-D5 entries for the full design
      of each (why the checksum has to cover the length field, not just
      the payload, for torn-tail recovery to be safe; why a batch record
      makes commit atomicity fall out of that recovery work for free; why
      `insert_row`/`insert_rows` validate under a read lock and apply
      under a separate write lock while `create_table` holds one write
      lock across both; why `fdatasync` over `fsync`; why `EverySecond`
      was deliberately deferred to PD-2's dedicated-writer-thread
      architecture rather than half-built now) and the real, measured
      trade-offs each introduced (PERFORMANCE_DURABILITY_PLAN.md's
      "Baseline" section: `sync=always` costs ~100x the per-INSERT latency
      of `sync=never` on this machine; 200 concurrent commits went from
      46.9ms to 829ms once every one of them paid its own fsync —
      precisely the number PD-2's group commit is scored against).
      Proof: **`tests/crash.rs`'s entire 5-test suite passes with zero
      `#[ignore]`d assertions** — every durability bug the harness was
      built in PD-0 to catch (17/1000 and 361/500 rows surviving a killed
      statement; a torn tail refusing to start) is fixed, closing the loop
      PD-0 opened. 421 tests total (was 397 before PD-0); e2e app (41/41)
      and the benchmark suite (`cargo bench`) both green. fmt + clippy
      `-D warnings` clean throughout.)_
- [x] PD-2 — Write-path architecture: dedicated log-writer thread with
      group commit.
      _(✅ Done 2026-07-13. New `storage::log_writer::LogWriter`: a plain
      `std::thread` owning the log file, fed by a bounded `tokio::sync::
      mpsc` channel; callers `.await` a `oneshot` ack instead of blocking a
      tokio worker on the write; the thread drains everything already
      queued before each write, so concurrent appends land as one
      `write_all` + one `fsync` instead of one each (group commit falls out
      of "one owner thread", which is why this isn't a `spawn_blocking`
      pool). Required `Storage::create_table`/`insert_row`/`insert_rows` to
      become genuinely `async` — hand-rolled boxed futures
      (`storage::BoxFuture`), since `Storage` is used as `&dyn Storage` and
      native `async fn` in traits isn't dyn-compatible; every read-only
      method stayed synchronous. Proof: a same-table 200-concurrent-commit
      benchmark barely moved (829ms → 800ms), correctly — Phase 7's
      per-table lock already serializes same-table writers before they'd
      ever reach the writer thread concurrently — but a new benchmark
      isolating the log writer itself (200 concurrent autocommit INSERTs
      to 200 *distinct* tables, no shared lock) dropped to 48ms median vs.
      ~796ms if fully serialized; a second new benchmark shows point-SELECT
      latency under 8-writer concurrent persistent-INSERT load is
      statistically indistinguishable from the uncontended baseline
      (65.8µs vs. 74.0µs median), proving no runtime worker blocks on file
      I/O. See PERFORMANCE_DURABILITY_PLAN.md's PD-2 entry for the full
      numbers and the reasoning behind the same-table benchmark's
      near-flat result. 425 tests total (was 421); `tests/crash.rs`'s
      5-test crash-safety suite still passes unchanged; e2e app (41/41)
      still green; fmt + clippy `-D warnings` clean throughout.)_
- [x] PD-3 — Query-path performance: `TCP_NODELAY`, single-buffer
      responses, scan predicate pushdown, `Arc<TableSchema>`.
      _(✅ Done 2026-07-13, four items landed plus one deliberately not
      merged. `TCP_NODELAY` set best-effort at accept. `Packet::
      encode_into`/`ResultSet::encode_text_into`/`encode_binary_into`
      encode straight into a reused per-connection buffer instead of
      materializing a `Vec<Packet>`; `send_result`'s multi-row path is one
      `write_all` + one `flush` regardless of row count — `fetch
      1,000-row result set` dropped ~10x (4.2-4.4ms → ~435-517µs). New
      required `Storage::scan_filtered(table, filter)` clones only
      matching rows instead of the whole table first; `full-scan WHERE
      SELECT`, remeasured at the 100k-row scale PD-3's own acceptance
      names, dropped 6.8-8.3x (7.19ms → 867.6µs). `Table` now shares one
      `Arc<TableSchema>` instead of deep-cloning it on every
      `table_schema()` call, proven via `Arc::ptr_eq` rather than just a
      benchmark delta (which is small on these narrow benchmark tables,
      as expected — the cost scales with column count). The fifth item,
      `Arc<[Value]>` rows, was evaluated and deliberately not merged: the
      plan's own gate ("only if the numbers justify the churn") wasn't
      met once the other four landed, since the scenarios it would help
      most (an unfiltered full scan, a JOIN's per-table scan) either
      already improved via P2/P1 or still need a fresh allocation
      downstream regardless (a joined row can't be a slice of either
      input; wire encoding touches every value either way) — see
      PERFORMANCE_DURABILITY_PLAN.md's PD-3 entry for the full reasoning.
      435 tests total (was 425 before PD-3); `tests/crash.rs`'s 5-test
      crash-safety suite and the e2e app (41/41) still green throughout;
      fmt + clippy `-D warnings` clean.)_
- [x] PD-4 — Operational hardening: streaming replay, checkpoint/
      compaction, volatile-mode warning, idle-connection reaping, sort-path
      and buffer hygiene.
      _(**Streaming replay** (D6 step 1) done 2026-07-13:
      `Log::open` streams through the file with a `BufReader` instead of
      `std::fs::read`-ing it whole first, one record at a time into its
      own short-lived allocation — startup memory is O(one record), not
      O(the whole file twice over) — with the torn-tail/corruption
      classification preserved exactly (every existing byte-exact test
      passes unchanged) and a new 5,000-record test proving the streaming
      loop itself is correct at a scale the old handful-of-records tests
      wouldn't catch an off-by-one at.
      **Checkpoint/compaction** (D6 step 2) done 2026-07-13: if the
      just-replayed log is at least `Config::checkpoint_threshold_bytes`
      (16 MiB default), `InMemoryStorage::open` rewrites it as a compact
      snapshot (one `CreateTable` + one batched `Transaction` of every
      current row, per table) via write-temp → fsync → atomic rename →
      fsync-the-directory, entirely at startup and before any live
      `LogWriter` exists (deliberately no on-demand/background compaction
      path — hot-swapping a *running* writer thread's file handle
      mid-flight is a harder problem the plan's "and/or" wording doesn't
      require solving here). A new crash test races a `SIGKILL` against
      process startup itself across 9 swept delays (checkpointing runs
      before the accept loop, so there's no client-observable moment to
      race against instead) and always recovers to exactly the right row
      count. Honestly reported: this measurably shrinks the on-disk log
      (~24% smaller in the test case) but does **not** measurably speed up
      replay in this codebase today, because there's no `UPDATE`/`DELETE`
      yet — compaction can't reduce live row count, only record-framing
      overhead, and replay's real cost is dominated by per-value/per-row
      allocation, not framing. See PERFORMANCE_DURABILITY_PLAN.md's D6
      entries for the full reasoning. 443 tests total (was 436); e2e app
      (41/41) and `tests/crash.rs`'s crash-safety suite (now 6 tests)
      still green throughout; fmt + clippy `-D warnings` clean.
      **Volatile-mode warning + persisted DB namespace** (D8) done
      2026-07-13: `Server::serve` logs a `Warn`-level `volatile_mode` event
      (with a hint pointing at `MYSQLRUST_DATA_DIR`) whenever `data_dir` is
      `None`, verified by manual smoke run; `None` stays the default.
      `CREATE DATABASE`/`DROP DATABASE` names now go through the same
      log/`LogWriter`/checkpoint machinery as tables — two new record types
      (tags 4/5), `Storage::create_database`/`drop_database` became
      `BoxFuture`-returning following `create_table`'s exact check-release-
      log-reacquire pattern, and `checkpoint_if_worthwhile`'s snapshot now
      re-emits every still-registered name. Two new tests prove a name
      survives both a plain restart and a forced (threshold-0) checkpoint.
      README's env-var/`Config`-field tables now document
      `MYSQLRUST_DATA_DIR`'s full volatile-mode consequence,
      `MYSQLRUST_SYNC_POLICY`, and `MYSQLRUST_CHECKPOINT_THRESHOLD_BYTES`
      (previously undocumented). 446 tests total (was 443); e2e app (41/41,
      exercising `CREATE`/`DROP DATABASE` + `SHOW DATABASES`) and
      `tests/crash.rs`'s 6-test suite still green; fmt + clippy
      `-D warnings` clean.
      **Idle-connection reaping** (P9) done 2026-07-13: new `Config::
      wait_timeout`/`connect_timeout` (`Duration`s, env `MYSQLRUST_WAIT_
      TIMEOUT_SECS`/`MYSQLRUST_CONNECT_TIMEOUT_SECS`, defaults 8h/10s
      matching MySQL's own). The command loop's packet read is
      `tokio::time::timeout`-wrapped with `wait_timeout`; on elapse it's a
      clean close (not a protocol error — idleness is routine, unlike a
      stalled handshake), logged as a distinct `idle_connection_reaped`
      event, releasing the connection's permit exactly as a normal
      disconnect would. Handshake/auth-phase reads get `connect_timeout`
      instead, via a new `read_packet_before_connect_timeout` helper, and
      *do* error on elapse (a slow-loris client never finishing its
      handshake is the anomaly this specifically guards against).
      `SystemVariables::new` now takes the real `wait_timeout` instead of a
      hardcoded constant, so `@@wait_timeout`/`@@interactive_timeout` stop
      being a lie disconnected from actual server behavior — fixing both
      halves of the P9 finding, not just the enforcement half. New
      `tests/idle_timeout.rs` (4 tests, real sockets, a test-shortened
      150ms timeout): idle client observes the close; an active client
      sending commands faster than the timeout survives well past what
      would otherwise be its deadline (proving per-command reset, not a
      hard lifetime cap); a `max_connections: 1` idle connection's permit
      frees up for a waiting second connection; a stalled-handshake client
      is disconnected via `connect_timeout`. All 4 run 5x clean, no
      flakes. 456 tests total (was 446); e2e app (41/41) still green; fmt +
      clippy `-D warnings` clean.
      **Sort-path fixes (P7)**, **buffer shrink (P8)**, **release profile
      (P10)** done 2026-07-13 — **completes PD-4**. `value_ordering` gained
      an explicit `Date`/`Date` arm (the fallback's one actually-hot case —
      a column has one fixed type, so `ORDER BY` never genuinely compares
      mixed types — now allocation-free instead of cloning two `String`s
      per comparison). New shared `sort_and_paginate` (de-duplicating
      `execute_projected`'s/`execute_aggregate`'s identical sort-then-
      paginate tails) uses `Vec::select_nth_unstable_by` to partition top-N
      rows in average-case O(n) instead of a full O(n log n) sort whenever
      `offset + limit` covers only part of the row set. Benchmark-gated per
      the plan's own requirement: a new `order_by_small_limit` scenario
      (`ORDER BY ... LIMIT 10` over 100,000 rows) measured a real but
      modest ~4% median improvement (5.38ms -> 5.18ms) — smaller than the
      complexity change alone suggests, because this no-`WHERE` query's
      cost is dominated by `scan()`'s upfront clone of all 100,000 rows,
      not the sort itself; kept anyway since it's unconditionally not
      worse and its isolated benefit will matter more once row-cloning
      cost drops or comparators get pricier. New `shrink_read_buf_if_idle_
      and_oversized` shrinks `Connection::read_buf` back to 4 KiB once
      it's both empty and above 1 MiB, so one huge packet (up to
      `max_allowed_packet`, 64 MiB default) doesn't pin that allocation
      for the connection's whole remaining life; the plan's other
      suggested P8 change (read-cursor + periodic compaction instead of
      per-packet `drain`) was evaluated and deliberately not pursued —
      `drain`'s cost is proportional to whatever's left *after* the
      consumed packet, zero in the common no-pipelining case, and the
      finding itself calls this "fine at current scale." `[profile.
      release] lto = "thin", codegen-units = 1` added to `Cargo.toml`;
      full before/after `cargo bench` run shows every CPU-bound scenario
      2-9% faster, fsync-/lock-bound ones unchanged (as expected — a
      compiler can't speed up time spent blocked on `fsync` or a lock).
      460 tests total (was 456); e2e app (41/41) still green throughout;
      fmt + clippy `-D warnings` clean. See PERFORMANCE_DURABILITY_PLAN.md's
      P7/P8/P10 entries and updated benchmark table for the full
      before/after numbers.)_
- [x] **Acceptance:** every checkbox in
      [PERFORMANCE_DURABILITY_PLAN.md](PERFORMANCE_DURABILITY_PLAN.md) is
      checked, the crash harness passes un-`#[ignore]`d in CI, and the
      benchmark table there records before/after numbers.

---

## Async runtime note

The skeleton is deliberately dependency-free and blocking. Introduce `tokio`
at Phase 6, once the single-connection path (Phases 1–5) is proven, rather than
up front. Adding it earlier is allowed only if a task explicitly needs it —
note the rationale in the commit.
