//! Hand-rolled benchmark suite (PERFORMANCE_DURABILITY_PLAN.md, phase
//! PD-0's "baseline" deliverable). Boots a real `Server` in-process, drives
//! it with the real `mysql_async` driver — same pattern as `e2e/main.rs`,
//! timed instead of pass/fail-checked — across the scenarios the plan's
//! baseline table tracks, and prints a table of min/median/mean/p99/max.
//!
//! Deliberately **not** `criterion`: this project adds a dependency only
//! when std genuinely can't do the job (see CLAUDE.md's "dependencies
//! added intentionally" convention — `tokio`/`tokio-rustls`/`mysql_async`
//! each carry a written rationale in Cargo.toml). "Time N iterations, sort,
//! report percentiles" doesn't need criterion's statistical-analysis
//! machinery or its dependency tree (`plotters`, `clap`, `rayon`, `serde`,
//! `regex`, ...) — a few dozen lines of `std::time::Instant` do the whole
//! job, and `[[bench]] harness = false` gets `cargo bench` (release-profile
//! build, no nightly needed) for free.
//!
//! Run with: `cargo bench` (or `cargo bench --bench mysql_bench`).
//! Re-run after each PD-2/PD-3 task and compare against the table recorded
//! in PERFORMANCE_DURABILITY_PLAN.md's "Baseline" section.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mysql_async::prelude::*;
use mysql_async::{Conn, OptsBuilder};

use mysql_rust::config::{Config, SyncPolicy, UserCredential};
use mysql_rust::observability::LogLevel;
use mysql_rust::server::Server;

const USERNAME: &str = "bench";
const PASSWORD: &str = "s3cret";

fn main() {
    let runtime = tokio::runtime::Runtime::new().expect("build tokio runtime");
    let results = runtime.block_on(run_all());
    print_table(&results);
}

async fn run_all() -> Vec<Stats> {
    vec![
        point_select_by_pk().await,
        full_scan_where_select().await,
        fetch_1000_rows().await,
        single_row_insert(InsertMode::Volatile).await,
        single_row_insert(InsertMode::PersistentAlways).await,
        single_row_insert(InsertMode::PersistentNever).await,
        concurrent_commits().await,
        concurrent_inserts_across_many_tables().await,
        read_latency_under_write_load().await,
        join_group_by_report().await,
    ]
}

// ---------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------

async fn point_select_by_pk() -> Stats {
    const ROWS: usize = 20_000;
    const ITERS: usize = 2_000;

    let addr = start_server(None, SyncPolicy::Never).await;
    let mut conn = connect(addr).await;
    conn.query_drop("CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR)")
        .await
        .expect("create table");
    seed_id_name_rows(&mut conn, ROWS).await;

    let mut samples = Vec::with_capacity(ITERS);
    for i in 0..ITERS {
        let id = (i * 7) % ROWS; // scattered, not sequential
        let start = Instant::now();
        let _: Option<String> = conn
            .query_first(format!("SELECT name FROM t WHERE id = {id}"))
            .await
            .expect("point select");
        samples.push(start.elapsed());
    }
    stats(&format!("point SELECT (PK), {ROWS} rows"), samples)
}

async fn full_scan_where_select() -> Stats {
    const ROWS: usize = 20_000;
    const CATEGORIES: usize = 100; // ~1% selectivity per value
    const ITERS: usize = 200;

    let addr = start_server(None, SyncPolicy::Never).await;
    let mut conn = connect(addr).await;
    conn.query_drop("CREATE TABLE t (id INT PRIMARY KEY, category VARCHAR, name VARCHAR)")
        .await
        .expect("create table");
    let mut sql = String::from("INSERT INTO t VALUES ");
    for i in 0..ROWS {
        if i > 0 {
            sql.push(',');
        }
        sql.push_str(&format!("({i}, 'cat{}', 'row{i}')", i % CATEGORIES));
    }
    conn.query_drop(sql).await.expect("seed rows");

    let mut samples = Vec::with_capacity(ITERS);
    for i in 0..ITERS {
        let cat = i % CATEGORIES;
        let start = Instant::now();
        let _: Vec<(i64, String)> = conn
            .query(format!(
                "SELECT id, name FROM t WHERE category = 'cat{cat}'"
            ))
            .await
            .expect("full scan select");
        samples.push(start.elapsed());
    }
    stats(&format!("full-scan WHERE SELECT, {ROWS} rows"), samples)
}

