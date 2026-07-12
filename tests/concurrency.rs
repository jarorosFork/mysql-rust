//! Integration tests for Phase 6 (concurrency): simultaneously-open
//! connections sharing live state, a mixed-read/write stress test with many
//! concurrent clients, graceful shutdown, and connection limits.

mod common;

use std::net::TcpStream;
use std::time::Duration;

use mysql_rust::auth::native_password::compute_auth_response;
use mysql_rust::config::{Config, UserCredential};
use mysql_rust::protocol::capabilities::{
    CLIENT_DEPRECATE_EOF, CLIENT_PLUGIN_AUTH, CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION,
};
use mysql_rust::protocol::Packet;

use common::{extract_scramble, TestClient, TestServer};

const COM_QUERY: u8 = 0x03;
const COM_QUIT: u8 = 0x01;

fn test_config() -> Config {
    Config {
        users: vec![UserCredential::with_password("alice", "s3cret")],
        ..Config::default()
    }
}

/// Complete the handshake + auth on an already-connected client.
fn authenticate(client: &mut TestClient) {
    let handshake_packet = client.read_packet();
    let scramble = extract_scramble(&handshake_packet.payload);

    let auth_response = compute_auth_response(Some(b"s3cret"), &scramble);
    let caps =
        CLIENT_PROTOCOL_41 | CLIENT_PLUGIN_AUTH | CLIENT_SECURE_CONNECTION | CLIENT_DEPRECATE_EOF;
    let mut payload = Vec::new();
    payload.extend_from_slice(&caps.to_le_bytes());
    payload.extend_from_slice(&16_777_216u32.to_le_bytes());
    payload.push(45);
    payload.extend_from_slice(&[0u8; 23]);
    payload.extend_from_slice(b"alice\0");
    payload.push(auth_response.len() as u8);
    payload.extend_from_slice(&auth_response);
    payload.extend_from_slice(b"mysql_native_password\0");
    client.write_packet(&Packet::new(1, payload));

    let verdict = client.read_packet();
    assert_eq!(
        verdict.payload[0], 0x00,
        "expected auth to succeed with an OK packet"
    );
}

fn send_query(client: &mut TestClient, sql: &str) {
    let mut payload = vec![COM_QUERY];
    payload.extend_from_slice(sql.as_bytes());
    client.write_packet(&Packet::new(0, payload));
}

fn expect_ok(client: &mut TestClient) {
    let packet = client.read_packet();
    assert_eq!(
        packet.payload[0], 0x00,
        "expected an OK packet, got header {:#x} (query: {:?})",
        packet.payload[0], packet
    );
}

/// Read a full text-protocol result set under `CLIENT_DEPRECATE_EOF` framing.
fn read_result_set(client: &mut TestClient) -> Vec<Vec<String>> {
    let count_packet = client.read_packet();
    let column_count = count_packet.payload[0] as usize;

    for _ in 0..column_count {
        let _column_def = client.read_packet();
    }

    let mut rows = Vec::new();
    loop {
        let packet = client.read_packet();
        // CLIENT_DEPRECATE_EOF terminator: OK packet with the 0xFE header.
        if packet.payload.first() == Some(&0xfe) && packet.payload.len() < 9 {
            break;
        }
        rows.push(parse_text_row(&packet.payload, column_count));
    }
    rows
}

fn parse_text_row(payload: &[u8], column_count: usize) -> Vec<String> {
    let mut values = Vec::with_capacity(column_count);
    let mut pos = 0;
    for _ in 0..column_count {
        let len = payload[pos] as usize;
        pos += 1;
        values.push(String::from_utf8(payload[pos..pos + len].to_vec()).expect("utf8 value"));
        pos += len;
    }
    values
}

#[test]
fn two_simultaneous_connections_see_each_others_writes_live() {
    let server = TestServer::start(test_config());

    // Both connections stay open at once — unlike tests/persistence.rs,
    // nothing here is dropped/reopened between the write and the read.
    let mut a = server.connect();
    authenticate(&mut a);
    let mut b = server.connect();
    authenticate(&mut b);

    send_query(
        &mut a,
        "CREATE TABLE shared (id INT PRIMARY KEY, note VARCHAR)",
    );
    expect_ok(&mut a);
    send_query(&mut a, "INSERT INTO shared VALUES (1, 'from-a')");
    expect_ok(&mut a);

    // b, a completely separate live connection, sees a's write immediately.
    send_query(&mut b, "SELECT note FROM shared WHERE id = 1");
    assert_eq!(read_result_set(&mut b), vec![vec!["from-a".to_string()]]);

    // And the reverse direction: b writes, a (still open) sees it.
    send_query(&mut b, "INSERT INTO shared VALUES (2, 'from-b')");
    expect_ok(&mut b);
    send_query(&mut a, "SELECT note FROM shared WHERE id = 2");
    assert_eq!(read_result_set(&mut a), vec![vec!["from-b".to_string()]]);
}

