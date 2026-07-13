//! End-to-end conformance (Phase 10): a **real, standard third-party MySQL
//! driver** — `mysql_async`, not our own scripted client — connects to the
//! server and runs a realistic workload. This is the strongest wire-
//! compatibility proof available in this environment (no stock `mysql` CLI
//! binary is installed): the driver performs its own capability negotiation,
//! auth, connect-time settings query, text-protocol CRUD, prepared statements
//! over the binary protocol, and transactions — all against our server.

mod common;

use mysql_async::prelude::*;
use mysql_async::{Conn, OptsBuilder};

use mysql_rust::config::{Config, UserCredential};
use mysql_rust::observability::LogLevel;

use common::TestServer;

/// Connect the real driver to a running `TestServer`. Leaves `prefer_socket`
/// at its default (true) so the driver issues its full connect-time settings
/// query, `SELECT @@max_allowed_packet,@@wait_timeout,@@socket`, which the
/// server must answer for the connection to establish.
async fn connect(server: &TestServer, user: &str, pass: &str) -> Conn {
    let opts = OptsBuilder::default()
        .ip_or_hostname("127.0.0.1")
        .tcp_port(server.addr.port())
        .user(Some(user))
        .pass(Some(pass));
    Conn::new(opts)
        .await
        .expect("real driver connects and authenticates")
}

/// The full workload, run once the driver is connected: trivial query, CRUD
/// over the text protocol, a prepared statement over the binary protocol, and
/// an explicit transaction — all issued by `mysql_async` itself.
async fn run_workload(conn: &mut Conn) {
    // Trivial constant query (text protocol).
    let one: Vec<u8> = conn.query("SELECT 1").await.expect("SELECT 1");
    assert_eq!(one, vec![1]);

    // A coherent @@version the driver can read back.
    let version: Vec<String> = conn
        .query("SELECT @@version")
        .await
        .expect("SELECT @@version");
    assert_eq!(version.len(), 1);
    assert!(
        version[0].contains("mysql-rust"),
        "unexpected @@version: {:?}",
        version[0]
    );

    // Realistic CRUD (text protocol).
    conn.query_drop("CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR)")
        .await
        .expect("CREATE TABLE");
    conn.query_drop("INSERT INTO users (id, name) VALUES (1, 'alice'), (2, 'bob')")
        .await
        .expect("INSERT");
    let rows: Vec<(i32, String)> = conn
        .query("SELECT id, name FROM users")
        .await
        .expect("SELECT rows");
    assert_eq!(rows, vec![(1, "alice".to_string()), (2, "bob".to_string())]);

    // Prepared statement with a bound parameter (binary protocol both ways:
    // the driver sends the param binary-encoded, the server replies with a
    // binary result row).
    let stmt = conn
        .prep("SELECT name FROM users WHERE id = ?")
        .await
        .expect("PREPARE");
    let names: Vec<String> = conn.exec(&stmt, (2,)).await.expect("EXECUTE");
    assert_eq!(names, vec!["bob".to_string()]);

    // Transaction driven through the real client: COMMIT persists.
    conn.query_drop("BEGIN").await.expect("BEGIN");
    conn.query_drop("INSERT INTO users (id, name) VALUES (3, 'carol')")
        .await
        .expect("INSERT in tx");
    conn.query_drop("COMMIT").await.expect("COMMIT");
    let after: Vec<(i32, String)> = conn
        .query("SELECT id, name FROM users WHERE id = 3")
        .await
        .expect("verify committed row");
    assert_eq!(after, vec![(3, "carol".to_string())]);
}

#[tokio::test]
async fn real_driver_caching_sha2_end_to_end() {
    let config = Config {
        users: vec![UserCredential::with_caching_sha2_password(
            "alice", "s3cret",
        )],
        log_level: LogLevel::Error,
        ..Config::default()
    };
    let server = TestServer::start(config);

    let mut conn = connect(&server, "alice", "s3cret").await;
    run_workload(&mut conn).await;
    conn.disconnect().await.expect("clean disconnect");

    drop(server);
}

#[tokio::test]
async fn real_driver_native_password_end_to_end() {
    let config = Config {
        users: vec![UserCredential::with_password("bob", "hunter2")],
        log_level: LogLevel::Error,
        ..Config::default()
    };
    let server = TestServer::start(config);

    let mut conn = connect(&server, "bob", "hunter2").await;
    run_workload(&mut conn).await;
    conn.disconnect().await.expect("clean disconnect");

    drop(server);
}

