//! Integration tests for Phase 8 (prepared statements): the full
//! `COM_STMT_PREPARE` -> `COM_STMT_EXECUTE` -> `COM_STMT_CLOSE` binary-
//! protocol exchange with bound parameters, driven by a scripted client
//! that speaks the wire format a standard driver would — the Phase 8
//! acceptance criterion in ROADMAP.md.

mod common;

use mysql_rust::auth::native_password::compute_auth_response;
use mysql_rust::config::{Config, UserCredential};
use mysql_rust::protocol::capabilities::{
    CLIENT_DEPRECATE_EOF, CLIENT_PLUGIN_AUTH, CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION,
};
use mysql_rust::protocol::Packet;

use common::{extract_scramble, TestClient, TestServer};

const COM_QUERY: u8 = 0x03;
const COM_STMT_PREPARE: u8 = 0x16;
const COM_STMT_EXECUTE: u8 = 0x17;
const COM_STMT_CLOSE: u8 = 0x19;
const COM_STMT_RESET: u8 = 0x1a;

const MYSQL_TYPE_LONGLONG: u8 = 0x08;
const MYSQL_TYPE_VAR_STRING: u8 = 0xfd;

fn test_config() -> Config {
    Config {
        users: vec![UserCredential::with_password("alice", "s3cret")],
        ..Config::default()
    }
}

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
    assert_eq!(verdict.payload[0], 0x00, "auth should succeed");
}

fn text_query(client: &mut TestClient, sql: &str) {
    let mut payload = vec![COM_QUERY];
    payload.extend_from_slice(sql.as_bytes());
    client.write_packet(&Packet::new(0, payload));
    let ok = client.read_packet();
    assert_eq!(ok.payload[0], 0x00, "setup query failed: {sql}");
}

fn connect(server: &TestServer) -> TestClient {
    let mut client = server.connect();
    authenticate(&mut client);
    client
}

/// Send `COM_STMT_PREPARE` and read the OK header, returning
/// `(statement_id, num_params)`. Consumes the parameter-definition packets
/// (we negotiate `CLIENT_DEPRECATE_EOF`, so there's no trailing EOF).
fn prepare(client: &mut TestClient, sql: &str) -> (u32, u16) {
    let mut payload = vec![COM_STMT_PREPARE];
    payload.extend_from_slice(sql.as_bytes());
    client.write_packet(&Packet::new(0, payload));

    let ok = client.read_packet();
    assert_eq!(
        ok.payload[0], 0x00,
        "expected COM_STMT_PREPARE_OK, got {:#x}",
        ok.payload[0]
    );
    let statement_id =
        u32::from_le_bytes([ok.payload[1], ok.payload[2], ok.payload[3], ok.payload[4]]);
    let num_columns = u16::from_le_bytes([ok.payload[5], ok.payload[6]]);
    let num_params = u16::from_le_bytes([ok.payload[7], ok.payload[8]]);
    assert_eq!(
        num_columns, 0,
        "this server reports columns at execute time, not prepare"
    );

    for _ in 0..num_params {
        let _param_def = client.read_packet();
    }
    (statement_id, num_params)
}

/// A bound parameter for `COM_STMT_EXECUTE`.
enum Param {
    Int(i64),
    Str(String),
    Null,
}

fn send_execute(client: &mut TestClient, statement_id: u32, params: &[Param]) {
    let mut payload = vec![COM_STMT_EXECUTE];
    payload.extend_from_slice(&statement_id.to_le_bytes());
    payload.push(0); // flags: CURSOR_TYPE_NO_CURSOR
    payload.extend_from_slice(&1u32.to_le_bytes()); // iteration_count

    if !params.is_empty() {
        // NULL bitmap.
        let bitmap_len = params.len().div_ceil(8);
        let mut bitmap = vec![0u8; bitmap_len];
        for (i, p) in params.iter().enumerate() {
            if matches!(p, Param::Null) {
                bitmap[i / 8] |= 1 << (i % 8);
            }
        }
        payload.extend_from_slice(&bitmap);
        payload.push(1); // new_params_bound_flag

        // Parameter types (2 bytes each: type + unsigned flag).
        for p in params {
            let type_byte = match p {
                Param::Int(_) => MYSQL_TYPE_LONGLONG,
                Param::Str(_) => MYSQL_TYPE_VAR_STRING,
                Param::Null => 0x06, // MYSQL_TYPE_NULL
            };
            payload.push(type_byte);
            payload.push(0);
        }
        // Parameter values (non-NULL only).
        for p in params {
            match p {
                Param::Int(n) => payload.extend_from_slice(&n.to_le_bytes()),
                Param::Str(s) => {
                    payload.push(s.len() as u8); // lenenc length (short strings)
                    payload.extend_from_slice(s.as_bytes());
                }
                Param::Null => {}
            }
        }
    }

    client.write_packet(&Packet::new(0, payload));
}

