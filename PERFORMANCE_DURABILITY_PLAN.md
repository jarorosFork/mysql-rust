# Performance & Durability Plan

An audit of `mysql-rust` against database-engineering best practices for
**durability** (does acknowledged data survive crashes?) and **performance**
(where does the design waste time/memory?), with a phased, checkbox-driven
remediation plan in the style of [ROADMAP.md](ROADMAP.md) /
[PROGRESS.md](PROGRESS.md).

**Audited at:** commit `5ae6c0e` (Phase 11 through JOIN; 397 tests green).
**Files reviewed line-by-line:** `storage/log.rs`, `storage/engine.rs`,
`storage/transaction.rs`, `server/connection.rs`, `server/mod.rs`,
`protocol/packet.rs`, `query/executor.rs`, `ARCHITECTURE.md`,
`PRODUCTION_READINESS.md`, `Cargo.toml`.

---

## Verdict in one paragraph

The codebase is honest about what it is — the log module's own doc comment
says "no fsync durability guarantees" — and its correctness-under-normal-
operation story is genuinely good (typed storage, PK index, per-table write
locks, panic-free client paths, a fuzz suite, real-driver conformance
tests). But measured against how real databases are built, **durability is
the weak axis**: an acknowledged `COMMIT` does not survive a power failure
(no fsync), a crash mid-commit can leave *half a transaction* permanently
visible (no atomic commit record), the write-ahead log is actually written
*behind* the in-memory apply (inverted WAL ordering), and — worst for
availability — a torn final record, the *expected* artifact of any crash,
makes the server **refuse to start**. Performance has one dominant flaw
(every non-indexed query deep-copies the entire table) and a cluster of
wire-path inefficiencies (per-packet syscalls, triple-buffered result sets,
no `TCP_NODELAY`). None of this is surprising for the project's stage; all
of it is fixable incrementally, and the plan below sequences the work so
correctness lands before optimization, with a crash-test harness and a
benchmark baseline built *first* so every later checkbox has a mechanical
acceptance check.

## Summary of findings

| ID | Severity | Area | Finding |
|----|----------|------|---------|
| D1 | **Critical** | Durability | No `fsync` — acknowledged writes/commits lost on power/OS failure |
| D2 | **Critical** | Durability | No atomic commit record — crash mid-`COMMIT` persists half a transaction |
| D3 | High | Durability | WAL ordering inverted: memory applied before log append |
| D4 | High | Durability | Torn log tail (normal crash artifact) makes the server refuse to start |
| D5 | High | Durability | No per-record checksums — corruption detectable only if it breaks parsing |
| D6 | Medium | Durability | No checkpoint/compaction; whole log loaded into RAM at open |
| D7 | Medium | Durability | Parent directory never fsynced after creating the data file |
| D8 | Medium | Durability | Volatile-by-default posture (`data_dir: None`), silent; DB namespace never persisted |
| P1 | **High** | Performance | `scan()` deep-copies the entire table for every non-PK query |
| P2 | High | Performance | Result sets triple-buffered; one syscall + flush **per packet** |
| P3 | High | Performance | Blocking disk I/O runs directly on tokio worker threads |
| P4 | Medium | Performance | Global log mutex; no group commit (bites hard once D1 adds fsync) |
| P5 | Medium | Performance | `TCP_NODELAY` never set on accepted sockets |
| P6 | Medium | Performance | Full `TableSchema` clone (all column names) per statement |
| P7 | Medium | Performance | Sort-path allocations; ORDER BY + LIMIT always sorts the full set |
| P8 | Low | Performance | Connection read buffer never shrinks; `drain()` memmove per packet |
| P9 | Low | Robustness | `wait_timeout` advertised but never enforced — idle connections pin permits forever |
| P10 | Low | Performance | No release-profile tuning; no benchmark suite to catch regressions |

---

## Part 1 — Durability findings

### D1 (Critical): acknowledged writes are not durable — no fsync anywhere

**Where:** `src/storage/log.rs:278-285`

```rust
fn append(&mut self, entry_bytes: &[u8]) -> Result<()> {
    ...
    self.file.write_all(&framed)?;
    self.file.flush()?;      // <-- no-op: File has no userspace buffer
    Ok(())
}
```

`std::fs::File::flush()` is documented as a no-op — `write_all` already
handed the bytes to the OS page cache, and nothing ever calls
`sync_all()`/`sync_data()`. Consequences:

- Data survives a **process** crash (page cache belongs to the kernel) —
  which is why the existing persistence tests pass — but an acknowledged
  `INSERT`/`COMMIT` is **lost on power failure or kernel panic**, up to
  however much the OS had not yet written back (typically up to ~30s).
- The server tells the client "OK" for a write that may not exist tomorrow.
  That violates the D in ACID as every real database defines it: *an
  acknowledged commit survives anything short of media failure*.

**Best practice:** fsync the log before acknowledging a commit. Because
fsync is expensive (~0.5–5 ms on SSDs, worse on cloud block storage), real
databases make the policy configurable — InnoDB's
`innodb_flush_log_at_trx_commit` (1 = fsync per commit, 2 = write per
commit + fsync per second, 0 = neither) and Postgres's `synchronous_commit`
are the reference designs.