#[tokio::test]
async fn real_driver_handles_jdbc_style_connection_boilerplate() {
    // The statements a JDBC driver (Connector/J) and GUI clients like DBeaver
    // fire around connect — none of which may error: a comment-wrapped,
    // multi-`@@variable` SELECT with `AS` aliases; `SET`/`USE`/`SHOW`; and
    // backtick-quoted identifiers.
    let config = Config {
        users: vec![UserCredential::with_caching_sha2_password(
            "alice", "s3cret",
        )],
        log_level: LogLevel::Error,
        ..Config::default()
    };
    let server = TestServer::start(config);
    let mut conn = connect(&server, "alice", "s3cret").await;

    // Connector/J's connect-time settings query: a leading `/* ... */` comment
    // then many `@@session.<var> AS <alias>` reads. Read back by alias.
    let row: Option<(i64, String, String, String)> = conn
        .query_first(
            "/* mysql-connector-java-8.0 */ SELECT \
             @@session.auto_increment_increment AS auto_increment_increment, \
             @@character_set_client AS character_set_client, \
             @@sql_mode AS sql_mode, \
             @@time_zone AS time_zone",
        )
        .await
        .expect("JDBC-style init SELECT must succeed");
    let (auto_inc, charset, _sql_mode, _tz) = row.expect("one row");
    assert_eq!(auto_inc, 1);
    assert_eq!(charset, "utf8mb4");

    // Session setup statements — accepted as no-ops.
    for stmt in [
        "SET NAMES utf8mb4",
        "SET character_set_results = NULL",
        "SET SESSION sql_mode = 'STRICT_TRANS_TABLES'",
        "SET autocommit = 1",
        "SET @my_user_var = 42",
        "USE mysql",
    ] {
        conn.query_drop(stmt)
            .await
            .unwrap_or_else(|e| panic!("{stmt:?} should be accepted: {e}"));
    }

    // Introspection the navigator uses.
    let vars: Vec<(String, String)> = conn
        .query("SHOW VARIABLES LIKE 'max_allowed_packet'")
        .await
        .expect("SHOW VARIABLES");
    assert_eq!(vars.len(), 1);
    assert_eq!(vars[0].0, "max_allowed_packet");
    let _: Vec<String> = conn.query("SHOW WARNINGS").await.expect("SHOW WARNINGS");
    let db: Option<Option<String>> = conn
        .query_first("SELECT DATABASE()")
        .await
        .expect("DATABASE()");
    assert_eq!(db, Some(None)); // NULL — schemaless

    // Backtick-quoted identifiers, used pervasively by GUI clients.
    conn.query_drop("CREATE TABLE `my tbl` (`id` INT PRIMARY KEY, `full name` VARCHAR)")
        .await
        .expect("backtick DDL");
    conn.query_drop("INSERT INTO `my tbl` (`id`, `full name`) VALUES (1, 'Ada')")
        .await
        .expect("backtick INSERT");
    let names: Vec<String> = conn
        .query("SELECT `full name` FROM `my tbl` WHERE `id` = 1")
        .await
        .expect("backtick SELECT");
    assert_eq!(names, vec!["Ada".to_string()]);

    conn.disconnect().await.expect("clean disconnect");
    drop(server);
}

#[tokio::test]
async fn real_driver_creates_and_drops_a_database() {
    // The exact regression this guards: a GUI client's "create database"
    // action first reads SHOW CHARACTER SET / SHOW COLLATION to populate its
    // dialog (previously empty, which crashed the client with no SQL error at
    // all — nothing for the server to even reject), then actually issues
    // CREATE DATABASE. Drive the whole sequence through the real driver.
    let config = Config {
        users: vec![UserCredential::with_caching_sha2_password(
            "alice", "s3cret",
        )],
        log_level: LogLevel::Error,
        ..Config::default()
    };
    let server = TestServer::start(config);
    let mut conn = connect(&server, "alice", "s3cret").await;

    // Charset, Description, Default collation, Maxlen.
    let charsets: Vec<(String, String, String, i32)> = conn
        .query("SHOW CHARACTER SET")
        .await
        .expect("SHOW CHARACTER SET");
    assert!(!charsets.is_empty(), "charset list must not be empty");

    // Collation, Charset, Id, Default, Compiled, Sortlen.
    let collations: Vec<(String, String, i32, String, String, i32)> =
        conn.query("SHOW COLLATION").await.expect("SHOW COLLATION");
    assert!(!collations.is_empty(), "collation list must not be empty");

    conn.query_drop("CREATE DATABASE IF NOT EXISTS my_new_db")
        .await
        .expect("CREATE DATABASE");
    let databases: Vec<String> = conn.query("SHOW DATABASES").await.expect("SHOW DATABASES");
    assert!(databases.contains(&"my_new_db".to_string()));

    conn.query_drop("DROP DATABASE my_new_db")
        .await
        .expect("DROP DATABASE");
    let databases: Vec<String> = conn.query("SHOW DATABASES").await.expect("SHOW DATABASES");
    assert!(!databases.contains(&"my_new_db".to_string()));

    conn.disconnect().await.expect("clean disconnect");
    drop(server);
}

