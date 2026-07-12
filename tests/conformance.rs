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