**Fix:** add `Config::sync_policy` (`Always` | `EverySecond` | `Never`),
default `Always`; call `File::sync_data()` in `Log::append` per policy.
`sync_data` (fdatasync) over `sync_all` (fsync): metadata like mtime doesn't
need to hit disk, file *length* changes do and fdatasync covers those.
Pair with group commit (P4) so the cost amortizes under concurrency.

### D2 (Critical): `COMMIT` is not atomic — a crash can persist half a transaction

**Where:** `src/storage/transaction.rs:79-87` and
`src/query/executor.rs` (`execute_insert` row loop)

```rust
pub fn commit(self) -> Result<()> {
    let pending = ...;
    for (table, rows) in pending {
        for row in rows {
            self.storage.insert_row(&table, row)?;   // one log entry EACH
        }
    }
    Ok(())
}
```

Each buffered row becomes an independent log entry with no transactional
framing. Two failure modes:

1. **Crash mid-commit:** the log ends up containing a *prefix* of the
   transaction's rows. On restart, replay resurrects that prefix — the
   database now permanently shows half a transaction. This breaks
   atomicity (the A in ACID), the one property clients rely on most.
2. **I/O error mid-commit (no crash needed):** `insert_row` applies to
   memory and then appends to the log; if the append for row *k* fails,
   rows 1..k are already applied *and visible to other connections*, the
   client gets an error, and rows k+1.. never happen. A failed `COMMIT`
   must leave *nothing* applied.

The same shape affects a plain multi-row `INSERT` (`execute_insert` calls
`insert_row` once per row): a crash mid-statement persists some rows of a
statement the client never got an OK for. Statement-level atomicity is the
same best practice at smaller granularity.

**Best practice:** a commit is **one atomic log record** (or a
begin/commit-marker pair, with replay discarding unterminated
transactions). InnoDB writes an MLOG-style group with a commit LSN;
SQLite's WAL commits by writing a commit frame; either pattern works here.

**Fix:** add a `TAG_TX` log entry that carries *all* rows of a
transaction/multi-row statement in one framed, checksummed record. Replay
applies it all-or-nothing (a torn/invalid `TAG_TX` record at the tail is
discarded whole — see D4). `Transaction::commit` and multi-row
`execute_insert` route through it. Single-row autocommit inserts keep the
existing entry shape (they are already atomic at record granularity).

### D3 (High): WAL ordering is inverted — memory first, log second

**Where:** `src/storage/engine.rs:234-251`

```rust
fn insert_row(&self, table: &str, row: Vec<Value>) -> Result<()> {
    {
        let mut tables = self.tables.write()...;
        t.insert_checked(row.clone())?;     // 1. visible to every reader NOW
    }
    self.append_log(|log| log.append_insert_row(table, &row))  // 2. durable (maybe) later
}
```

"Write-*ahead* log" means the log entry is durable **before** the change is
applied where readers can see it. Here it's the reverse, which produces two
concrete bugs:

- **Phantom row on log failure:** if `append_log` fails (disk full,
  permission, I/O error), the client receives an ERR — but the row was
  already inserted into the shared in-memory table and *stays visible to
  every connection* until the next restart, after which it silently
  vanishes. A row that "was never inserted" is readable for hours.
- **Order divergence:** the memory apply (under the `tables` write lock)
  and the log append (under the separate `log` mutex) are not one critical
  section, so two concurrent inserts can hit memory as A,B but the log as
  B,A. Harmless today because inserts commute (append-only, no
  UPDATE/DELETE) — a landmine the moment they don't.

**Fix:** log first, then apply to memory; on log error, apply nothing and
return the error. Combined with D2's batch record this becomes: encode
record → append+fsync → apply to memory under the write lock. (Encode
*before* taking the lock; the memory apply is then infallible, so no undo
path is needed.) This also removes the `row.clone()` — encode borrows the
row, then the row moves into the table.

### D4 (High): a torn log tail prevents startup — crash recovery punishes the crash

**Where:** `src/storage/log.rs:253-276`; behavior pinned by the test
`rejects_truncated_entry_not_panicking` and
`tests/persistence.rs::reopening_a_corrupt_data_file_errors_instead_of_panicking`

`Log::open` errors on any truncated entry. But **a torn final record is the
normal, expected artifact of a crash** — the process died mid-`write_all`,
or the kernel wrote back a partial page. Refusing to start on it means the
server is *unbootable precisely when it just crashed*, requiring manual
surgery on the data file. That converts a durability event into an
availability incident.

**Best practice** (universal — Postgres, InnoDB, RocksDB, SQLite): scan
forward; a record that fails its length/checksum check **at the tail** ends
replay — truncate it away, warn in the log, open for business. Corruption
*followed by more valid data* (mid-file damage) is the un-continuable case
and should still refuse with a clear message.

**Fix:** with D5's per-record CRC in place, `Log::open` recovers by
truncating an invalid tail record (log a warning with the byte offset) and
only errors on mid-file corruption. The existing corrupt-file test splits
into two: torn-tail → recovers minus the last record; mid-file damage →
refuses.