/// The literal statement DBeaver's "Create Database" dialog sends is
/// `CREATE SCHEMA`, not `CREATE DATABASE` — a real MySQL synonym this server
/// didn't originally accept (`expected TABLE, found 'SCHEMA'`), which is what
/// actually broke the reported flow (the SHOW CHARACTER SET/COLLATION fix
/// above addressed a *related* client-side crash, but not this one).
#[tokio::test]
async fn real_driver_create_schema_synonym() {
    let config = Config {
        users: vec![UserCredential::with_caching_sha2_password(
            "alice", "s3cret",
        )],
        log_level: LogLevel::Error,
        ..Config::default()
    };
    let server = TestServer::start(config);
    let mut conn = connect(&server, "alice", "s3cret").await;

    conn.query_drop("CREATE SCHEMA IF NOT EXISTS my_schema")
        .await
        .expect("CREATE SCHEMA");
    let databases: Vec<String> = conn.query("SHOW DATABASES").await.expect("SHOW DATABASES");
    assert!(databases.contains(&"my_schema".to_string()));

    conn.query_drop("DROP SCHEMA my_schema")
        .await
        .expect("DROP SCHEMA");

    conn.disconnect().await.expect("clean disconnect");
    drop(server);
}

/// The literal DDL DBeaver's visual table editor generated (captured
/// verbatim from a live debug-log session, see `parser::tests::
/// dbeaver_generated_create_table_parses` for the same text) — run through
/// the real driver, followed by the INSERT a GUI's data editor issues, which
/// omits the AUTO_INCREMENT column and expects the server to assign it.
#[tokio::test]
async fn real_driver_dbeaver_create_table_and_auto_increment_insert() {
    let config = Config {
        users: vec![UserCredential::with_caching_sha2_password(
            "alice", "s3cret",
        )],
        log_level: LogLevel::Error,
        ..Config::default()
    };
    let server = TestServer::start(config);
    let mut conn = connect(&server, "alice", "s3cret").await;

    conn.query_drop(
        "CREATE TABLE testfd.NewTable (\n\
         \tId INT auto_increment NOT NULL,\n\
         \tName varchar(100) NULL,\n\
         \tCONSTRAINT NewTable_PK PRIMARY KEY (Id)\n\
         )\n\
         DEFAULT CHARSET=utf8mb4\n\
         COLLATE=utf8mb4_general_ci",
    )
    .await
    .expect("the exact DBeaver-generated CREATE TABLE must succeed");

    // A GUI data editor's generated INSERT omits the auto-increment column.
    conn.query_drop("INSERT INTO NewTable (Name) VALUES ('Ada')")
        .await
        .expect("INSERT omitting the AUTO_INCREMENT column");
    conn.query_drop("INSERT INTO NewTable (Name) VALUES ('Grace')")
        .await
        .expect("second INSERT");

    let rows: Vec<(i32, String)> = conn
        .query("SELECT Id, Name FROM NewTable")
        .await
        .expect("SELECT");
    assert_eq!(rows, vec![(1, "Ada".to_string()), (2, "Grace".to_string())]);

    // An explicit high value, then another omitted-Id insert: the sequence
    // must jump past it rather than colliding with a duplicate-key error.
    conn.query_drop("INSERT INTO NewTable (Id, Name) VALUES (100, 'Linus')")
        .await
        .expect("explicit Id value");
    conn.query_drop("INSERT INTO NewTable (Name) VALUES ('Margaret')")
        .await
        .expect("auto-assigned Id must not collide with the explicit one");
    let last: Option<i32> = conn
        .query_first("SELECT Id FROM NewTable WHERE Name = 'Margaret'")
        .await
        .expect("SELECT");
    assert_eq!(last, Some(101));

    conn.disconnect().await.expect("clean disconnect");
    drop(server);
}