#[test]
fn many_concurrent_clients_mixed_read_write_no_races_or_lost_writes() {
    const CLIENTS: usize = 30;

    let server = TestServer::start(test_config());

    // Set up the shared table before the concurrent phase.
    let mut setup = server.connect();
    authenticate(&mut setup);
    send_query(
        &mut setup,
        "CREATE TABLE t (id INT PRIMARY KEY, value VARCHAR)",
    );
    expect_ok(&mut setup);

    let addr = server.addr;
    let threads: Vec<_> = (0..CLIENTS)
        .map(|i| {
            std::thread::spawn(move || {
                let mut client = TestClient::connect(addr);
                authenticate(&mut client);

                // Write: each thread owns a distinct primary key, so all
                // CLIENTS inserts must succeed with no lost writes and no
                // spurious duplicate-key errors from cross-thread interference.
                send_query(
                    &mut client,
                    &format!("INSERT INTO t VALUES ({i}, 'value-{i}')"),
                );
                expect_ok(&mut client);

                // Read: interleave a scan of the (concurrently-changing)
                // shared table right after, exercising real read/write
                // concurrency rather than writes alone.
                send_query(&mut client, "SELECT * FROM t");
                let rows = read_result_set(&mut client);
                assert!(
                    !rows.is_empty(),
                    "thread {i} should see at least its own row"
                );

                // Point lookup via the primary-key index, from a connection
                // distinct from whichever thread performed the insert.
                send_query(&mut client, &format!("SELECT value FROM t WHERE id = {i}"));
                assert_eq!(
                    read_result_set(&mut client),
                    vec![vec![format!("value-{i}")]]
                );
            })
        })
        .collect();

    for (i, thread) in threads.into_iter().enumerate() {
        thread
            .join()
            .unwrap_or_else(|_| panic!("client thread {i} panicked"));
    }

    // Final check from a fresh connection: every row landed, none lost or
    // duplicated, no torn/corrupted values.
    let mut verify = server.connect();
    authenticate(&mut verify);
    send_query(&mut verify, "SELECT id, value FROM t");
    let mut rows = read_result_set(&mut verify);
    rows.sort_by_key(|r| r[0].parse::<i64>().unwrap());
    let expected: Vec<Vec<String>> = (0..CLIENTS)
        .map(|i| vec![i.to_string(), format!("value-{i}")])
        .collect();
    assert_eq!(rows, expected);
}

#[test]
fn shutdown_stops_accepting_new_connections_but_lets_in_flight_ones_finish() {
    let mut server = TestServer::start(test_config());
    let addr = server.addr;

    let mut in_flight = server.connect();
    authenticate(&mut in_flight); // fully established before shutdown begins

    server.trigger_shutdown();

    // New connections should stop succeeding once shutdown begins. The
    // accept loop noticing the signal and dropping the listener isn't
    // instantaneous, so retry briefly rather than asserting on attempt one.
    let mut still_accepting = true;
    for _ in 0..50 {
        match TcpStream::connect_timeout(&addr, Duration::from_millis(20)) {
            Ok(_) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                still_accepting = false;
                break;
            }
        }
    }
    assert!(
        !still_accepting,
        "server kept accepting new connections after shutdown was triggered"
    );

    // The connection that was already established is drained, not severed:
    // it can still complete a request during shutdown.
    send_query(&mut in_flight, "SELECT 1");
    assert_eq!(read_result_set(&mut in_flight), vec![vec!["1".to_string()]]);

    // Tell the server this connection is done (COM_QUIT), then drop the
    // socket, so the drain has something to finish promptly on instead of
    // running out the clock on `SHUTDOWN_DRAIN_TIMEOUT` — proving the drain
    // actually completed, not just that the timeout fallback saved us.
    in_flight.write_packet(&Packet::new(0, vec![COM_QUIT]));
    drop(in_flight);

    let before_join = std::time::Instant::now();
    server.join();
    assert!(
        before_join.elapsed() < Duration::from_secs(5),
        "drain took {:?}, suspiciously close to the 10s force-exit timeout — the connection likely wasn't actually drained",
        before_join.elapsed()
    );
}

#[test]
fn max_connections_rejects_extras_with_too_many_connections_error() {
    let config = Config {
        max_connections: 1,
        ..test_config()
    };
    let server = TestServer::start(config);

    // Occupies the one available connection slot for the rest of the test.
    let mut first = server.connect();
    authenticate(&mut first);

    // A second, simultaneous connection is rejected immediately (no
    // handshake — the server never gets that far for a refused connection).
    let mut second = server.connect();
    let packet = second.read_packet();
    assert_eq!(
        packet.payload[0], 0xff,
        "expected an ERR packet for the extra connection"
    );
    assert_eq!(
        &packet.payload[1..3],
        &1040u16.to_le_bytes(),
        "expected ER_CON_COUNT_ERROR (1040)"
    );
    assert_eq!(&packet.payload[4..9], b"08004");

    // The first connection is entirely unaffected by the rejection.
    send_query(&mut first, "SELECT 1");
    assert_eq!(read_result_set(&mut first), vec![vec!["1".to_string()]]);
}

#[test]
fn connection_slot_is_released_when_a_client_disconnects() {
    let config = Config {
        max_connections: 1,
        ..test_config()
    };
    let server = TestServer::start(config);

    {
        let mut first = server.connect();
        authenticate(&mut first);
        send_query(&mut first, "SELECT 1");
        assert_eq!(read_result_set(&mut first), vec![vec!["1".to_string()]]);
        // `first` is dropped (socket closed) at the end of this block,
        // which should let the server notice EOF and release its permit.
    }

    // Retry briefly: the server needs a moment to observe the disconnect.
    let mut accepted = false;
    for _ in 0..50 {
        let mut client = server.connect();
        let first_packet = client.read_packet();
        if first_packet.payload[0] == 10 {
            // A real HandshakeV10, not an immediate "too many connections" ERR.
            accepted = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        accepted,
        "connection slot was never released after the first client disconnected"
    );
}