fn expect_ok(client: &mut TestClient) {
    let packet = client.read_packet();
    assert_eq!(
        packet.payload[0], 0x00,
        "expected an OK packet, got {:#x}",
        packet.payload[0]
    );
}

fn expect_err(client: &mut TestClient) {
    let packet = client.read_packet();
    assert_eq!(
        packet.payload[0], 0xff,
        "expected an ERR packet, got {:#x}",
        packet.payload[0]
    );
}

/// A decoded binary-protocol result cell.
#[derive(Debug, PartialEq)]
enum BinCell {
    Int(i64),
    Str(String),
    Null,
}

/// Read a full binary result set under `CLIENT_DEPRECATE_EOF` framing.
/// Returns the column MySQL type codes and the decoded rows.
fn read_binary_result_set(client: &mut TestClient) -> (Vec<u8>, Vec<Vec<BinCell>>) {
    let count_packet = client.read_packet();
    let column_count = count_packet.payload[0] as usize;

    let mut column_types = Vec::with_capacity(column_count);
    for _ in 0..column_count {
        let def = client.read_packet();
        column_types.push(column_type_of(&def.payload));
    }

    let mut rows = Vec::new();
    loop {
        let packet = client.read_packet();
        // The result-set terminator is an OK-style packet with the 0xFE
        // header (length < 9). A binary data row starts with 0x00, so this
        // is unambiguous.
        if packet.payload.first() == Some(&0xfe) && packet.payload.len() < 9 {
            break;
        }
        rows.push(decode_binary_row(&packet.payload, &column_types));
    }
    (column_types, rows)
}

fn column_type_of(column_def: &[u8]) -> u8 {
    // Skip catalog/schema/table/org_table/name/org_name (6 lenenc strings,
    // all single-byte-length here), then 0x0c, charset(2), length(4), type(1).
    let mut pos = 0;
    for _ in 0..6 {
        let len = column_def[pos] as usize;
        pos += 1 + len;
    }
    pos += 1; // length-of-fixed-fields (0x0c)
    pos += 2; // charset
    pos += 4; // column length
    column_def[pos]
}

fn decode_binary_row(payload: &[u8], column_types: &[u8]) -> Vec<BinCell> {
    let n = column_types.len();
    let bitmap_len = (n + 2).div_ceil(8);
    let null_bitmap = &payload[1..1 + bitmap_len];
    let mut pos = 1 + bitmap_len;

    let mut cells = Vec::with_capacity(n);
    for (i, &ty) in column_types.iter().enumerate() {
        let bit = i + 2;
        let is_null = null_bitmap[bit / 8] & (1 << (bit % 8)) != 0;
        if is_null {
            cells.push(BinCell::Null);
            continue;
        }
        match ty {
            MYSQL_TYPE_LONGLONG => {
                let b = &payload[pos..pos + 8];
                let arr = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
                cells.push(BinCell::Int(i64::from_le_bytes(arr)));
                pos += 8;
            }
            MYSQL_TYPE_VAR_STRING => {
                let len = payload[pos] as usize;
                pos += 1;
                cells.push(BinCell::Str(
                    String::from_utf8(payload[pos..pos + len].to_vec()).unwrap(),
                ));
                pos += len;
            }
            other => panic!("unexpected column type {other:#x} in binary row"),
        }
    }
    cells
}

#[test]
fn prepared_insert_and_select_round_trip_with_bound_params() {
    let server = TestServer::start(test_config());
    let mut client = connect(&server);

    text_query(
        &mut client,
        "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR)",
    );

    // Prepare a parameterized INSERT and execute it twice with different values.
    let (insert_id, insert_params) = prepare(&mut client, "INSERT INTO users VALUES (?, ?)");
    assert_eq!(insert_params, 2);

    send_execute(
        &mut client,
        insert_id,
        &[Param::Int(1), Param::Str("alice".to_string())],
    );
    expect_ok(&mut client);
    send_execute(
        &mut client,
        insert_id,
        &[Param::Int(2), Param::Str("bob".to_string())],
    );
    expect_ok(&mut client);

    // Prepare a parameterized SELECT and execute it, reading a binary result set.
    let (select_id, select_params) =
        prepare(&mut client, "SELECT id, name FROM users WHERE id = ?");
    assert_eq!(select_params, 1);

    send_execute(&mut client, select_id, &[Param::Int(2)]);
    let (types, rows) = read_binary_result_set(&mut client);
    // Column types reported accurately: id is LONGLONG, name is VAR_STRING.
    assert_eq!(types, vec![MYSQL_TYPE_LONGLONG, MYSQL_TYPE_VAR_STRING]);
    assert_eq!(
        rows,
        vec![vec![BinCell::Int(2), BinCell::Str("bob".to_string())]]
    );

    // A different bound value hits the other row.
    send_execute(&mut client, select_id, &[Param::Int(1)]);
    let (_types, rows) = read_binary_result_set(&mut client);
    assert_eq!(
        rows,
        vec![vec![BinCell::Int(1), BinCell::Str("alice".to_string())]]
    );
}