/// `ORDER BY`/`LIMIT`/`OFFSET` — a GUI data grid's column-sort-click and
/// page-through-results both compile down to exactly this.
#[tokio::test]
async fn real_driver_order_by_limit_offset() {
    let config = Config {
        users: vec![UserCredential::with_caching_sha2_password(
            "alice", "s3cret",
        )],
        log_level: LogLevel::Error,
        ..Config::default()
    };
    let server = TestServer::start(config);
    let mut conn = connect(&server, "alice", "s3cret").await;

    conn.query_drop("CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR)")
        .await
        .expect("CREATE TABLE");
    conn.query_drop("INSERT INTO t (id, name) VALUES (1, 'carol'), (2, 'alice'), (3, 'bob')")
        .await
        .expect("INSERT");

    // Column-sort-click: ORDER BY name ASC / DESC.
    let ascending: Vec<String> = conn
        .query("SELECT name FROM t ORDER BY name")
        .await
        .expect("ORDER BY ASC");
    assert_eq!(
        ascending,
        vec!["alice".to_string(), "bob".to_string(), "carol".to_string()]
    );
    let descending: Vec<String> = conn
        .query("SELECT name FROM t ORDER BY name DESC")
        .await
        .expect("ORDER BY DESC");
    assert_eq!(
        descending,
        vec!["carol".to_string(), "bob".to_string(), "alice".to_string()]
    );

    // Page-through-results: LIMIT/OFFSET, ordered by the primary key.
    let page1: Vec<i32> = conn
        .query("SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 0")
        .await
        .expect("page 1");
    assert_eq!(page1, vec![1, 2]);
    let page2: Vec<i32> = conn
        .query("SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 2")
        .await
        .expect("page 2");
    assert_eq!(page2, vec![3]);

    conn.disconnect().await.expect("clean disconnect");
    drop(server);
}

/// `DECIMAL`/`DATE`/`BOOLEAN` (Phase 11): exact fixed-point money math, a
/// calendar date sorting/filtering correctly, and BOOLEAN as the plain INT
/// alias real MySQL treats it as. Both `Decimal` and `Date` are wire-encoded
/// as text (see `server::connection::value_to_cell`), so the driver reads
/// them back as ordinary strings/ints — exactly what a real client does.
#[tokio::test]
async fn real_driver_decimal_date_and_boolean() {
    let config = Config {
        users: vec![UserCredential::with_caching_sha2_password(
            "alice", "s3cret",
        )],
        log_level: LogLevel::Error,
        ..Config::default()
    };
    let server = TestServer::start(config);
    let mut conn = connect(&server, "alice", "s3cret").await;

    conn.query_drop(
        "CREATE TABLE orders (\n\
         \tid INT AUTO_INCREMENT PRIMARY KEY,\n\
         \ttotal DECIMAL(10,2) NOT NULL,\n\
         \tplaced_on DATE NOT NULL,\n\
         \tpaid BOOLEAN NOT NULL\n\
         )",
    )
    .await
    .expect("CREATE TABLE with DECIMAL/DATE/BOOLEAN columns");

    conn.query_drop(
        "INSERT INTO orders (total, placed_on, paid) VALUES \
         (19.99, '2024-01-15', TRUE), \
         (5, '2023-12-25', FALSE), \
         (100.005, '2024-06-01', TRUE)",
    )
    .await
    .expect("INSERT with decimal/date/boolean literals");

    // DECIMAL round-trips as exact text — no float rounding artifacts (a
    // real f64 can't even represent 19.99 exactly; a driver reading this
    // value as a string proves the server never went through one).
    let totals: Vec<String> = conn
        .query("SELECT total FROM orders ORDER BY id")
        .await
        .expect("SELECT total");
    // 5 (int) normalizes to the column's scale; 100.005 rounds to 100.01
    // (half-away-from-zero) at scale 2.
    assert_eq!(totals, vec!["19.99", "5.00", "100.01"]);

    // DATE orders chronologically (not insertion order).
    let dates: Vec<String> = conn
        .query("SELECT placed_on FROM orders ORDER BY placed_on")
        .await
        .expect("SELECT placed_on ORDER BY");
    assert_eq!(dates, vec!["2023-12-25", "2024-01-15", "2024-06-01"]);

    // BOOLEAN reads back as a plain integer 0/1.
    let paid_flags: Vec<i32> = conn
        .query("SELECT paid FROM orders ORDER BY id")
        .await
        .expect("SELECT paid");
    assert_eq!(paid_flags, vec![1, 0, 1]);

    // A DECIMAL WHERE filter compares numerically, not lexically.
    let expensive: Vec<String> = conn
        .query("SELECT total FROM orders WHERE total > 10.00 ORDER BY total")
        .await
        .expect("SELECT ... WHERE total > 10.00");
    assert_eq!(expensive, vec!["19.99", "100.01"]);

    // A malformed date is a clean ERR, not a crash.
    let bad_date = conn
        .query_drop("INSERT INTO orders (total, placed_on, paid) VALUES (1.00, 'not-a-date', TRUE)")
        .await;
    assert!(
        bad_date.is_err(),
        "a malformed DATE literal must be rejected"
    );

    conn.disconnect().await.expect("clean disconnect");
    drop(server);
}