async fn fetch_1000_rows() -> Stats {
    const ROWS: usize = 1_000;
    const ITERS: usize = 200;

    let addr = start_server(None, SyncPolicy::Never).await;
    let mut conn = connect(addr).await;
    conn.query_drop("CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR)")
        .await
        .expect("create table");
    seed_id_name_rows(&mut conn, ROWS).await;

    let mut samples = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let start = Instant::now();
        let rows: Vec<(i64, String)> = conn.query("SELECT id, name FROM t").await.expect("fetch");
        assert_eq!(rows.len(), ROWS);
        samples.push(start.elapsed());
    }
    stats(&format!("fetch {ROWS}-row result set"), samples)
}

/// PERFORMANCE_DURABILITY_PLAN.md D1's acceptance calls for recording the
/// INSERT-latency cost per `SyncPolicy` rather than guessing it — these
/// three variants are that measurement.
enum InsertMode {
    Volatile,
    /// The default: `fdatasync` after every insert.
    PersistentAlways,
    /// Persisted to disk, but never explicitly synced — the old (pre-D1)
    /// behavior, kept as an explicit opt-in via `MYSQLRUST_SYNC_POLICY=never`.
    PersistentNever,
}

async fn single_row_insert(mode: InsertMode) -> Stats {
    const ITERS: usize = 2_000;

    let (dir, sync_policy, label): (Option<TempDir>, SyncPolicy, &str) = match mode {
        InsertMode::Volatile => (
            None,
            SyncPolicy::Never,
            "single-row autocommit INSERT, volatile (in-memory)",
        ),
        InsertMode::PersistentAlways => (
            Some(temp_data_dir("single-insert-always")),
            SyncPolicy::Always,
            "single-row autocommit INSERT, persistent (sync=always)",
        ),
        InsertMode::PersistentNever => (
            Some(temp_data_dir("single-insert-never")),
            SyncPolicy::Never,
            "single-row autocommit INSERT, persistent (sync=never)",
        ),
    };
    let addr = start_server(dir.as_ref().map(TempDir::path), sync_policy).await;
    let mut conn = connect(addr).await;
    conn.query_drop("CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR)")
        .await
        .expect("create table");

    let mut samples = Vec::with_capacity(ITERS);
    for i in 0..ITERS {
        let start = Instant::now();
        conn.query_drop(format!("INSERT INTO t VALUES ({i}, 'row{i}')"))
            .await
            .expect("insert");
        samples.push(start.elapsed());
    }
    stats(label, samples)
}

/// A realistic "hot table" scenario: many concurrent transactions all
/// writing to the *same* table (e.g. a shared queue/counter). Recorded
/// against PD-1's baseline (829ms median) since D1 first added `fsync`, but
/// **this benchmark cannot show PD-2's group-commit win** — `lock_table`'s
/// per-table lock (Phase 7) is held for a transaction's whole lifetime, so
/// with every task contending for table `t`'s one lock, only one
/// transaction is ever actually inside its `COMMIT` (and therefore only
/// one request ever reaches the log-writer thread) at a time regardless of
/// PD-2. It's kept as a real-world reference point (see
/// `concurrent_inserts_across_many_tables` below for the benchmark that
/// actually isolates the log writer's own concurrency).
async fn concurrent_commits() -> Stats {
    const CONCURRENCY: usize = 200;
    const BURSTS: usize = 5;

    let dir = temp_data_dir("concurrent-commits");
    let addr = start_server(Some(dir.path()), SyncPolicy::Always).await;
    {
        let mut conn = connect(addr).await;
        conn.query_drop("CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR)")
            .await
            .expect("create table");
    }

    let mut samples = Vec::with_capacity(BURSTS);
    for burst in 0..BURSTS {
        let start = Instant::now();
        let mut tasks = Vec::with_capacity(CONCURRENCY);
        for i in 0..CONCURRENCY {
            let id = burst * CONCURRENCY + i;
            tasks.push(tokio::spawn(async move {
                let mut conn = connect(addr).await;
                conn.query_drop("BEGIN").await.expect("begin");
                conn.query_drop(format!("INSERT INTO t VALUES ({id}, 'row{id}')"))
                    .await
                    .expect("insert");
                conn.query_drop("COMMIT").await.expect("commit");
            }));
        }
        for task in tasks {
            task.await.expect("connection task panicked");
        }
        samples.push(start.elapsed());
    }
    stats(
        &format!("{CONCURRENCY} concurrent BEGIN+INSERT+COMMIT (same table), total wall/burst"),
        samples,
    )
}

