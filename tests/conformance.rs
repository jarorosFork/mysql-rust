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