/// `GROUP BY` + aggregate functions (Phase 11): a realistic "totals by
/// category" report — exactly the kind of query a GUI's aggregate/pivot
/// view or a dashboard would issue.
#[tokio::test]
async fn real_driver_group_by_and_aggregates() {
    let config = Config {
        users: vec![UserCredential::with_caching_sha2_password(
            "alice", "s3cret",
        )],
        log_level: LogLevel::Error,
        ..Config::default()
    };
    let server = TestServer::start(config);
    let mut conn = connect(&server, "alice", "s3cret").await;

    conn.query_drop(
        "CREATE TABLE sales (id INT PRIMARY KEY, category VARCHAR, amount DECIMAL(10,2))",
    )
    .await
    .expect("CREATE TABLE");
    conn.query_drop(
        "INSERT INTO sales VALUES \
         (1, 'fruit', 10.00), (2, 'fruit', 5.50), \
         (3, 'veg', 3.25), (4, 'veg', 7.75), (5, 'veg', 1.00)",
    )
    .await
    .expect("INSERT");

    // A plain aggregate (no GROUP BY) over the whole table.
    let total_count: Option<i64> = conn
        .query_first("SELECT COUNT(*) FROM sales")
        .await
        .expect("COUNT(*)");
    assert_eq!(total_count, Some(5));

    let grand_total: Option<String> = conn
        .query_first("SELECT SUM(amount) FROM sales")
        .await
        .expect("SUM(amount)");
    assert_eq!(grand_total.as_deref(), Some("27.50"));

    // GROUP BY: one row per category, with a count and a sum, sorted by the
    // aggregate's own alias — a report a real dashboard would generate.
    let report: Vec<(String, i64, String)> = conn
        .query(
            "SELECT category, COUNT(*) AS n, SUM(amount) AS total \
             FROM sales GROUP BY category ORDER BY total DESC",
        )
        .await
        .expect("GROUP BY report");
    assert_eq!(
        report,
        vec![
            ("fruit".to_string(), 2, "15.50".to_string()),
            ("veg".to_string(), 3, "12.00".to_string()),
        ]
    );

    // WHERE filters before grouping (not after).
    let filtered: Vec<(String, i64)> = conn
        .query("SELECT category, COUNT(*) FROM sales WHERE amount > 4.00 GROUP BY category")
        .await
        .expect("filtered GROUP BY");
    assert_eq!(
        filtered,
        vec![("fruit".to_string(), 2), ("veg".to_string(), 1)]
    );

    // AVG returns exact fixed-point, not a float approximation.
    let avg: Option<String> = conn
        .query_first("SELECT AVG(amount) FROM sales")
        .await
        .expect("AVG(amount)");
    assert_eq!(avg.as_deref(), Some("5.500000"));

    // MIN/MAX.
    let bounds: Option<(String, String)> = conn
        .query_first("SELECT MIN(amount), MAX(amount) FROM sales")
        .await
        .expect("MIN/MAX");
    assert_eq!(bounds, Some(("1.00".to_string(), "10.00".to_string())));

    // A non-grouped, non-aggregated column is a clean ERR, not a crash or
    // silently-wrong data (standard SQL; MySQL's own default too).
    let bad = conn
        .query_drop("SELECT id, category FROM sales GROUP BY category")
        .await;
    assert!(bad.is_err());

    conn.disconnect().await.expect("clean disconnect");
    drop(server);
}