#[test]
fn prepared_statement_handles_null_and_negative_bound_params() {
    let server = TestServer::start(test_config());
    let mut client = connect(&server);
    text_query(&mut client, "CREATE TABLE t (a INT, b VARCHAR)");

    let (id, _) = prepare(&mut client, "INSERT INTO t VALUES (?, ?)");
    send_execute(&mut client, id, &[Param::Int(-42), Param::Null]);
    expect_ok(&mut client);

    // Read it back with a non-parameterized prepared SELECT.
    let (sel, _) = prepare(&mut client, "SELECT a, b FROM t");
    send_execute(&mut client, sel, &[]);
    let (_types, rows) = read_binary_result_set(&mut client);
    assert_eq!(rows, vec![vec![BinCell::Int(-42), BinCell::Null]]);
}

#[test]
fn executing_a_closed_statement_is_an_error_not_a_crash() {
    let server = TestServer::start(test_config());
    let mut client = connect(&server);
    text_query(&mut client, "CREATE TABLE t (a INT)");

    let (id, _) = prepare(&mut client, "INSERT INTO t VALUES (?)");
    send_execute(&mut client, id, &[Param::Int(1)]);
    expect_ok(&mut client);

    // COM_STMT_CLOSE has no response.
    let mut close = vec![COM_STMT_CLOSE];
    close.extend_from_slice(&id.to_le_bytes());
    client.write_packet(&Packet::new(0, close));

    // Executing the now-closed statement must yield an ERR, and the
    // connection must survive it.
    send_execute(&mut client, id, &[Param::Int(2)]);
    expect_err(&mut client);

    text_query(&mut client, "INSERT INTO t VALUES (99)");
}

#[test]
fn stmt_reset_acks_ok_for_a_known_statement() {
    let server = TestServer::start(test_config());
    let mut client = connect(&server);
    let (id, _) = prepare(&mut client, "SELECT 1");

    let mut reset = vec![COM_STMT_RESET];
    reset.extend_from_slice(&id.to_le_bytes());
    client.write_packet(&Packet::new(0, reset));
    expect_ok(&mut client);
}

/// Panic-audit regression: a `COM_STMT_EXECUTE` whose parameter section is
/// truncated mid-type used to overrun a slice index and panic the connection
/// task. It must now be a clean ERR, with the connection still usable.
#[test]
fn malformed_execute_payload_yields_err_not_a_crash() {
    let server = TestServer::start(test_config());
    let mut client = connect(&server);
    text_query(&mut client, "CREATE TABLE t (id INT PRIMARY KEY)");
    let (id, num_params) = prepare(&mut client, "SELECT id FROM t WHERE id = ?");
    assert_eq!(num_params, 1);

    // Hand-craft a truncated COM_STMT_EXECUTE: header + null bitmap +
    // new_params_bound + a type byte, but NOT the unsigned-flag byte or any
    // value. This is the exact shape that previously overran `pos`.
    let mut payload = vec![COM_STMT_EXECUTE];
    payload.extend_from_slice(&id.to_le_bytes());
    payload.push(0); // flags
    payload.extend_from_slice(&1u32.to_le_bytes()); // iteration count
    payload.push(0x00); // null bitmap (1 byte for 1 param)
    payload.push(1); // new_params_bound
    payload.push(MYSQL_TYPE_LONGLONG); // type byte, then abruptly truncated
    client.write_packet(&Packet::new(0, payload));

    expect_err(&mut client);

    // The server survived; a normal query on the same connection still works.
    text_query(&mut client, "INSERT INTO t VALUES (7)");
}

#[test]
fn prepared_select_without_from_returns_a_binary_row() {
    let server = TestServer::start(test_config());
    let mut client = connect(&server);

    let (id, params) = prepare(&mut client, "SELECT 1");
    assert_eq!(params, 0);
    send_execute(&mut client, id, &[]);
    let (types, rows) = read_binary_result_set(&mut client);
    assert_eq!(types, vec![MYSQL_TYPE_LONGLONG]);
    assert_eq!(rows, vec![vec![BinCell::Int(1)]]);
}