### D5 (High): no record checksums

**Where:** `src/storage/log.rs` framing (`[u32 len][payload]`)

Detection of corruption currently depends on the payload happening to break
the parser. A bit flip inside a `Varchar`'s bytes parses fine and serves
wrong data forever; a partial page writeback that happens to leave a valid
length prefix replays garbage. Checksums are also the mechanism D4 needs to
distinguish "torn tail" from "valid record".

**Fix:** frame as `[u32 len][u32 crc32][payload]`, verify at replay.
CRC32 is the standard (fast, catches burst errors); a small hand-rolled
table-driven CRC32 fits the project's dependency discipline (or justify the
tiny `crc32fast` crate in `Cargo.toml`, same as `tokio` was). Bump the log
format version — the project has explicitly reserved the right to break the
format ("clean version bump, no back-compat concern", PROGRESS.md).

### D6 (Medium): no checkpoint/compaction; replay loads the whole file into RAM

**Where:** `src/storage/log.rs:254` (`std::fs::read(path)`), plus the
append-only design

Two compounding costs: the log grows without bound for the server's entire
life (there is no DELETE yet, but every INSERT since t=0 is replayed on
every start), and `Log::open` reads the **entire** file into one `Vec<u8>`
before replaying — startup memory is O(total history), twice (bytes +
built tables).

**Fix, in two steps:**
1. Streaming replay (`BufReader`, read records incrementally) — removes the
   2× memory spike, trivial.
2. Checkpointing: at startup-complete (or on demand / size threshold),
   write a compact snapshot (`CREATE TABLE` + current rows per table) to
   `data.log.new`, fsync it, atomically `rename` over `data.log`, fsync the
   directory. Replay time becomes O(live data), not O(history). The
   atomic-rename-then-dir-fsync dance is the standard crash-safe pattern.

### D7 (Medium): parent directory is never fsynced after file creation

**Where:** `src/storage/log.rs:274`, `src/storage/engine.rs:175-178`

`OpenOptions::create(true)` makes the file durable in *content* once D1
lands, but the **directory entry** referencing it is its own inode: on
power loss shortly after first boot, the file itself can vanish even though
its blocks were synced. Same applies to `create_dir_all` and to D6's
rename-based checkpointing.

**Fix:** after creating (or renaming) the data file, open the parent
directory and `sync_all()` it (Unix; on Windows this is a no-op —
`cfg`-gate it). A dozen lines, standard in every storage engine.

### D8 (Medium): volatile-by-default, silently; database namespace never persisted

**Where:** `src/config.rs` (`data_dir: None` default), `src/storage/engine.rs:122-128`

A *database server* whose default configuration silently keeps everything
in RAM surprises operators in the worst possible way. The project already
hit this in practice: the dev server ran a whole session in-memory-only
before anyone noticed (PROGRESS.md, 2026-07-12 DATE/DECIMAL entry).
Separately, `CREATE DATABASE` registrations live only in a `RwLock<HashSet>`
— documented, but it means `SHOW DATABASES` lies after a restart.