#[tokio::test]
async fn real_driver_join() {
    let config = Config {
        users: vec![UserCredential::with_caching_sha2_password(
            "alice", "s3cret",
        )],
        log_level: LogLevel::Error,
        ..Config::default()
    };
    let server = TestServer::start(config);
    let mut conn = connect(&server, "alice", "s3cret").await;

    conn.query_drop("CREATE TABLE customers (id INT PRIMARY KEY, name VARCHAR)")
        .await
        .expect("CREATE TABLE customers");
    conn.query_drop(
        "CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT, total DECIMAL(10,2))",
    )
    .await
    .expect("CREATE TABLE orders");
    conn.query_drop("INSERT INTO customers VALUES (1, 'Ada'), (2, 'Grace'), (3, 'Alan')")
        .await
        .expect("INSERT customers");
    // Alan (3) deliberately has no orders — the row an INNER JOIN must drop
    // and a LEFT JOIN must keep, NULL-padded.
    conn.query_drop("INSERT INTO orders VALUES (100, 1, 9.99), (101, 1, 5.00), (102, 2, 20.00)")
        .await
        .expect("INSERT orders");

    // INNER JOIN with table aliases and qualified column references.
    let inner: Vec<(String, String)> = conn
        .query(
            "SELECT c.name, o.total FROM customers c JOIN orders o \
             ON c.id = o.customer_id ORDER BY o.id",
        )
        .await
        .expect("INNER JOIN");
    assert_eq!(
        inner,
        vec![
            ("Ada".to_string(), "9.99".to_string()),
            ("Ada".to_string(), "5.00".to_string()),
            ("Grace".to_string(), "20.00".to_string()),
        ]
    );

    // LEFT JOIN keeps Alan, with NULL order columns.
    let left: Vec<(String, Option<String>)> = conn
        .query(
            "SELECT c.name, o.total FROM customers c LEFT JOIN orders o \
             ON c.id = o.customer_id ORDER BY c.id, o.id",
        )
        .await
        .expect("LEFT JOIN");
    assert_eq!(
        left,
        vec![
            ("Ada".to_string(), Some("9.99".to_string())),
            ("Ada".to_string(), Some("5.00".to_string())),
            ("Grace".to_string(), Some("20.00".to_string())),
            ("Alan".to_string(), None),
        ]
    );

    // WHERE filters the joined result, and GROUP BY/aggregates work over it
    // too — a report a real dashboard would generate.
    let report: Vec<(String, i64, String)> = conn
        .query(
            "SELECT c.name, COUNT(*), SUM(o.total) FROM customers c JOIN orders o \
             ON c.id = o.customer_id GROUP BY c.name ORDER BY c.name",
        )
        .await
        .expect("GROUP BY over a JOIN");
    assert_eq!(
        report,
        vec![
            ("Ada".to_string(), 2, "14.99".to_string()),
            ("Grace".to_string(), 1, "20.00".to_string()),
        ]
    );

    // An unqualified column that exists on both sides is a clean ERR
    // ("ambiguous"), not a silent guess.
    let ambiguous = conn
        .query_drop("SELECT id FROM customers c JOIN orders o ON c.id = o.customer_id")
        .await;
    assert!(ambiguous.is_err());

    conn.disconnect().await.expect("clean disconnect");
    drop(server);
}

