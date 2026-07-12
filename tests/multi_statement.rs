//! Integration tests for Phase 8's multi-statement / multi-resultset
//! support (`CLIENT_MULTI_STATEMENTS`): several `;`-separated statements in
//! one `COM_QUERY`, each producing its own result with
//! `SERVER_MORE_RESULTS_EXISTS` set between them.

mod common;

use mysql_rust::auth::native_password::compute_auth_response;
use mysql_rust::config::{Config, UserCredential};
use mysql_rust::protocol::capabilities::{
    CLIENT_DEPRECATE_EOF, CLIENT_MULTI_STATEMENTS, CLIENT_PLUGIN_AUTH, CLIENT_PROTOCOL_41,
    CLIENT_SECURE_CONNECTION,
};
use mysql_rust::protocol::Packet;

use common::{extract_scramble, TestClient, TestServer};

const COM_QUERY: u8 = 0x03;
const SERVER_MORE_RESULTS_EXISTS: u16 = 0x0008;

fn test_config() -> Config {
    Config {
        users: vec![UserCredential::with_password("alice", "s3cret")],
        ..Config::default()
    }
}

/// Authenticate, negotiating `CLIENT_MULTI_STATEMENTS` unless `multi` is false.
fn authenticate(client: &mut TestClient, multi: bool) {
    let handshake_packet = client.read_packet();
    let scramble = extract_scramble(&handshake_packet.payload);
    let auth_response = compute_auth_response(Some(b"s3cret"), &scramble);
    let mut caps =
        CLIENT_PROTOCOL_41 | CLIENT_PLUGIN_AUTH | CLIENT_SECURE_CONNECTION | CLIENT_DEPRECATE_EOF;
    if multi {
        caps |= CLIENT_MULTI_STATEMENTS;
    }
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
    assert_eq!(verdict.payload[0], 0x00, "auth should succeed");
}

fn send_query(client: &mut TestClient, sql: &str) {
    let mut payload = vec![COM_QUERY];
    payload.extend_from_slice(sql.as_bytes());
    client.write_packet(&Packet::new(0, payload));
}

/// One decoded result from a (possibly multi-) query response.
enum QueryResponse {
    /// An OK packet: (affected_rows, more_results_flag).
    Ok {
        affected_rows: u8,
        more: bool,
    },
    /// A result set: (rows, more_results_flag).
    ResultSet {
        rows: Vec<Vec<String>>,
        more: bool,
    },
    Err,
}

/// Read one response unit from the stream — an OK, an ERR, or a full result
/// set — and report whether `SERVER_MORE_RESULTS_EXISTS` was set on its
/// terminator (so the caller knows to read another).
fn read_response(client: &mut TestClient) -> QueryResponse {
    let first = client.read_packet();
    match first.payload[0] {
        0xff => QueryResponse::Err,
        0x00 => {
            // OK packet: 0x00, affected_rows(lenenc, 1 byte here), last_insert
            // (lenenc, 1 byte), status(2), warnings(2).
            let affected_rows = first.payload[1];
            let status = u16::from_le_bytes([first.payload[3], first.payload[4]]);
            QueryResponse::Ok {
                affected_rows,
                more: status & SERVER_MORE_RESULTS_EXISTS != 0,
            }
        }
        _ => {
            // Column-count packet leads a result set.
            let column_count = first.payload[0] as usize;
            for _ in 0..column_count {
                let _def = client.read_packet();
            }
            let mut rows = Vec::new();
            let more;
            loop {
                let packet = client.read_packet();
                // Terminator: OK-style packet, 0xFE header, length < 9.
                if packet.payload.first() == Some(&0xfe) && packet.payload.len() < 9 {
                    let status = u16::from_le_bytes([packet.payload[3], packet.payload[4]]);
                    more = status & SERVER_MORE_RESULTS_EXISTS != 0;
                    break;
                }
                rows.push(parse_text_row(&packet.payload, column_count));
            }
            QueryResponse::ResultSet { rows, more }
        }
    }
}

fn parse_text_row(payload: &[u8], column_count: usize) -> Vec<String> {
    let mut values = Vec::with_capacity(column_count);
    let mut pos = 0;
    for _ in 0..column_count {
        let len = payload[pos] as usize;
        pos += 1;
        values.push(String::from_utf8(payload[pos..pos + len].to_vec()).expect("utf8"));
        pos += len;
    }
    values
}

