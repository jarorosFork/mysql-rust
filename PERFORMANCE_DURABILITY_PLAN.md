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
- [x] **Atomic commit records (D2)** — ✅ done 2026-07-12. New
      `TAG_TRANSACTION` record type (`Entry::Transaction { rows: Vec<
      (String, Vec<Value>)> }`) carrying a whole batch of `(table, row)`
      inserts. Atomicity falls out of D4/D5 for free: a batch is exactly
      *one* on-disk record, and `Log::open`'s replay already only ever
      applies a record whose checksum verified fully intact — there's no
      separate "partial batch" state to design for, since the record-level
      guarantee D4/D5 already built *is* the batch-level guarantee here.
      New `Storage::insert_rows(&self, rows: Vec<(String, Vec<Value>)>)`
      trait method (default impl: insert one at a time — exactly right for
      `Transaction`, whose buffered rows aren't durable until `commit()`
      calls this same method on the *real* storage); `InMemoryStorage`
      overrides it to validate the whole batch (including a duplicate
      primary key *within* the batch itself, e.g. `INSERT INTO t VALUES
      (1,'a'),(1,'b')` — not just against already-committed data, which the
      existing per-row check alone can't see) under a read lock, log it as
      one record, then apply every row. `Transaction::commit` now collects
      all its pending rows (across however many tables were touched) into
      one call to `insert_rows` instead of looping `insert_row`.
      `Executor::execute_insert` now coerces *every* row of a multi-row
      `INSERT` first, collecting them into a batch, before calling
      `insert_rows` once — this fixes a sharper bug than the crash case
      alone: previously, row 1 of `INSERT INTO t VALUES (1),(2),('bad')`
      was already applied by the time row 3's type error was discovered,
      so even an ordinary (no crash needed) multi-row INSERT with a bad
      value partway through used to leave a partial result. A statement is
      now genuinely all-or-nothing regardless of *why* it fails.
      Proof — this is the one where the crash harness closes the loop it
      opened in PD-0: `tests/crash.rs`'s
      `crash_mid_multi_row_insert_never_leaves_a_partial_statement` and
      `crash_mid_transaction_commit_never_leaves_a_partial_transaction` —
      the harness's last two `#[ignore]`d assertions — are now **both
      un-ignored and green**, closing out every assertion PD-0 set out to
      eventually prove; the whole file (5 tests, 0 ignored) passes clean.
      5 new unit/integration tests beyond that: duplicate-key-within-batch
      and duplicate-against-committed-data rejection (nothing applies
      either way), a fault-injection test extended to the batch path, a
      cross-table `Transaction::commit` test, and two executor-level tests
      proving the ordinary (non-crash) multi-row-INSERT atomicity fix.
      416 tests total (was 408); fmt + clippy `-D warnings` clean; e2e app
      (41/41) and the benchmark suite still build and run clean.
- [x] **fsync with policy (D1)** + **directory fsync (D7)** — ✅ done
      2026-07-12, closing out PD-1. New `Config::sync_policy: SyncPolicy`
      (env `MYSQLRUST_SYNC_POLICY`), `Log::append` calls `File::sync_data`
      (`fdatasync`, not a full `fsync` — this log's own length change is
      exactly the metadata that needs to hit disk for new bytes to be
      recoverable; other metadata like mtime doesn't) once per append under
      `SyncPolicy::Always`. D7's directory fsync happens once, inside
      `Log::open`, only when the file is being created for the first time
      (a restart's directory entry already exists and was already synced
      when first created) — `#[cfg(unix)]`, a no-op elsewhere.
      **Deviated from the plan's original wording in one way**: shipped
      exactly two policies, `Always` (default) and `Never`, not three.
      `EverySecond` (InnoDB's middle ground — write every commit, fsync
      about once a second) needs a live background task independent of any
      single write, which has no safe home in the *current*, synchronous,
      lock-per-call architecture: `InMemoryStorage::open` is called from
      plain synchronous `#[test]`s throughout this codebase (not just
      async ones), so it can't unconditionally `tokio::spawn` a periodic
      task without panicking outside a runtime. PD-2's dedicated
      log-writer thread is the natural, already-async home for a periodic
      sync — implementing a half-working `EverySecond` now, ahead of that
      architecture, would mean either silently reusing `Always`'s behavior
      under a name that promises something looser (dishonest) or building
      real background-task plumbing that PD-2 will immediately restructure
      anyway (wasted work). `MYSQLRUST_SYNC_POLICY=every_second` is
      rejected with a clear config error rather than silently accepted.
      Also changed 19 call sites of `InMemoryStorage::open`/`open_in_dir`
      to take `sync_policy` as an explicit, required parameter (no hidden
      default) — test code uses `SyncPolicy::Never` throughout (matching
      prior behavior, so the test suite doesn't pay a real fsync cost on
      every insert-heavy test), production (`Server::serve`) threads
      `Config::sync_policy` through.
      Proof: 2 new `Log`-level tests using a `#[cfg(test)]`-only
      `sync_call_count()` seam (the same style as D3's fault-injection
      seam) prove `Always` calls `sync_data()` exactly once per append and
      `Never` calls it zero times — a real behavioral difference, not just
      "the code compiles". 3 new `Config` parsing tests. 421 tests total
      (was 416). The benchmark suite (updated to add `PersistentAlways`/
      `PersistentNever` INSERT variants and to run `concurrent_commits`
      under `Always`, the realistic default) gives the acceptance
      criterion's requested real numbers instead of a guess — see
      "Baseline" below: `sync=always` costs ~100x the per-INSERT latency
      of `sync=never`/volatile (2.8-8ms vs. 30-300µs on this machine), and
      the 200-concurrent-commit burst went from 46.9ms (pre-D1 baseline,
      no fsync at all) to 829ms median (18x) — the exact, now-quantified
      cost PD-2's group commit exists to claw back. **This completes
      PD-1**: all five findings it set out to fix (D1-D5) are done, and
      `tests/crash.rs`'s full crash-safety suite passes with zero
      `#[ignore]`s.

### Phase PD-2 — Write-path architecture (fixes P3, P4; amortizes D1) — ✅ done 2026-07-13

- [x] **Dedicated log-writer thread + group commit**: writer thread owns
      the `File`; bounded channel of encoded records; callers await a
      oneshot ack; the writer drains the queue, writes one buffer, fsyncs
      once per batch, acks the batch. Backpressure via the bounded channel;
      clean shutdown drains the queue.
      _(✅ New `storage::log_writer::LogWriter`: a plain `std::thread`
      owning the already-open `Log`, fed by a bounded `tokio::sync::mpsc`
      channel (`WRITER_QUEUE_CAPACITY` = 4096) of pre-framed records;
      `Log::append`'s framing was split into a pure `frame_record` so the
      *calling* task does the (cheap, CPU-only) encoding and only the
      actual `write`/`fsync` happens on the dedicated thread. The thread
      blocks on `Receiver::blocking_recv()` for the first queued record,
      then drains everything already queued via non-blocking `try_recv()`
      before doing one `write_all` + one conditional `fsync` for the whole
      batch — group commit falls out of "one owner thread" for free, which
      is exactly why this isn't a `spawn_blocking`-per-write pool (that
      wouldn't coordinate at all). Callers get a `tokio::sync::oneshot`
      ack per request; a failed batch acks every waiter in it with its own
      fresh `Error::Io` (`Error` isn't `Clone`). `Drop` takes the sender
      out of an `Option` *before* joining the thread — joining first would
      deadlock, since the thread's receive loop only exits once every
      sender is gone and a struct's own `Drop::drop` runs before its
      fields are auto-dropped.
      Making this real required `Storage::create_table`/`insert_row`/
      `insert_rows` to become genuinely `async` (callers `.await` the
      writer's ack rather than block a tokio worker waiting for it) — since
      `Storage` is used as `&dyn Storage` (`Executor` holds one), native
      `async fn` in traits isn't dyn-compatible, so these three return a
      hand-rolled `Pin<Box<dyn Future<...> + Send>>` (`storage::BoxFuture`)
      instead of pulling in the `async-trait` crate — the same mechanism,
      written out directly, for exactly the handful of methods that need
      it; every read-only method (`scan`, `lookup_by_primary_key`,
      `table_schema`, `next_auto_increment`, `create_database`, ...) stays
      synchronous, since none of them touch the log. `create_table` used to
      hold `tables`' write lock across the whole operation (the one
      pre-PD-2 exception to "never log while holding the lock"); now that
      its log append genuinely awaits the writer thread, holding a
      `std::sync` lock across that await would block every other
      reader/writer for the duration and risk stalling a worker if another
      task's blocking lock call landed mid-suspend — so it now follows
      `insert_row`'s existing check → release → log (no lock held) →
      reacquire → apply shape. The one new edge case that shape introduces:
      two concurrent `CREATE TABLE t` calls for the same never-before-seen
      name can now both pass the first check and both durably log a
      `CreateTable` record; the loser's apply sees the name already taken
      and returns "already exists" instead of silently overwriting the
      winner. Harmless on replay (a redundant `CreateTable` for an
      existing name just re-creates the same empty table), so the cost is
      one wasted log record on a genuinely rare race, never data loss —
      documented in the code rather than engineered around, since building
      real machinery for it would cost more than the race does.)_
      _Acceptance: 200-concurrent-commit benchmark shows near-flat total
      wall time vs. 1 writer (group commit working); no runtime worker
      blocks on file I/O (verified by the read-latency-under-write-load
      benchmark not degrading)._
      _(✅ The existing `concurrent_commits` benchmark (200 concurrent
      `BEGIN`+`INSERT`+`COMMIT` against **one shared table**) turned out to
      be the wrong instrument: `lock_table`'s per-table lock (Phase 7) is
      held for a transaction's whole lifetime, so with everyone contending
      for the same table's lock, only one transaction is ever inside
      `COMMIT` at a time regardless of the log writer — it barely moved
      (829ms → 800ms median) because it was never measuring the log writer
      to begin with, and is kept only as a "hot table" reference point. A
      new `concurrent_inserts_across_many_tables` benchmark (200 concurrent
      autocommit INSERTs, each to its **own** table — no shared lock to
      serialize them, so the log writer is the only thing left to
      serialize on) is the real evidence: 48.12ms median, vs. ~796ms if
      those 200 commits had stayed fully serialized (200 × the 3.98ms
      single-writer persistent-INSERT latency) — proof of real batching. A
      second new benchmark, `read_latency_under_write_load`, measures
      point-SELECT latency on its own connection while 8 other connections
      hammer persistent INSERTs concurrently: 65.8µs median / 251.7µs p99,
      statistically indistinguishable from the uncontended point-SELECT
      baseline (74.0µs / 268.7µs) and nowhere near the ~4ms an inline
      `fsync` blocking a shared worker would cost — proof no worker blocks
      on file I/O. Both new benchmarks and the full before/after table are
      recorded in this file's "Baseline" section. Also proven at the unit
      level, deterministically: `storage::log_writer::tests::
      concurrent_appends_all_land_and_batch_into_fewer_physical_writes`
      fires 200 concurrent appends and asserts the physical write count is
      below 201 via a `#[cfg(test)]` counter exposed by `LogWriter` (the
      same seam style as D1's `Log::sync_call_count`), and `a_failed_batch_
      acks_every_waiter_in_it_with_an_error` proves a faulted batch fails
      every request it covers, not just the first. 425 tests total (was
      421); `tests/crash.rs`'s full 5-test crash-safety suite still passes
      unchanged, proving the write-path rearchitecture didn't regress any
      durability guarantee PD-1 established; e2e app (41/41) still green;
      fmt + clippy `-D warnings` clean throughout.)_

### Phase PD-3 — Query-path performance (fixes P1, P5, P2, P6) — ✅ done 2026-07-13
(one item evaluated and deliberately not merged — see below)

- [x] **`TCP_NODELAY` (P5)**: set at accept, best-effort.
      _(✅ One line in `Server::serve`'s accept loop —
      `stream.set_nodelay(true)`, error ignored — right after `accept()`
      and before the socket is handed to `Connection`, so it covers both
      the plain and TLS paths (TLS wraps the same underlying TCP stream).
      fmt + clippy clean, all tests green.)_
      _Acceptance: point-SELECT p99 in the macro benchmark; no 40ms
      outliers._
      _(✅ p99 stayed in the 130-270µs range across every subsequent
      benchmark run in this phase — no 40ms-scale outlier ever appeared.)_
- [x] **Single-buffer response writes (P2 step 1)**: `encode_into` +
      per-connection reused `out_buf`; one `write_all` + one `flush` per
      response (incl. OK/ERR/auth paths).
      _(✅ `Packet::encode_into` appends header+payload to a caller-supplied
      buffer instead of allocating a fresh `Vec`; `ResultSet::
      encode_text_into`/`encode_binary_into` are the byte-buffer twins of
      `to_text_packets`/`to_binary_packets` (kept, unchanged, as the
      packet-level structural test surface — the new methods don't
      materialize a `Vec<Packet>` at all). `Connection` gained a persistent
      `out_buf: Vec<u8>`; `write_packet` and `send_result`'s multi-row
      branch both clear-and-reuse it, ending in one `flush_out_buf()` call
      (one `write_all` + one `flush`) regardless of how many conceptual
      packets went in. 5 new tests cross-check the new encoders produce
      byte-identical output to the old ones, concatenated.)_
      _Acceptance: 1k-row fetch benchmark improves; syscall count per
      query (dtruss/strace spot-check) drops from O(rows) to O(1)._
      _(✅ `fetch 1000-row result set`: **4.21-4.38ms → ~435-517µs
      median, roughly a 10x drop** — consistent with going from ~1,004
      write+flush syscall pairs down to 1 (verified by design/code
      inspection of the single `flush_out_buf` call site rather than a
      live strace, which needs platform-specific tooling this project
      doesn't otherwise depend on).)_
- [x] **Predicate pushdown scan (P1 step 1)**: `Storage::scan_filtered`
      with the executor's typed comparison moved into the callback;
      `Transaction` overlays pending rows through the same API.
      _(✅ New required trait method (no default — a "scan then filter"
      fallback would compile but silently defeat the whole point for a
      future implementor who forgets to override it).
      `InMemoryStorage::scan_filtered` clones only rows the filter accepts;
      `Transaction::scan_filtered` applies the same filter to both the real
      storage's rows and its own pending overlay. `Executor::
      scan_and_filter`'s non-PK branch routes through it instead of
      `scan()` + an in-memory `.filter()`. The JOIN path's unconditional
      `scan()` calls are deliberately untouched — a `JOIN`'s `WHERE` runs
      on the *combined* rows after the join, so there's no single
      per-table predicate to push down without real cross-join predicate
      analysis, a materially larger feature this task doesn't ask for.
      4 new tests, including one that counts filter invocations
      separately from the returned row count, proving non-matching rows
      are inspected but never cloned.)_
      _Acceptance: full-scan WHERE benchmark at 100k rows improves
      materially (expect ~order-of-magnitude on low-selectivity filters);
      all 397+ tests green._
      _(✅ Bumped the benchmark to the 100k-row scale this acceptance
      names (was 20k): **7.19ms → 1.06ms median (6.8x)**, measured with a
      real before/after at the same row count via `git stash` (not
      estimated by scaling the old 20k-row number). 434 tests green
      throughout (425 after PD-2, +5 from P2's item above, +4 here).)_
- [x] **`Arc<TableSchema>` (P6)**: schema shared, not cloned, per
      statement.
      _(✅ `Table` now holds one `Arc<TableSchema>`; `Storage::
      table_schema`'s return type changed to `Result<Arc<TableSchema>>`.
      Every existing call site (`Executor`, `Transaction`) kept working
      completely unchanged, confirmed by a clean first-try compile —
      `Arc<T>: Deref<Target = T>` means `schema.columns`, `&schema`, etc.
      all still just work. A direct `Arc::ptr_eq`/`strong_count` test
      proves two calls share the same allocation, which is the real
      claim being made here, independent of any benchmark noise.)_
      _Acceptance: benchmark delta on point-SELECT; mechanical refactor,
      tests green._
      _(✅ Modest and honestly reported as such: 48.1µs → 46.5µs median on
      point-SELECT-by-PK. These benchmark tables are narrow (2-3 columns),
      and P6's cost is a clone of every column's name `String` — it scales
      with column count, which the `Arc::ptr_eq` test demonstrates
      structurally regardless of what a 2-column table's benchmark delta
      looks like. `full-scan WHERE SELECT` also dropped further (1.06ms →
      867.6µs) since `scan_and_filter` reads the schema once per query too.
      435 tests total (was 434); e2e app (41/41) and `tests/crash.rs`'s
      5-test crash-safety suite both still green; fmt + clippy `-D
      warnings` clean.)_
- [ ] **(Benchmark-gated) `Arc<[Value]>` rows (P1 step 2)**: row clone =
      refcount bump end-to-end.
      _Acceptance: scan + fetch benchmarks; only merge if the numbers
      justify the churn._
      _(Evaluated 2026-07-13, **deliberately not merged** — the gate the
      plan itself set wasn't met. Reasoning: (1) the scenario this would
      help most — a full, unfiltered `scan()` — is exactly what P2's
      single-buffer write fix already collapsed by ~10x (`fetch_1000_rows`:
      4.2-4.4ms → ~435-517µs), and P1 step 1 already eliminated the
      *dominant* clone cost (a full-table clone on every non-indexed
      `WHERE`) for the filtered case; what's left for `Arc<[Value]>` to
      capture is a much smaller slice than the plan's original estimate
      assumed before either of those landed. (2) The places that would
      benefit most — a bare `scan()` with no `WHERE`, or a `JOIN`'s
      per-table scan — are exactly the places downstream code still has
      to build a *new* row anyway: `hash_join` concatenates two rows into
      a combined one (no way to share that as a slice of either input),
      and result encoding converts every `Value` to a protocol `Cell`
      regardless of the row's container type. `Arc<[Value]>` would only
      remove one intermediate clone in a chain that still allocates at
      every other link, for a change that touches row representation
      everywhere in the query engine (`Table.rows`, `push_trusted`, the
      primary-key index, `Transaction`'s pending buffer, `hash_join`,
      aggregation, `ORDER BY`, projection). (3) No profiling evidence
      points at row-cloning as the next bottleneck now that P1/P2/P6 are
      in — the numbers don't justify the churn, exactly the condition the
      plan's own acceptance line names. Revisit if a future benchmark
      (e.g. very wide rows, or a workload that scans without filtering at
      real scale) shows otherwise.)_

### Phase PD-4 — Operational durability & hygiene (fixes D6, D8, P7–P10)

- [x] **Streaming replay (D6 step 1)**: `BufReader`-based incremental
      replay; startup memory O(live data).
      _(✅ Done 2026-07-13. `Log::open` no longer calls `std::fs::read`
      (whole file into one `Vec<u8>`); it now `stat`s the file for its
      total length (one syscall, not a read) and streams through it with a
      `BufReader`, decoding one record at a time — each record's payload is
      its own short-lived `Vec<u8>`, freed once `decode_entry` consumes it,
      so peak replay memory is O(one record) instead of O(the whole file,
      twice over: raw bytes + built tables). The torn-tail-vs-corruption
      classification (does this record's claimed end land exactly at the
      file's physical length?) is preserved exactly — now checked via `u64`
      arithmetic against a `stat`-provided length instead of a slice-bounds
      check against a preloaded buffer — and guards against a corrupted
      length field ever triggering an oversized allocation attempt by
      checking `payload_end <= file_len` *before* allocating the payload
      buffer, not just before reading it. New required helper
      `read_one_record`/`read_exact_or_eof` replace the old `read_record`/
      `RecordRead` (removed, not kept alongside — nothing else used them).
      Every existing torn-tail/mid-file-corruption test (the byte-exact
      truncation sweep included) passes completely unchanged, proving the
      observable recovery behavior didn't shift at all; one new test
      (`streaming_replay_handles_many_records_in_order`, 5,000 records)
      proves the streaming loop itself is correct at a scale the old
      handful-of-records tests wouldn't have caught an off-by-one at. New
      `restart_replay_time` benchmark (boots a fresh server against an
      already-persisted 50,000-row data directory, times to first
      successful query) gives a real number to score D6 step 2's
      checkpoint/compaction work against later: **23.46ms median**. 436
      tests total (was 435); `tests/crash.rs`'s 5-test crash-safety suite
      and the e2e app (41/41) both still green; fmt + clippy `-D warnings`
      clean.)_
- [x] **Checkpoint/compaction (D6 step 2)**: snapshot to `data.log.new` →
      fsync → atomic rename → dir fsync; triggered at startup-complete
      and/or a size threshold; replay prefers the snapshot.
      _(✅ Done 2026-07-13, with one honest caveat on the acceptance
      criterion's second half — see below. New `Config::
      checkpoint_threshold_bytes` (`MYSQLRUST_CHECKPOINT_THRESHOLD_BYTES`,
      default 16 MiB): if the just-replayed log is at least this many
      bytes, `InMemoryStorage::open` rewrites it as a compact snapshot —
      one `CreateTable` entry plus (if non-empty) one `Transaction` entry
      carrying every current row, per table — before returning. Crash-safe
      via write-to-`data.log.new` (full `sync_all`, unconditionally,
      regardless of the store's own `SyncPolicy` — a checkpoint's safety
      can't depend on a "don't bother syncing" policy) → `std::fs::rename`
      over `data.log` → `sync_parent_dir` (reusing D7's own primitive).
      Deliberately **startup-only, no live/on-demand compaction path**: it
      runs *before* `LogWriter::spawn`, specifically so there is never a
      *running* writer thread whose file handle would need to be hot-swapped
      mid-flight — a meaningfully harder problem than "at startup, if it's
      grown large enough since last time," which is what the plan's own
      "and/or" wording allows choosing. New `Log::open_for_append` (no
      replay — the caller already knows the file's exact content, it just
      wrote it) and `write_snapshot_file` support this without duplicating
      `Log::open`'s replay/recovery logic.
      _Acceptance: crash harness passes when killed mid-checkpoint (old log
      intact); replay time on a churned dataset drops accordingly._
      _(✅/⚠️ First half fully met, second half honestly did not measure
      out as expected. **Crash-safety**: new `tests/crash.rs` test races a
      `SIGKILL` against process startup itself (checkpointing runs before
      the accept loop, so — unlike every other crash test in this file —
      there's no client-observable moment to race against instead) across
      9 swept delays (0-34ms), against a real 500-row pre-built log with a
      threshold of 1 (guaranteed to attempt a checkpoint). Every trial
      restarts to exactly 500 rows, never more, never less, never
      corrupted — proving the crash lands either before the rename (old
      log untouched) or after it (new log complete and correct), never in
      between. Also proven directly at the unit level: `checkpoint_above_
      threshold_compacts_the_log_and_preserves_every_row` (200 rows, real
      compaction, ~24% smaller: 7,533 → 5,746 bytes) and `checkpoint_
      preserves_data_across_multiple_tables_and_auto_increment` (2 tables,
      `AUTO_INCREMENT` continuity survives the rewrite).
      **Replay time**: measured with a dedicated new benchmark
      (`restart_replay_time_after_checkpoint`) against the existing
      50,000-row `restart_replay_time` baseline — **23.05-24.77ms before
      vs. 23.39-25.05ms after, i.e. no measurable improvement, and if
      anything slightly slower within noise.** This is a genuine, reported-
      as-is result, not the "drops accordingly" the acceptance line
      predicted — and there's a real reason for it, not a bug: this
      codebase has no `UPDATE`/`DELETE` yet (confirmed by grep — Phase 11
      hasn't reached them), so "compaction" here cannot shrink the *live
      row count* the way it would once those exist; it only merges many
      small `InsertRow` records into fewer, larger `Transaction` records,
      eliminating one 8-byte record header per row (exactly the ~24%
      file-size win measured above). But replay's actual cost is dominated
      by *per-value and per-row* allocation (`decode_entry`'s `TAG_
      TRANSACTION` branch still allocates one `Vec<Value>` per row and one
      `String` per `Varchar` field, one row at a time, whether that row
      arrived inside a big batched record or its own small one) — record-
      *framing* overhead (the 8-byte header this compaction removes) is a
      small fraction of that. Net: checkpointing is real, correct, and
      already worth having for its actual proven benefits — smaller
      on-disk files, and the infrastructure `UPDATE`/`DELETE` will need to
      get real replay-time wins from compaction later — but the plan's
      replay-time prediction specifically doesn't hold *yet*, and closing
      that gap is a `Value`-representation change (P1 step 2 / P7
      territory, already evaluated and deliberately deferred in PD-3), not
      something this task should reach into. 443 tests total (was 436);
      e2e app (41/41) and `tests/crash.rs`'s now-6-test crash-safety suite
      both green; fmt + clippy `-D warnings` clean.)_
- [x] **Volatile-mode startup warning + persisted DB namespace (D8)**.
      _Acceptance: starting without `data_dir` logs a visible warning;
      `CREATE DATABASE` name survives a restart._
      _(✅ Both halves done. **Startup warning**: `Server::serve` now logs a
      `Warn`-level `volatile_mode` event with a `hint` pointing at
      `MYSQLRUST_DATA_DIR` whenever `config.data_dir` is `None` — verified
      with a manual smoke run (`WARN volatile_mode hint=running without
      persistence -- data will not survive a restart; set
      MYSQLRUST_DATA_DIR to persist it`); `None` stays the default (tests
      and embedded/ephemeral use both depend on it), it's just never silent
      again. **Persisted DB namespace**: two new log-record types
      (`CreateDatabase`/`DropDatabase`, tags 4/5) round-trip through
      `storage::log` exactly like the existing three; `Storage::
      create_database`/`drop_database` became `BoxFuture`-returning (same
      check-under-read-lock → release → log-append-and-`.await` →
      reacquire-write-lock → re-check shape as `create_table`, including the
      same benign harmless-on-replay race) and go through the same
      `LogWriter` group-commit path via two new `append_create_database`/
      `append_drop_database` methods; `checkpoint_if_worthwhile`'s snapshot
      now also re-emits a `CreateDatabase` record for every still-registered
      name, so compaction doesn't drop them. Proven by two new tests:
      `database_names_survive_reopening` (create two, drop one, reopen,
      confirm only the surviving name comes back) and `database_names_
      survive_checkpoint_compaction` (same, but forced through a threshold-0
      checkpoint in between). README's env-var and `Config`-field tables
      now document `MYSQLRUST_DATA_DIR`'s full volatile-mode consequence,
      `MYSQLRUST_SYNC_POLICY`, and `MYSQLRUST_CHECKPOINT_THRESHOLD_BYTES`
      (previously undocumented entirely). 446 tests total (was 443); e2e
      app (41/41, including its own `CREATE`/`DROP DATABASE` + `SHOW
      DATABASES` entries) and `tests/crash.rs`'s 6-test suite both green;
      fmt + clippy `-D warnings` clean.)_
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

### Pre-PD-1 (recorded 2026-07-12 at commit `8a27711`, before D1's `fsync` landed)

Via `cargo bench` (`benches/mysql_bench.rs`), release profile, on the
machine this session ran on. n = iteration count per scenario. Kept as a
historical record — this is what "no `fsync` at all" (the state D1 fixed)
actually cost.

| Benchmark | n | min | median | mean | p99 | max |
|---|---|---|---|---|---|---|
| point SELECT (PK), 20,000 rows | 2000 | 61.0µs | 73.6µs | 83.3µs | 273.9µs | 300.3µs |
| full-scan WHERE SELECT, 20,000 rows (~1% selectivity) | 200 | 1.78ms | 1.89ms | 1.90ms | 2.09ms | 2.09ms |
| fetch 1,000-row result set | 200 | 3.95ms | 4.21ms | 4.38ms | 5.43ms | 5.53ms |
| single-row autocommit INSERT, volatile (in-memory) | 2000 | 32.3µs | 39.2µs | 40.8µs | 57.2µs | 67.4µs |
| single-row autocommit INSERT, persistent (pre-D1: `flush()` was a no-op) | 2000 | 33.2µs | 44.7µs | 45.4µs | 61.9µs | 100.4µs |
| 200 concurrent BEGIN+INSERT+COMMIT, total wall per burst | 5 | 45.99ms | 46.90ms | 46.84ms | 47.44ms | 47.44ms |
| JOIN + GROUP BY report, 500 customers / 2,500 orders | 100 | 786.7µs | 809.1µs | 812.4µs | 928.9µs | 928.9µs |

**Reading it:** the full-scan/point-SELECT gap (~25x for a similarly-sized
result) is P1 made visible — confirms the fix is worth doing, not just
theoretically sound. The persistent-vs-volatile INSERT gap is small here
specifically *because* nothing was actually being forced to disk yet.

### After PD-1 (recorded 2026-07-12 at the D1/D7 commit — same machine, same command)

The `single-row autocommit INSERT` scenario split into three (volatile,
persistent with each `SyncPolicy`) once `sync_policy` existed to vary.

| Benchmark | n | min | median | mean | p99 | max | After PD-2 | After PD-3 |
|---|---|---|---|---|---|---|---|---|
| point SELECT (PK), 20,000 rows | 2000 | 59.7µs | 74.5µs | 82.8µs | 268.7µs | 373.5µs | 74.0µs | 46.5µs |
| full-scan WHERE SELECT, 20,000 rows (~1% selectivity) | 200 | 1.78ms | 1.88ms | 1.90ms | 2.06ms | 2.06ms | 1.98ms | *(scale changed — see below)* |
| fetch 1,000-row result set | 200 | 3.94ms | 4.21ms | 4.38ms | 5.33ms | 5.35ms | 4.27ms | 435.5µs |
| single-row autocommit INSERT, volatile (in-memory) | 2000 | 31.4µs | 38.2µs | 39.1µs | 55.7µs | 63.9µs | 41.8µs | 41.8µs |
| single-row autocommit INSERT, persistent, `sync=always` (default) | 2000 | 2.82ms | 3.98ms | 3.78ms | 4.98ms | 8.15ms | 3.98ms | 3.98ms |
| single-row autocommit INSERT, persistent, `sync=never` | 2000 | 36.6µs | 51.5µs | 58.6µs | 148.4µs | 303.2µs | 56.9µs | 57.9µs |
| 200 concurrent BEGIN+INSERT+COMMIT (same table), total wall per burst (`sync=always`) | 5 | 739.14ms | 828.99ms | 812.24ms | 851.05ms | 851.05ms | 799.65ms | 822.04ms |
| JOIN + GROUP BY report, 500 customers / 2,500 orders | 100 | 826.8µs | 1.04ms | 1.16ms | 3.19ms | 3.19ms | 850.6µs | 760.4µs |

**Full-scan WHERE SELECT was bumped from 20,000 to 100,000 rows during
PD-3** — PD-3's own P1 acceptance criterion names that scale specifically,
and 20k rows was too fast for the fix to show clearly. Measured at 100k
rows with a real before/after (via `git stash`, not by scaling the old
20k-row number): **7.19ms → 1.06ms (predicate pushdown alone) → 867.6µs
(with `Arc<TableSchema>` too) median, a 6.8-8.3x drop.** See PD-3's own
plan entries above for the full reasoning.

**New in PD-3** (200-concurrent-INSERT and read-under-write-load rows are
carried down from the "New in PD-2" table below, remeasured after PD-3):
200 concurrent autocommit INSERTs (distinct tables) 48.12ms → 47.83ms
median (unchanged, as expected — PD-3 doesn't touch the write path);
point SELECT under 8-writer write load 65.8µs → 41.3µs median (tracks the
same predicate-pushdown/`Arc<TableSchema>` read-path wins as the
uncontended point-SELECT row above, not a new write-path effect).

**New in PD-4** (D6 streaming replay + checkpoint/compaction):

| Benchmark | n | min | median | mean | p99 | max |
|---|---|---|---|---|---|---|
| server restart + replay, 50,000 persisted rows | 5 | 22.47-23.05ms | 23.46-24.77ms | 24.56-26.09ms | 26.63-33.29ms | 26.63-33.29ms |
| server restart + replay AFTER checkpoint, 50,000 rows (compacted) | 5 | 23.39ms | 25.05ms | 25.08ms | 26.63ms | 26.63ms |

Ranges on the first row are across the two separate runs recorded for D6
step 1 and step 2 respectively (normal run-to-run variance, not a
regression — see each step's own plan entry above). The second row is
essentially identical to the first, **not** the improvement the D6 step 2
acceptance line predicted; see that entry above for the measured file-size
win (real, ~24%) versus the replay-*time* win (not realized yet, and why).

**New in PD-2** (these scenarios didn't exist before — group commit can't
be measured by a single-writer number, and neither can "did a worker
block"):

| Benchmark | n | min | median | mean | p99 | max |
|---|---|---|---|---|---|---|
| 200 concurrent autocommit INSERTs (**distinct tables**), total wall per burst (`sync=always`) | 5 | 45.97ms | 48.12ms | 51.68ms | 66.72ms | 66.72ms |
| point SELECT (PK) under 8-writer concurrent persistent INSERT load | 500 | 60.3µs | 65.8µs | 71.1µs | 251.7µs | 987.7µs |

**Reading it — the predictions from the pre-PD-1 baseline both landed
exactly as expected, and PD-2's group commit shows up clearly once
measured the right way:**
- **`sync=always` costs ~100x the per-INSERT latency** of `sync=never`/
  volatile (3.98ms vs. 51.5µs/38.2µs median) on this machine. That's a real
  number now, not a guess — an honest trade-off a deployment can weigh,
  which is the entire point of D1 existing as a *policy* rather than a
  hardcoded choice. Unchanged after PD-2, as expected: a genuinely isolated
  single writer still pays its own fsync — group commit only helps when
  there's something to batch *with*.
- **The same-table 200-concurrent-commit row barely moved (828.99ms →
  799.65ms)** — and that's correct, not a sign PD-2 failed. `lock_table`'s
  per-table write lock (Phase 7) is held for a transaction's entire
  lifetime, so with all 200 tasks contending for table `t`'s one lock, only
  one transaction is ever actually inside `COMMIT` — and therefore only one
  request ever reaches the log-writer thread — at a time, regardless of how
  well that thread batches. This row was never actually measuring the log
  writer; it was measuring lock serialization the whole time. Kept as a
  realistic "hot table" reference point, not as PD-2's acceptance evidence.
- **The real acceptance evidence: 200 concurrent autocommit INSERTs to 200
  *distinct* tables — no shared lock to serialize them — dropped to 48.12ms
  median.** Fully serialized (200 × the single-writer 3.98ms persistent
  latency) would be ~796ms; fully-flat (one writer's cost, regardless of
  concurrency) would be ~4ms. 48ms sits far down that range — evidence of
  real batching (many logical appends sharing a handful of physical
  writes+fsyncs), not proof of literally-zero serialization, which no
  single-log-file design can offer. This is the PD-2 acceptance benchmark
  the plan asked for; the same-table row above is why a *different*
  benchmark was needed to see it.
- **No runtime worker blocks on file I/O**: point-SELECT latency measured
  on its own connection while 8 other connections hammer persistent
  (`sync=always`) INSERTs concurrently is statistically indistinguishable
  from the uncontended point-SELECT baseline (65.8µs vs. 74.0µs median;
  251.7µs vs. 268.7µs p99) — nowhere near the ~4ms an inline `fsync`
  blocking a shared worker thread would show up as. The one outlier (987.7µs
  max vs. 373.5µs baseline max) is consistent with ordinary scheduling
  jitter under 8-way contention, not a multi-millisecond fsync stall.
- Read-only scenarios (point SELECT, full-scan, fetch, JOIN+GROUP BY) stay
  within noise of the pre-PD-1/After-PD-1 numbers, as expected — none of
  them touch the log, so a write-path change shouldn't move them, and it
  didn't.