#[tokio::test]
async fn real_driver_alter_table() {
    let config = Config {
        users: vec![UserCredential::with_caching_sha2_password(
            "alice", "s3cret",
        )],
        log_level: LogLevel::Error,
        ..Config::default()
    };
    let server = TestServer::start(config);
    let mut conn = connect(&server, "alice", "s3cret").await;

    conn.query_drop("CREATE TABLE customers (id INT, name VARCHAR)")
        .await
        .expect("CREATE TABLE customers");
    conn.query_drop("INSERT INTO customers VALUES (1, 'Ada'), (2, 'Grace')")
        .await
        .expect("INSERT customers");

    // ADD COLUMN: existing rows read back NULL for it.
    conn.query_drop("ALTER TABLE customers ADD COLUMN email VARCHAR")
        .await
        .expect("ADD COLUMN");
    let with_email: Vec<(String, Option<String>)> = conn
        .query("SELECT name, email FROM customers ORDER BY name")
        .await
        .expect("SELECT after ADD COLUMN");
    assert_eq!(
        with_email,
        vec![("Ada".to_string(), None), ("Grace".to_string(), None),]
    );
    conn.query_drop("INSERT INTO customers VALUES (3, 'Alan', 'alan@example.com')")
        .await
        .expect("INSERT using the new column");

    // ADD PRIMARY KEY on an existing column: now enforced.
    conn.query_drop("ALTER TABLE customers ADD PRIMARY KEY (id)")
        .await
        .expect("ADD PRIMARY KEY");
    let duplicate_id = conn
        .query_drop("INSERT INTO customers VALUES (1, 'Duplicate', NULL)")
        .await;
    assert!(
        duplicate_id.is_err(),
        "primary key added via ALTER TABLE should reject a duplicate id"
    );

    // MODIFY COLUMN: widen an INT to VARCHAR in place.
    conn.query_drop("ALTER TABLE customers MODIFY COLUMN id VARCHAR")
        .await
        .expect("MODIFY COLUMN");
    let ids: Vec<String> = conn
        .query("SELECT id FROM customers ORDER BY id")
        .await
        .expect("SELECT after MODIFY COLUMN");
    assert_eq!(ids, vec!["1".to_string(), "2".to_string(), "3".to_string()]);

    // DROP COLUMN: gone from a subsequent SELECT *.
    conn.query_drop("ALTER TABLE customers DROP COLUMN email")
        .await
        .expect("DROP COLUMN");
    let remaining_columns: Vec<(String, String)> = conn
        .query("SELECT id, name FROM customers ORDER BY id")
        .await
        .expect("SELECT after DROP COLUMN");
    assert_eq!(
        remaining_columns,
        vec![
            ("1".to_string(), "Ada".to_string()),
            ("2".to_string(), "Grace".to_string()),
            ("3".to_string(), "Alan".to_string()),
        ]
    );
    let select_star_no_longer_has_email = conn.query_drop("SELECT email FROM customers").await;
    assert!(select_star_no_longer_has_email.is_err());

    // DROP PRIMARY KEY: no longer enforced.
    conn.query_drop("ALTER TABLE customers DROP PRIMARY KEY")
        .await
        .expect("DROP PRIMARY KEY");
    conn.query_drop("INSERT INTO customers VALUES ('1', 'Also Ada')")
        .await
        .expect("duplicate id now accepted, no primary key left");

    conn.disconnect().await.expect("clean disconnect");
    drop(server);
}

#[tokio::test]
async fn real_driver_connects_with_env_configured_account() {
    // Exactly what `MYSQLRUST_USER=alice MYSQLRUST_PASSWORD=s3cret cargo run`
    // produces — built through the same `from_env` code path, but with an
    // injected lookup so the global process environment isn't touched.
    let vars: std::collections::HashMap<&str, &str> = [
        ("MYSQLRUST_USER", "alice"),
        ("MYSQLRUST_PASSWORD", "s3cret"),
    ]
    .into_iter()
    .collect();
    let mut config =
        Config::from_env_with(|k| vars.get(k).map(|s| s.to_string())).expect("env config parses");
    config.log_level = LogLevel::Error;

    let server = TestServer::start(config);
    let mut conn = connect(&server, "alice", "s3cret").await;
    let one: Vec<u8> = conn.query("SELECT 1").await.expect("SELECT 1");
    assert_eq!(one, vec![1]);
    conn.disconnect().await.expect("clean disconnect");

    drop(server);
}

#[tokio::test]
async fn real_driver_rejects_bad_password() {
    let config = Config {
        users: vec![UserCredential::with_caching_sha2_password(
            "alice", "s3cret",
        )],
        log_level: LogLevel::Error,
        ..Config::default()
    };
    let server = TestServer::start(config);

    let opts = OptsBuilder::default()
        .ip_or_hostname("127.0.0.1")
        .tcp_port(server.addr.port())
        .user(Some("alice"))
        .pass(Some("wrong-password"));
    let result = Conn::new(opts).await;
    assert!(
        result.is_err(),
        "the driver must reject a connection with a bad password"
    );

    drop(server);
}