/// Read a whole multi-statement response: a vector of responses, following
/// the `more` flag from each to decide whether to read the next.
fn read_all_responses(client: &mut TestClient) -> Vec<QueryResponse> {
    let mut responses = Vec::new();
    loop {
        let response = read_response(client);
        let more = match &response {
            QueryResponse::Ok { more, .. } => *more,
            QueryResponse::ResultSet { more, .. } => *more,
            QueryResponse::Err => false,
        };
        responses.push(response);
        if !more {
            break;
        }
    }
    responses
}

#[test]
fn batch_of_selects_returns_a_result_set_each_with_more_flags_between() {
    let server = TestServer::start(test_config());
    let mut client = server.connect();
    authenticate(&mut client, true);

    send_query(&mut client, "SELECT 1; SELECT 2; SELECT 3");
    let responses = read_all_responses(&mut client);

    assert_eq!(responses.len(), 3);
    for (i, response) in responses.iter().enumerate() {
        match response {
            QueryResponse::ResultSet { rows, more } => {
                // The i-th statement was `SELECT {i+1}`.
                assert_eq!(rows, &vec![vec![(i + 1).to_string()]]);
                // MORE flag set on all but the last.
                assert_eq!(*more, i + 1 < 3, "wrong more-results flag at index {i}");
            }
            _ => panic!("expected a result set at index {i}"),
        }
    }
}

#[test]
fn batch_mixing_dml_and_select_produces_ok_then_result_set() {
    let server = TestServer::start(test_config());
    let mut client = server.connect();
    authenticate(&mut client, true);

    send_query(
        &mut client,
        "CREATE TABLE t (id INT PRIMARY KEY); INSERT INTO t VALUES (1), (2); SELECT id FROM t",
    );
    let responses = read_all_responses(&mut client);
    assert_eq!(responses.len(), 3);

    match &responses[0] {
        QueryResponse::Ok { more, .. } => assert!(*more),
        _ => panic!("CREATE TABLE should be an OK"),
    }
    match &responses[1] {
        QueryResponse::Ok {
            affected_rows,
            more,
        } => {
            assert_eq!(*affected_rows, 2, "INSERT affected 2 rows");
            assert!(*more);
        }
        _ => panic!("INSERT should be an OK"),
    }
    match &responses[2] {
        QueryResponse::ResultSet { rows, more } => {
            assert_eq!(rows, &vec![vec!["1".to_string()], vec!["2".to_string()]]);
            assert!(!*more, "the last result must not set MORE_RESULTS");
        }
        _ => panic!("SELECT should be a result set"),
    }
}

#[test]
fn a_failing_statement_aborts_the_rest_of_the_batch() {
    let server = TestServer::start(test_config());
    let mut client = server.connect();
    authenticate(&mut client, true);

    // The second statement fails (no such table); the third must not run.
    send_query(
        &mut client,
        "CREATE TABLE t (id INT); SELECT * FROM missing; INSERT INTO t VALUES (1)",
    );
    let responses = read_all_responses(&mut client);
    assert_eq!(responses.len(), 2, "batch should stop after the error");
    assert!(matches!(responses[0], QueryResponse::Ok { .. }));
    assert!(matches!(responses[1], QueryResponse::Err));

    // Confirm the third statement really didn't run: t is still empty.
    send_query(&mut client, "SELECT id FROM t");
    match read_response(&mut client) {
        QueryResponse::ResultSet { rows, .. } => assert!(rows.is_empty()),
        _ => panic!("expected a result set"),
    }
}

#[test]
fn multiple_statements_are_rejected_without_the_capability() {
    let server = TestServer::start(test_config());
    let mut client = server.connect();
    authenticate(&mut client, false); // did NOT negotiate CLIENT_MULTI_STATEMENTS

    send_query(&mut client, "SELECT 1; SELECT 2");
    assert!(
        matches!(read_response(&mut client), QueryResponse::Err),
        "a multi-statement query without the capability must be an error"
    );

    // A single statement still works fine on the same connection.
    send_query(&mut client, "SELECT 1");
    match read_response(&mut client) {
        QueryResponse::ResultSet { rows, .. } => assert_eq!(rows, vec![vec!["1".to_string()]]),
        _ => panic!("single statement should still work"),
    }
}