/// PERFORMANCE_DURABILITY_PLAN.md PD-2's actual acceptance scenario: each of
/// `CONCURRENCY` connections writes to its *own* table, so — unlike
/// `concurrent_commits` above — `lock_table`'s per-table lock never
/// serializes them against each other; the only thing left to serialize on
/// is the log-writer thread itself. Before PD-2, every one of these paid
/// its own separate inline `fsync` (roughly `CONCURRENCY *
/// single-row-insert-latency`, i.e. close to flat-line growth with
/// concurrency); group commit should make this scale far better than that,
/// since the writer thread drains and batches whatever's queued into one
/// `write` + one `fsync` per burst instead of one each.
async fn concurrent_inserts_across_many_tables() -> Stats {
    const CONCURRENCY: usize = 200;
    const BURSTS: usize = 5;

    let dir = temp_data_dir("concurrent-inserts-many-tables");
    let addr = start_server(Some(dir.path()), SyncPolicy::Always).await;
    {
        let mut conn = connect(addr).await;
        for t in 0..CONCURRENCY {
            conn.query_drop(format!(
                "CREATE TABLE t{t} (id INT PRIMARY KEY, name VARCHAR)"
            ))
            .await
            .expect("create table");
        }
    }

    let mut samples = Vec::with_capacity(BURSTS);
    for burst in 0..BURSTS {
        let start = Instant::now();
        let mut tasks = Vec::with_capacity(CONCURRENCY);
        for table in 0..CONCURRENCY {
            let id = burst * CONCURRENCY + table;
            tasks.push(tokio::spawn(async move {
                let mut conn = connect(addr).await;
                conn.query_drop(format!("INSERT INTO t{table} VALUES ({id}, 'row{id}')"))
                    .await
                    .expect("insert");
            }));
        }
        for task in tasks {
            task.await.expect("connection task panicked");
        }
        samples.push(start.elapsed());
    }
    stats(
        &format!("{CONCURRENCY} concurrent autocommit INSERTs (distinct tables), total wall/burst"),
        samples,
    )
}

