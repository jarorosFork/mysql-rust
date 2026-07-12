//! Standalone end-to-end app — not a `cargo test`, a real program you run.
//!
//! Boots a real `mysql-rust` `Server` in-process, connects to it with the
//! actual third-party `mysql_async` driver (not this project's own scripted
//! test client), then works through a scripted list of realistic SQL
//! entries one at a time on that single connection — so DDL a later entry
//! depends on (a table, some rows) is genuinely in place, exactly like a
//! real client session. Each entry is reported as it runs; a summary and a
//! non-zero exit code on any failure make this usable as a smoke test, not
//! just a demo.
//!
//! Run with: `cargo run --example e2e`

use mysql_async::prelude::*;
use mysql_async::{Conn, OptsBuilder, Row};

use mysql_rust::config::{Config, UserCredential};
use mysql_rust::observability::LogLevel;
use mysql_rust::server::Server;

/// One scripted step: a human-readable label and the SQL to run. Entries
/// share one connection and run strictly in order, so later entries may
/// depend on earlier ones (a table existing, rows already inserted, an
/// open transaction) the same way a real client session would.
struct Entry {
    name: &'static str,
    sql: &'static str,
}

const USERNAME: &str = "alice";
const PASSWORD: &str = "s3cret";

const ENTRIES: &[Entry] = &[
    Entry {
        name: "create database",
        sql: "CREATE DATABASE IF NOT EXISTS shop",
    },
    Entry {
        name: "create schema (MySQL's synonym for CREATE DATABASE)",
        sql: "CREATE SCHEMA IF NOT EXISTS shop_alt",
    },
    Entry {
        name: "show databases",
        sql: "SHOW DATABASES",
    },
    Entry {
        name: "create table: qualified name, AUTO_INCREMENT, NOT NULL, \
               a table-level PRIMARY KEY constraint, trailing table options",
        sql: "CREATE TABLE shop.customers (\n\
              \tid INT AUTO_INCREMENT NOT NULL,\n\
              \tname VARCHAR(100) NOT NULL,\n\
              \temail VARCHAR(255) NULL,\n\
              \tCONSTRAINT customers_pk PRIMARY KEY (id)\n\
              )\n\
              DEFAULT CHARSET=utf8mb4\n\
              COLLATE=utf8mb4_general_ci",
    },
    Entry {
        name: "insert, omitting the AUTO_INCREMENT column",
        sql: "INSERT INTO customers (name, email) VALUES ('Ada Lovelace', 'ada@example.com')",
    },
    Entry {
        name: "insert, multi-row, one NULL email",
        sql: "INSERT INTO customers (name, email) VALUES \
              ('Grace Hopper', 'grace@example.com'), ('Alan Turing', NULL)",
    },
    Entry {
        name: "select *",
        sql: "SELECT * FROM customers",
    },
    Entry {
        name: "select with a WHERE equality (indexed primary-key lookup)",
        sql: "SELECT name FROM customers WHERE id = 1",
    },
    Entry {
        name: "select with ORDER BY / LIMIT / OFFSET (a GUI grid's sort + paging)",
        sql: "SELECT name FROM customers ORDER BY name DESC LIMIT 2 OFFSET 1",
    },
    Entry {
        name: "show tables",
        sql: "SHOW TABLES",
    },
    Entry {
        name: "show character set",
        sql: "SHOW CHARACTER SET",
    },
    Entry {
        name: "show collation",
        sql: "SHOW COLLATION",
    },
    Entry {
        name: "show variables like",
        sql: "SHOW VARIABLES LIKE 'max_allowed_packet'",
    },
    Entry {
        name: "select @@version",
        sql: "SELECT @@version",
    },
    Entry {
        name: "select database() (NULL: this server is schemaless)",
        sql: "SELECT DATABASE()",
    },
    Entry {
        name: "begin transaction",
        sql: "BEGIN",
    },
    Entry {
        name: "insert inside the open transaction",
        // `email` must be listed explicitly: an explicit column list needs
        // every column except AUTO_INCREMENT ones (no DEFAULT-value model
        // for a merely-nullable, omitted column — pass NULL if that's the
        // intent, as the multi-row insert above does for Alan Turing).
        sql: "INSERT INTO customers (name, email) VALUES ('Margaret Hamilton', NULL)",
    },
    Entry {
        name: "commit",
        sql: "COMMIT",
    },
    Entry {
        name: "verify the committed row is visible",
        sql: "SELECT name FROM customers WHERE name = 'Margaret Hamilton'",
    },
    // ---- Phase 11: DECIMAL / DATE / BOOLEAN ----
    Entry {
        name: "create table with DECIMAL, DATE, and BOOLEAN columns",
        sql: "CREATE TABLE orders (\n\
              \tid INT AUTO_INCREMENT PRIMARY KEY,\n\
              \ttotal DECIMAL(10,2) NOT NULL,\n\
              \tplaced_on DATE NOT NULL,\n\
              \tpaid BOOLEAN NOT NULL\n\
              )",
    },
    Entry {
        name: "insert decimal/date/boolean literals (incl. an int and an \
               over-precise decimal, both normalized to the column's scale)",
        sql: "INSERT INTO orders (total, placed_on, paid) VALUES \
              (19.99, '2024-01-15', TRUE), \
              (5, '2023-12-25', FALSE), \
              (100.005, '2024-06-01', TRUE)",
    },
    Entry {
        name: "select decimal total (exact — no float rounding artifacts)",
        sql: "SELECT total FROM orders ORDER BY id",
    },
    Entry {
        name: "select date, ORDER BY chronologically (not insertion order)",
        sql: "SELECT placed_on FROM orders ORDER BY placed_on",
    },
    Entry {
        name: "select boolean (reads back as plain 0/1)",
        sql: "SELECT paid FROM orders ORDER BY id",
    },
    Entry {
        name: "WHERE on a decimal column compares numerically, not lexically",
        sql: "SELECT total FROM orders WHERE total > 10.00 ORDER BY total",
    },
    // ---- Phase 11: GROUP BY + aggregate functions ----
    Entry {
        name: "create and seed a sales table for GROUP BY / aggregate queries",
        sql: "CREATE TABLE sales (id INT PRIMARY KEY, category VARCHAR, amount DECIMAL(10,2))",
    },
    Entry {
        name: "insert sales rows across two categories",
        sql: "INSERT INTO sales VALUES \
              (1, 'fruit', 10.00), (2, 'fruit', 5.50), \
              (3, 'veg', 3.25), (4, 'veg', 7.75), (5, 'veg', 1.00)",
    },
    Entry {
        name: "plain aggregate (no GROUP BY): total row count",
        sql: "SELECT COUNT(*) FROM sales",
    },
    Entry {
        name: "plain aggregate: grand total (exact fixed-point, not a float)",
        sql: "SELECT SUM(amount) FROM sales",
    },
    Entry {
        name: "GROUP BY: a totals-by-category report, sorted by the \
               aggregate's own alias",
        sql: "SELECT category, COUNT(*) AS n, SUM(amount) AS total \
              FROM sales GROUP BY category ORDER BY total DESC",
    },
    Entry {
        name: "WHERE filters before GROUP BY (not after)",
        sql: "SELECT category, COUNT(*) FROM sales WHERE amount > 4.00 GROUP BY category",
    },
    Entry {
        name: "AVG returns exact fixed-point (not a float approximation)",
        sql: "SELECT AVG(amount) FROM sales",
    },
    Entry {
        name: "MIN and MAX",
        sql: "SELECT MIN(amount), MAX(amount) FROM sales",
    },
    Entry {
        name: "drop schema (cleanup)",
        sql: "DROP SCHEMA IF EXISTS shop_alt",
    },
];

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let addr = start_server().await;
    println!("mysql-rust listening on {addr}\n");

    let opts = OptsBuilder::default()
        .ip_or_hostname(addr.ip().to_string())
        .tcp_port(addr.port())
        .user(Some(USERNAME))
        .pass(Some(PASSWORD));
    let mut conn = match Conn::new(opts).await {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("fatal: could not connect: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    println!("connected as '{USERNAME}' via mysql_async\n");

    let mut failures = 0usize;
    for (i, entry) in ENTRIES.iter().enumerate() {
        print!("[{:>2}/{}] {} ... ", i + 1, ENTRIES.len(), entry.name);
        match run_entry(&mut conn, entry.sql).await {
            Ok(summary) => println!("ok ({summary})"),
            Err(e) => {
                println!("FAILED: {e}");
                failures += 1;
            }
        }
    }

    println!(
        "\n{}/{} entries passed",
        ENTRIES.len() - failures,
        ENTRIES.len()
    );

    conn.disconnect().await.ok();

    if failures == 0 {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

/// Run one entry's SQL and summarize the outcome. Works generically for any
/// statement — DDL/DML (an OK packet, so an empty row set) or a `SELECT`
/// (one or more rows) — since [`Row`] is `mysql_async`'s untyped row type.
async fn run_entry(conn: &mut Conn, sql: &str) -> mysql_async::Result<String> {
    let mut result = conn.query_iter(sql).await?;
    let rows: Vec<Row> = result.collect().await?;
    let affected = result.affected_rows();

    if rows.is_empty() {
        return Ok(format!("{affected} row(s) affected"));
    }

    let preview = rows
        .iter()
        .take(3)
        .map(format_row)
        .collect::<Vec<_>>()
        .join("; ");
    let more = if rows.len() > 3 {
        format!(", +{} more", rows.len() - 3)
    } else {
        String::new()
    };
    Ok(format!("{} row(s): {preview}{more}", rows.len()))
}

fn format_row(row: &Row) -> String {
    let values: Vec<String> = (0..row.len())
        .map(|i| format_value(row.as_ref(i)))
        .collect();
    format!("({})", values.join(", "))
}

/// Render a value the way a person reading a report wants: text as text
/// (not a truncated byte-debug dump), numbers as numbers, `NULL` as `NULL`.
fn format_value(value: Option<&mysql_async::Value>) -> String {
    use mysql_async::Value;
    match value {
        None | Some(Value::NULL) => "NULL".to_string(),
        Some(Value::Bytes(bytes)) => match std::str::from_utf8(bytes) {
            Ok(s) => format!("'{s}'"),
            Err(_) => format!("{bytes:?}"),
        },
        Some(Value::Int(n)) => n.to_string(),
        Some(Value::UInt(n)) => n.to_string(),
        Some(Value::Float(n)) => n.to_string(),
        Some(Value::Double(n)) => n.to_string(),
        Some(other) => format!("{other:?}"),
    }
}

/// Boot a real `Server` on an ephemeral loopback port, in a background task,
/// with one configured account. Returns once it's actually listening.
async fn start_server() -> std::net::SocketAddr {
    let config = Config {
        users: vec![UserCredential::with_caching_sha2_password(
            USERNAME, PASSWORD,
        )],
        log_level: LogLevel::Warn,
        ..Config::default()
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let server = Server::new(config);
        // Never signaled: this process exits (dropping the listener) once
        // `main` returns, which is all the cleanup a short-lived e2e run needs.
        let shutdown = std::future::pending::<()>();
        if let Err(e) = server.serve(listener, shutdown).await {
            eprintln!("server error: {e}");
        }
    });

    addr
}