**Fix:** keep `None` as the default if desired (tests depend on it; embedded
use is legitimate) but **log a prominent startup warning** ("running
without persistence — data will not survive a restart; set
MYSQLRUST_DATA_DIR"). Persist database names as a new log record type.
Document the posture in README's config table.

---

## Part 2 — Performance findings

### P1 (High): every non-indexed query deep-copies the entire table

**Where:** `src/storage/engine.rs:253-259`; consumed by
`src/query/executor.rs::scan_and_filter` and the JOIN path

```rust
fn scan(&self, table: &str) -> Result<Vec<Vec<Value>>> {
    ...
    tables.get(table).map(|t| t.rows.clone())   // deep copy: every row, every String
    ...
}
```

`SELECT ... WHERE non_pk = x LIMIT 5` on a 1M-row table clones one million
`Vec<Value>` (each `Varchar` a fresh heap `String`), filters, keeps 5, drops
the rest. The clone happens **while holding the table read lock**, so it
also lengthens reader/writer contention windows. This is the single biggest
algorithmic waste in the query path and it taxes *every* SELECT, JOIN, and
aggregate.

**Fix (ordered by leverage):**
1. **Predicate pushdown:** extend `Storage` with
   `scan_filtered(&self, table, filter: &mut dyn FnMut(&[Value]) -> bool) -> Result<Vec<Vec<Value>>>`
   — the executor already reduces WHERE to a single typed comparison
   (`compare_values(&row[idx], op, &expected)`), which moves verbatim into
   the callback. Only matching rows are ever cloned. `Transaction` overlays
   its pending rows through the same shape.
2. **Cheap rows:** store rows as `Arc<[Value]>` — cloning a row becomes a
   refcount bump; WHERE-misses never copy row contents at all. Pairs well
   with (1); QueryResult/projection then copy only what they emit.
3. (Later, only if profiling demands) columnar or paged layouts — explicitly
   out of scope for now, matching ARCHITECTURE.md's "not a page/B-tree
   engine" stance.

### P2 (High): result sets are triple-buffered, then written one syscall per packet

**Where:** `src/server/connection.rs:403-422` (build + write loop),
`connection.rs:650-656` (`write_packet` = `write_all` + `flush` per packet),
`protocol/resultset.rs::to_text_packets` (materializes `Vec<Packet>`),
`protocol/packet.rs::encode` (fresh `Vec` per packet)

The pipeline for one SELECT response: `QueryResult` rows → converted into a
second full copy (`Cell` rows) → encoded into a third copy (a `Vec<Packet>`,
one heap `Vec` per column-def/row/terminator packet) → then **each** packet
is `write_all`'d and `flush`'d individually. A 1 000-row result performs
~1 004 write syscalls and ~1 004 flushes (on TLS, also ~1 004 record
encryptions), with peak memory ≈ 3× the result set.

**Best practice:** encode the entire response into one reusable output
buffer and hand it to the socket in one `write_all` + one `flush`; real
servers additionally stream (encode-and-send in chunks) so a huge result
set never fully materializes.

**Fix, staged:**
1. Give `Connection` a persistent `out_buf: Vec<u8>`; add
   `Packet::encode_into(&self, buf: &mut Vec<u8>)` and a
   `ResultSet::encode_into(...)`; `send_result` does one write + one flush.
   (Also collapses the OK/ERR paths' per-packet flushes for free.)
2. Fold the `Cell` conversion away — encode straight from `Value` rows.
3. (Later) chunked streaming with a high-water mark (e.g. flush every 64
   KiB) so `SELECT *` on a huge table is O(chunk) memory, which also
   unblocks dropping the full-materialization in `QueryResult` itself.

### P3 (High): blocking disk I/O runs on the async runtime's worker threads

**Where:** `src/storage/engine.rs::append_log` (sync `write_all` under a
`std::sync::Mutex`), called from async code via the sync `Storage` trait
(`src/server/connection.rs::execute_statement`)

Every persistent INSERT/COMMIT performs blocking file I/O directly on a
tokio worker. Today that's a page-cache memcpy (fast, usually invisible);
the moment D1 adds fsync it becomes **milliseconds of a runtime thread
stalled per write**, during which that worker can't poll *any* other
connection — under load, p99 latency for pure-read connections degrades
because unrelated writers pinned the workers. ARCHITECTURE.md's "never held
across an .await" claim is true for the locks, but a multi-ms blocking
syscall under a mutex on a runtime thread has the same practical effect.

**Fix:** move log appends to a **dedicated writer thread** owning the
`File`, fed by a bounded `std::sync::mpsc`/`tokio` channel of encoded
records; callers `await` a oneshot ack that resolves after write(+fsync per
policy). This is deliberately *not* `spawn_blocking`-per-write: a single
owner thread is what makes group commit (P4) natural — it drains whatever
accumulated in the channel, writes it as one buffer, fsyncs **once**, and
acks the whole batch. One design solves P3, P4, and amortizes D1's cost.

### P4 (Medium): global log mutex, one fsync per writer — no group commit

**Where:** `src/storage/engine.rs:114` (`log: Mutex<Option<Log>>`)

All writers across all tables serialize on one mutex; with fsync-per-commit
(D1) each queued writer pays a *separate* fsync — 200 concurrent 1-row
commits = 200 fsyncs ≈ multiple seconds, when one batched fsync would ack
all 200 in ~1 fsync. Group commit is the standard mitigation everywhere
(InnoDB, Postgres, etcd). **Fix:** falls directly out of P3's writer-thread
design; acceptance is a concurrency benchmark showing commit throughput
scaling with concurrent writers instead of flat-lining at 1/fsync-latency.

### P5 (Medium): `TCP_NODELAY` is never set

**Where:** `src/server/mod.rs:109-114` (accept loop)

MySQL itself sets `TCP_NODELAY` on every TCP session. Without it, Nagle's
algorithm can hold the *second* small write of a response until the
client's delayed ACK (~40 ms spikes) — and the current write path (P2)
emits many small writes per response, maximizing exposure. **Fix:**
`stream.set_nodelay(true)` right after `accept()` (ignore the error —
best-effort), before handing to `Connection`. One line; do it together with
P2 since single-buffer writes are what make latency consistently good.

### P6 (Medium): full schema clone per statement

**Where:** `src/storage/engine.rs:223-232` (`table_schema` clones every
`ColumnSchema`, i.e. every column-name `String`), called at least once per
statement by the executor, twice+ per JOIN, plus again inside
`Transaction::insert_row`.

**Fix:** store `Arc<TableSchema>` (or `Arc<[ColumnSchema]>`) in `Table`;
`table_schema` returns a clone of the `Arc`. Executor signatures move from
`&TableSchema` to the `Arc` transparently. Small, mechanical, removes a
per-query allocation storm on wide tables.

### P7 (Medium): sort-path allocations; ORDER BY + LIMIT sorts everything

**Where:** `src/query/executor.rs::value_ordering` (mixed-type fallback
allocates two `to_display_string()` Strings **per comparison** — O(n log n)
allocations in a worst-case sort) and `execute_projected` (full sort even
when `LIMIT k` needs only the top k).

**Fix:** (a) make the fallback allocation-free (compare via a
`Display`-style writer into stack buffers, or pre-compute sort keys once
per row — decorate/sort/undecorate); (b) when `limit + offset` is small
relative to row count, use a bounded binary heap (top-N) instead of sorting
everything. Both are contained inside the executor; (b) should be
benchmark-gated (Phase 0) to prove it matters before adding code.

### P8 (Low): read buffer never shrinks; `drain` memmoves per packet

**Where:** `src/server/connection.rs:658-685`

One near-`max_allowed_packet` (default 64 MiB) packet permanently pins a
64 MiB `read_buf` for the connection's lifetime; `read_buf.drain(..n)`
memmoves the pipelined remainder on every packet. Fine at current scale.
**Fix (when touched next):** shrink the buffer back to a small cap when
both empty and oversized; use a read cursor + periodic compaction instead
of per-packet `drain`.

### P9 (Low, robustness): idle connections are never reaped

**Where:** `src/server/connection.rs::read_packet` (waits forever);
`wait_timeout`/`interactive_timeout` are *reported* to clients
(`executor.rs::known_variables`) but never enforced.

Each idle client pins a connection permit (`max_connections`), its buffers,
and a task, indefinitely — slow-loris-adjacent resource exhaustion, and a
lie in the advertised variable. **Fix:** wrap command-loop reads in
`tokio::time::timeout(wait_timeout)`; on expiry, close like MySQL does
(client sees "server has gone away"). Handshake/auth reads deserve a much
shorter timeout (e.g. 10s, MySQL's `connect_timeout`).

### P10 (Low): no release profile, no benchmarks

**Where:** `Cargo.toml` (no `[profile.release]` section); no `benches/`;
nothing in CI measures speed.

**Fix:** `[profile.release] lto = "thin", codegen-units = 1` (measure
before/after; these are the standard free wins). More important: the
benchmark harness in Phase 0 below — optimization checkboxes in this plan
are only checkable against measured numbers, per the project's own
"proof it works" discipline.

---

## What is already done right (kept, not rebuilt)

- **PK hash index with a real point-lookup fast path** (`WHERE pk = x`
  skips the scan) — and the JOIN work preserved it.
- **Hash join**, not a nested-loop scan, for the JOIN path.
- **Per-table async write locks** with a timeout (no global write
  serialization at the logical level; lost-update prevention tested).
- **Locks never held across `.await`**; poison-recovering lock usage.
- **`max_allowed_packet` enforced from the 4-byte header**, before
  buffering — allocation-bomb resistant.
- **Panic-free client-reachable paths**, backed by a ~240k-input fuzz
  harness; checked arithmetic in DECIMAL/aggregates.
- **Read-your-own-writes transaction overlay** with documented
  read-committed semantics.
- **Persistent read buffer** across packets (no per-read allocation churn),
  O(1) auto-increment, replay rebuilds the PK index correctly.
- Honest documentation of every one of these gaps in module docs — this
  plan sharpens "known simplification" into "scheduled work item".

---

## Remediation plan

Sequenced so that (1) measurement exists before optimization, (2)
correctness (durability) lands before performance, (3) each task has a
mechanical acceptance check. Follow the PROGRESS.md loop protocol: one
task, implement fully, verify (`fmt`/`clippy`/`build`/`test` + the task's
acceptance), check the box, log it, continue.

Suggested placement: **Phase 12** in ROADMAP.md, after Phase 11's
`ALTER TABLE` completes. D-phase items should also tighten
PRODUCTION_READINESS.md §4, whose current wording ("data persists across a
full server restart") only covers *graceful* restarts.

### Phase PD-0 — Measurement first (prerequisite for everything below) — ✅ done 2026-07-12

- [x] **Crash-safety harness** (`tests/crash.rs`): spawns the real compiled
      `mysql-rust` binary as its own OS process (`env!("CARGO_BIN_EXE_...")`,
      not the in-process `TestServer` every other integration test uses —
      that would kill the test binary too) against a temp data dir, drives
      it with the real `mysql_async` driver, `SIGKILL`s it (`Child::kill`)
      at a swept range of delays after firing a large multi-row `INSERT` or
      a multi-statement `COMMIT` (without waiting for the ack — `tokio::
      time::timeout` races the query against the delay, then kills
      unconditionally), restarts against the same dir, and asserts the row
      count is `0` or `N`, never in between. Two more tests manipulate the
      on-disk log file directly (byte-exact truncation/corruption, no
      subprocess needed) to check torn-tail recovery and mid-file-damage
      refusal. Deviated from the plan's original sketch in one way: no
      separate "helper binary" was needed — the real `mysql-rust` binary
      built by `cargo build` already reads `MYSQLRUST_*` env vars
      (`Config::from_env`), so spawning it directly with a redirected
      `MYSQLRUST_DATA_DIR`/`MYSQLRUST_LISTEN_ADDR` was sufficient.
      **Verified the harness is real, not a rubber stamp**: run with
      `--ignored`, all three currently fail with concrete evidence —
      17/1000 rows survived a killed multi-row INSERT, 361/500 survived a
      killed COMMIT, and a 30-byte truncation produces `"corrupt data
      file: truncated u32"` instead of recovering. 2 non-ignored tests
      (process-crash-persists-acknowledged-write; mid-file-corruption-with-
      valid-data-after-it-is-still-refused) pass today and guard against
      regressing what's already correct.
      _Acceptance met: harness runs in CI (2 pass, 3 correctly `#[ignore]`d
      with a reason naming the D-task that un-ignores them); confirmed via
      `cargo test --test crash -- --ignored` that each ignored assertion
      currently fails for the documented reason, not by accident._
- [x] **Benchmark baseline** (`benches/mysql_bench.rs`, `cargo bench`):
      point-SELECT via PK, full-scan WHERE SELECT at 20k rows, 1k-row
      result-set fetch, single-row autocommit INSERT (volatile +
      persistent), 200-concurrent BEGIN+INSERT+COMMIT throughput, JOIN +
      GROUP BY report query. **Deviated from the plan's original wording**
      ("criterion for micro"): used a hand-rolled ~50-line timer/percentile
      harness instead (`[[bench]] harness = false`, no nightly needed) —
      criterion's dependency tree (`plotters`/`clap`/`rayon`/`serde`/
      `regex`/...) is disproportionate to "time N iterations and print a
      table," and this project adds a dependency only when std genuinely
      can't do the job (every existing dependency in Cargo.toml carries
      that same written justification). Same boot-a-real-`Server`-in-
      process-then-drive-it-with-`mysql_async` pattern as `e2e/main.rs`,
      timed instead of pass/fail-checked.
      _Acceptance met: `cargo bench` runs the whole suite and prints both a
      terminal table and a ready-to-paste markdown table; baseline recorded
      below. The numbers already confirm finding P1 empirically before any
      fix: the full-scan WHERE SELECT (1.89ms median, ~200 of 20,000 rows
      matching) is ~25x slower than the point SELECT (73.6µs median)
      despite returning a similar row count, exactly because `scan()`
      clones all 20,000 rows before the WHERE filter drops most of them._

### Phase PD-1 — Durability core (fixes D1–D5, D7)

Order matters: framing/CRC first (it's the format break), then recovery,
then ordering/atomicity, then fsync policy — each step keeps all tests
green.

- [x] **CRC-checked record framing (D5)** + **Torn-tail recovery (D4)** —
      ✅ done 2026-07-12, implemented together (deliberately, not a scope
      slip): a torn tail can only be told apart from mid-file corruption
      *using* a checksum, so doing D4 without D5 first isn't meaningfully
      possible — `tests/crash.rs`'s own `#[ignore]` reason already said as
      much ("needs D5's checksums to detect a torn record").
      New framing: `[len: u32][crc: u32][payload: len bytes]`, where `crc`
      covers **`len`'s own bytes together with the payload**, not the
      payload alone. That one design choice is what makes recovery safe:
      if only the payload were checksummed, a corrupted length field could
      make mid-file damage look exactly like a torn tail (the claimed
      payload would run past the true end of file, or land on a plausible
      but wrong boundary), silently discarding real subsequent records.
      With the length field itself inside the checksummed region, that
      failure mode reliably surfaces as a checksum mismatch instead.
      Hand-rolled CRC-32 (IEEE 802.3/zlib/gzip/PNG variant) — no
      dependency, matching `auth::sha1`/`auth::sha256`'s precedent — a
      plain bit-by-bit implementation (not table-driven: record sizes here
      never approach where that would matter, and the simpler form is
      easier to verify by inspection), tested against the standard
      `"123456789"` → `0xCBF43926` check value plus empty-input and
      bit-flip-detection cases. No format-version byte: matches this
      project's established precedent (every prior on-disk format change —
      DECIMAL/DATE, per-column flags — was a clean break, no migration
      path, "no back-compat concern for this project's data").
      `Log::open`'s replay loop (`read_record`) now returns one of three
      outcomes per record: a verified payload, `TornTail` (an incomplete
      header, a length running past EOF, or a checksum failure on what's
      positioned as the file's last record — all three are exactly what an
      interrupted write looks like, since a crash can only ever damage the
      *end* of a file), or `Corrupt` (checksum failure with more
      structured-looking data still following — impossible to explain by a
      crash, so refused). On a `TornTail`, the file is physically
      truncated to the last good record via `File::set_len` — not just
      skipped in memory — so a subsequent append lands right after the
      good data instead of after orphaned garbage (which would otherwise
      turn a torn tail from *this* crash into apparent mid-file corruption
      after the *next* one).
      Proof: `tests/crash.rs`'s `torn_log_tail_recovers_by_discarding_the
      _incomplete_final_record` — one of PD-0's own `#[ignore]`d
      assertions — is now **un-ignored and green** at every byte-exact
      truncation offset within the final record, exactly per its own
      acceptance criterion; `mid_file_corruption_with_valid_data_after_it
      _is_still_refused` continues to pass. Two existing tests whose
      expectations were superseded by the new (better) behavior were
      updated, not just made to pass: `storage::log`'s
      `rejects_truncated_entry_not_panicking` split into
      `recovers_from_a_torn_trailing_record_by_truncating_it_away` (sweeps
      every truncation offset, also asserts the on-disk file is physically
      truncated) and `a_file_too_short_for_even_one_record_header_recovers
      _as_empty`, plus a new `mid_file_checksum_mismatch_with_valid_data
      _after_it_is_refused`; `tests/persistence.rs`'s
      `reopening_a_corrupt_data_file_errors_instead_of_panicking` (which
      asserted the *old*, now-incorrect expectation for a torn header)
      split into `reopening_a_file_with_a_torn_trailing_record_recovers
      _without_panicking` and `reopening_a_file_with_mid_file_corruption
      _errors_instead_of_panicking`. 406 tests total (was 399); fmt +
      clippy `-D warnings` clean throughout.
- [x] **True WAL ordering (D3)** — ✅ done 2026-07-12. `InMemoryStorage::
      insert_row` reordered from validate-and-apply-then-log to
      validate → log (durable) → apply; `create_table` the same. If the
      log append fails, nothing has mutated any state a reader could
      observe, so the failed row/table simply never happened — no phantom
      state, no undo needed. `Table::insert_checked` (fused check-and-
      mutate) split into `check_insertable` (read-only) and the existing
      `push_trusted` (now infallible, called only after a successful log
      append) so the two phases can straddle the log I/O. Also removes the
      `row.clone()`/`columns.clone()` the old ordering needed (the log call
      used to need its own copy because the *original* had already been
      moved into the memory apply first) — the plan's stated bonus, landed
      as a natural side effect of the reorder, not a separate change.
      `insert_row` validates under only a **read** lock and applies under
      a **separate** write lock afterward (not one lock held across the
      log I/O) — safe because the caller already holds this table's
      exclusive lock for the whole statement (`InMemoryStorage::
      lock_table`, acquired in `server::connection::execute_statement`
      before the executor ever runs), so no concurrent writer to the same
      table can appear between the two. `create_table` has no equivalent
      per-name lock to lean on (there's no table yet to lock by name until
      it exists), so it keeps its existence-check, log-append, and memory-
      apply under one held write lock — a deliberate, documented asymmetry,
      accepted because schema changes are rare relative to row inserts.
      **Deviated from the plan's suggested acceptance** ("a `Log` wrapper
      that errors on demand"): a genuine OS-level write failure turned out
      not to be reliably triggerable on an *already-open* file handle
      without new platform-specific machinery (permission changes after
      `open()` don't affect an already-open fd on Unix; forcing an fd
      closed out from under a safe `File` isn't possible in safe std) —
      rather than add a dependency (`libc`/`rustix`) for one test, added a
      minimal `#[cfg(test)]`-only fault-injection seam
      (`InMemoryStorage::fail_next_log_write`, an `AtomicBool` field +
      setter) that makes the next `append_log` call fail without touching
      the real log file. Compiles to nothing in a non-test build — zero
      production cost or API surface, private to the crate (not `pub`, so
      no external test can reach it either). Proof: two new tests —
      `failed_log_append_on_insert_leaves_no_trace_in_memory` (also
      confirms a retry with the same value succeeds cleanly, proving no
      phantom PK entry survives a failed attempt) and `failed_log_append_
      on_create_table_leaves_it_absent`. 329 lib tests (408 total, was
      406); fmt + clippy `-D warnings` clean; `tests/transactions.rs` and
      `tests/concurrency.rs` (8 + 5 tests) still green unchanged, since
      `Transaction::commit` routes through this same, now-fixed
      `insert_row` for each buffered row.
- [ ] **Atomic commit records (D2)**: new `TAG_TX` batch record; used by
      `Transaction::commit` and multi-row INSERT; replay applies
      all-or-nothing.
      _Acceptance: PD-0 partial-transaction assertions un-ignored and
      green under randomized mid-commit SIGKILL._
- [ ] **fsync with policy (D1)** + **directory fsync (D7)**:
      `Config::sync_policy` (`always` default / `every_second` / `never`,
      env `MYSQLRUST_SYNC_POLICY`), `sync_data` per policy in the append
      path, parent-dir `sync_all` after create/rename (cfg-gated for
      Unix). Document in README config table.
      _Acceptance: PD-0 harness green with `always`; benchmark records the
      INSERT-latency cost per policy so the trade-off is written down, not
      guessed._

### Phase PD-2 — Write-path architecture (fixes P3, P4; amortizes D1)

- [ ] **Dedicated log-writer thread + group commit**: writer thread owns
      the `File`; bounded channel of encoded records; callers await a
      oneshot ack; the writer drains the queue, writes one buffer, fsyncs
      once per batch, acks the batch. Backpressure via the bounded channel;
      clean shutdown drains the queue.
      _Acceptance: 200-concurrent-commit benchmark shows near-flat total
      wall time vs. 1 writer (group commit working); no runtime worker
      blocks on file I/O (verified by the read-latency-under-write-load
      benchmark not degrading)._

### Phase PD-3 — Query-path performance (fixes P1, P5, P2, P6)

- [ ] **`TCP_NODELAY` (P5)**: set at accept, best-effort.
      _Acceptance: point-SELECT p99 in the macro benchmark; no 40ms
      outliers._
- [ ] **Single-buffer response writes (P2 step 1)**: `encode_into` +
      per-connection reused `out_buf`; one `write_all` + one `flush` per
      response (incl. OK/ERR/auth paths).
      _Acceptance: 1k-row fetch benchmark improves; syscall count per
      query (dtruss/strace spot-check) drops from O(rows) to O(1)._
- [ ] **Predicate pushdown scan (P1 step 1)**: `Storage::scan_filtered`
      with the executor's typed comparison moved into the callback;
      `Transaction` overlays pending rows through the same API.
      _Acceptance: full-scan WHERE benchmark at 100k rows improves
      materially (expect ~order-of-magnitude on low-selectivity filters);
      all 397+ tests green._
- [ ] **`Arc<TableSchema>` (P6)**: schema shared, not cloned, per
      statement.
      _Acceptance: benchmark delta on point-SELECT; mechanical refactor,
      tests green._
- [ ] **(Benchmark-gated) `Arc<[Value]>` rows (P1 step 2)**: row clone =
      refcount bump end-to-end.
      _Acceptance: scan + fetch benchmarks; only merge if the numbers
      justify the churn._

### Phase PD-4 — Operational durability & hygiene (fixes D6, D8, P7–P10)

- [ ] **Streaming replay (D6 step 1)**: `BufReader`-based incremental
      replay; startup memory O(live data).
- [ ] **Checkpoint/compaction (D6 step 2)**: snapshot to `data.log.new` →
      fsync → atomic rename → dir fsync; triggered at startup-complete
      and/or a size threshold; replay prefers the snapshot.
      _Acceptance: crash harness passes when killed mid-checkpoint (old log
      intact); replay time on a churned dataset drops accordingly._
- [ ] **Volatile-mode startup warning + persisted DB namespace (D8)**.
- [ ] **Idle-connection reaping (P9)**: enforce `wait_timeout` on
      command-loop reads and a short handshake timeout.
      _Acceptance: integration test — idle client is disconnected after
      the (test-shortened) timeout and its permit is released._
- [ ] **Sort-path fixes (P7)**: allocation-free comparison fallback;
      top-N heap for ORDER BY + small LIMIT (benchmark-gated).
- [ ] **Buffer shrink policy (P8)**; **release profile (P10)** with
      before/after benchmark numbers recorded here.

### Explicit non-goals (documented so they stay deliberate)

- MVCC / snapshot isolation (read committed stays the documented level).
- On-disk B-tree / page cache / buffer pool — the in-memory + WAL model
  stays; revisit only if data outgrows RAM (per ARCHITECTURE.md).
- Secondary indexes, query planner/optimizer beyond the PK fast path.
- Replication, point-in-time recovery, incremental backup.

---

## Baseline

Recorded 2026-07-12 at commit `8a27711` + this plan's PD-0 work, via
`cargo bench` (`benches/mysql_bench.rs`), release profile, on the machine
this session ran on. n = iteration count per scenario. Re-run and append a
column after each of PD-2/PD-3 lands — same command, same machine if
possible, so the columns are actually comparable.

| Benchmark | n | min | median | mean | p99 | max | After PD-2 | After PD-3 |
|---|---|---|---|---|---|---|---|---|
| point SELECT (PK), 20,000 rows | 2000 | 61.0µs | 73.6µs | 83.3µs | 273.9µs | 300.3µs | | |
| full-scan WHERE SELECT, 20,000 rows (~1% selectivity) | 200 | 1.78ms | 1.89ms | 1.90ms | 2.09ms | 2.09ms | | |
| fetch 1,000-row result set | 200 | 3.95ms | 4.21ms | 4.38ms | 5.43ms | 5.53ms | | |
| single-row autocommit INSERT, volatile (in-memory) | 2000 | 32.3µs | 39.2µs | 40.8µs | 57.2µs | 67.4µs | | |
| single-row autocommit INSERT, persistent (pre-PD-1: `flush()` is a no-op) | 2000 | 33.2µs | 44.7µs | 45.4µs | 61.9µs | 100.4µs | | |
| 200 concurrent BEGIN+INSERT+COMMIT, total wall per burst | 5 | 45.99ms | 46.90ms | 46.84ms | 47.44ms | 47.44ms | | |
| JOIN + GROUP BY report, 500 customers / 2,500 orders | 100 | 786.7µs | 809.1µs | 812.4µs | 928.9µs | 928.9µs | | |

**Reading the baseline:** the full-scan/point-SELECT gap (~25x for a
similarly-sized result) is P1 made visible — confirms the fix is worth
doing, not just theoretically sound. The persistent-vs-volatile INSERT gap
is small today (no fsync yet); expect it to grow substantially once D1
adds `sync_data()` per commit, and expect PD-2's group commit to claw most
of that back under concurrency (watch the 200-concurrent-commits row
specifically — it's the one PD-2 is scored against).