/// PERFORMANCE_DURABILITY_PLAN.md PD-2's second acceptance clause: "no
/// runtime worker blocks on file I/O (verified by the read-latency-under-
/// write-load benchmark not degrading)". Before PD-2, every INSERT's
/// `fsync` ran inline on whatever tokio worker was running that statement
/// (P3) — a worker stalled for the duration of the syscall couldn't poll
/// *any* other connection meanwhile, so a concurrent point-SELECT sharing
/// that worker would see its own latency spike in lockstep with the
/// writers' fsyncs. With the dedicated log-writer thread, a worker that
/// sent a write off to that thread is immediately free to poll other
/// connections while waiting for the ack — this measures point-SELECT
/// latency on its own connection while several other connections hammer
/// persistent (`sync=always`) INSERTs concurrently, so a regression back to
/// inline blocking would show up here as read p99/max latency tracking the
/// fsync cost instead of staying close to the uncontended point-SELECT
/// baseline above.
async fn read_latency_under_write_load() -> Stats {
    const ROWS: usize = 5_000;
    const ITERS: usize = 500;
    const WRITERS: usize = 8;

    let dir = temp_data_dir("read-under-write-load");
    let addr = start_server(Some(dir.path()), SyncPolicy::Always).await;
    let mut conn = connect(addr).await;
    conn.query_drop("CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR)")
        .await
        .expect("create table");
    seed_id_name_rows(&mut conn, ROWS).await;

    let stop = Arc::new(AtomicBool::new(false));
    let mut writer_tasks = Vec::with_capacity(WRITERS);
    for w in 0..WRITERS {
        let stop = Arc::clone(&stop);
        writer_tasks.push(tokio::spawn(async move {
            let mut conn = connect(addr).await;
            let mut i = ROWS + w * 1_000_000;
            while !stop.load(Ordering::Relaxed) {
                conn.query_drop(format!("INSERT INTO t VALUES ({i}, 'w{i}')"))
                    .await
                    .expect("background writer insert");
                i += 1;
            }
        }));
    }

    let mut samples = Vec::with_capacity(ITERS);
    for i in 0..ITERS {
        let id = (i * 37) % ROWS;
        let start = Instant::now();
        let _: Option<String> = conn
            .query_first(format!("SELECT name FROM t WHERE id = {id}"))
            .await
            .expect("point select under write load");
        samples.push(start.elapsed());
    }

    stop.store(true, Ordering::Relaxed);
    for task in writer_tasks {
        task.await.expect("background writer task panicked");
    }

    stats(
        &format!("point SELECT (PK) under {WRITERS}-writer concurrent persistent INSERT load"),
        samples,
    )
}

async fn join_group_by_report() -> Stats {
    const CUSTOMERS: usize = 500;
    const ORDERS: usize = 2_500;
    const ITERS: usize = 100;

    let addr = start_server(None, SyncPolicy::Never).await;
    let mut conn = connect(addr).await;
    conn.query_drop("CREATE TABLE customers (id INT PRIMARY KEY, name VARCHAR)")
        .await
        .expect("create customers");
    conn.query_drop(
        "CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT, total DECIMAL(10,2))",
    )
    .await
    .expect("create orders");

    let mut sql = String::from("INSERT INTO customers VALUES ");
    for i in 0..CUSTOMERS {
        if i > 0 {
            sql.push(',');
        }
        sql.push_str(&format!("({i}, 'customer{i}')"));
    }
    conn.query_drop(sql).await.expect("seed customers");

    let mut sql = String::from("INSERT INTO orders VALUES ");
    for i in 0..ORDERS {
        if i > 0 {
            sql.push(',');
        }
        let customer_id = i % CUSTOMERS;
        let cents = 1 + (i % 9999);
        sql.push_str(&format!(
            "({i}, {customer_id}, {}.{:02})",
            cents / 100,
            cents % 100
        ));
    }
    conn.query_drop(sql).await.expect("seed orders");

    let query = "SELECT c.name, COUNT(*), SUM(o.total) FROM customers c \
                 JOIN orders o ON c.id = o.customer_id \
                 GROUP BY c.name ORDER BY c.name LIMIT 50";

    let mut samples = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let start = Instant::now();
        let rows: Vec<(String, i64, String)> = conn.query(query).await.expect("join+group by");
        assert!(!rows.is_empty());
        samples.push(start.elapsed());
    }
    stats(
        &format!("JOIN+GROUP BY report, {CUSTOMERS} customers/{ORDERS} orders"),
        samples,
    )
}

// ---------------------------------------------------------------------
// Harness plumbing
// ---------------------------------------------------------------------

async fn seed_id_name_rows(conn: &mut Conn, count: usize) {
    let mut sql = String::from("INSERT INTO t VALUES ");
    for i in 0..count {
        if i > 0 {
            sql.push(',');
        }
        sql.push_str(&format!("({i}, 'row{i}')"));
    }
    conn.query_drop(sql).await.expect("seed rows");
}

/// A directory that deletes itself (and its contents) on drop, so a
/// persistent-mode scenario doesn't leak temp data files across runs.
struct TempDir(PathBuf);

impl TempDir {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn temp_data_dir(name: &str) -> TempDir {
    static COUNTER: std::sync::Mutex<u64> = std::sync::Mutex::new(0);
    let mut counter = COUNTER.lock().unwrap_or_else(|e| e.into_inner());
    *counter += 1;
    let dir = std::env::temp_dir().join(format!(
        "mysql-rust-bench-{name}-{}-{}",
        std::process::id(),
        *counter
    ));
    std::fs::create_dir_all(&dir).expect("create temp data dir");
    TempDir(dir)
}

/// Boot a real `Server` on an ephemeral loopback port, in a background
/// task, with one configured account and (optionally) on-disk persistence.
/// The caller owns and keeps alive whatever `TempDir` backs `data_dir` (see
/// [`TempDir::path`]) for as long as the server needs it. `sync_policy` is
/// irrelevant when `data_dir` is `None` (nothing is ever written to disk).
async fn start_server(data_dir: Option<&Path>, sync_policy: SyncPolicy) -> std::net::SocketAddr {
    let config = Config {
        users: vec![UserCredential::with_caching_sha2_password(
            USERNAME, PASSWORD,
        )],
        log_level: LogLevel::Error,
        data_dir: data_dir.map(Path::to_path_buf),
        sync_policy,
        ..Config::default()
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let server = Server::new(config);
        let shutdown = std::future::pending::<()>();
        if let Err(e) = server.serve(listener, shutdown).await {
            eprintln!("server error: {e}");
        }
    });

    addr
}

async fn connect(addr: std::net::SocketAddr) -> Conn {
    let opts = OptsBuilder::default()
        .ip_or_hostname(addr.ip().to_string())
        .tcp_port(addr.port())
        .user(Some(USERNAME))
        .pass(Some(PASSWORD));
    Conn::new(opts).await.expect("connect")
}

// ---------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------

struct Stats {
    label: String,
    n: usize,
    min: Duration,
    median: Duration,
    mean: Duration,
    p99: Duration,
    max: Duration,
}

fn stats(label: &str, mut samples: Vec<Duration>) -> Stats {
    samples.sort();
    let n = samples.len();
    let sum: Duration = samples.iter().sum();
    Stats {
        label: label.to_string(),
        n,
        min: samples[0],
        median: samples[n / 2],
        mean: sum / n as u32,
        p99: samples[((n * 99) / 100).min(n - 1)],
        max: samples[n - 1],
    }
}

fn fmt_dur(d: Duration) -> String {
    let micros = d.as_secs_f64() * 1_000_000.0;
    if micros >= 1000.0 {
        format!("{:.2}ms", micros / 1000.0)
    } else {
        format!("{micros:.1}µs")
    }
}

fn print_table(results: &[Stats]) {
    println!(
        "\n{:<58} {:>6} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "benchmark", "n", "min", "median", "mean", "p99", "max"
    );
    println!("{}", "-".repeat(58 + 6 + 10 * 5 + 6));
    for s in results {
        println!(
            "{:<58} {:>6} {:>10} {:>10} {:>10} {:>10} {:>10}",
            s.label,
            s.n,
            fmt_dur(s.min),
            fmt_dur(s.median),
            fmt_dur(s.mean),
            fmt_dur(s.p99),
            fmt_dur(s.max),
        );
    }
    println!();
    println!("| Benchmark | n | min | median | mean | p99 | max |");
    println!("|---|---|---|---|---|---|---|");
    for s in results {
        println!(
            "| {} | {} | {} | {} | {} | {} | {} |",
            s.label,
            s.n,
            fmt_dur(s.min),
            fmt_dur(s.median),
            fmt_dur(s.mean),
            fmt_dur(s.p99),
            fmt_dur(s.max),
        );
    }
}
