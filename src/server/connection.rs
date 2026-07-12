//! Per-connection state and lifecycle.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;

use crate::auth::{AuthOutcome, AuthPlugin, Authenticator};
use crate::config::Config;
use crate::observability::{LogLevel, Observability};
use crate::protocol::capabilities::{CLIENT_DEPRECATE_EOF, CLIENT_MULTI_STATEMENTS, CLIENT_SSL};
use crate::protocol::command::{
    COM_PING, COM_QUERY, COM_QUIT, COM_STMT_CLOSE, COM_STMT_EXECUTE, COM_STMT_PREPARE,
    COM_STMT_RESET,
};
use crate::protocol::handshake::generate_scramble;
use crate::protocol::{
    parse_execute_params, AuthMoreData, AuthSwitchRequest, Cell, ColumnDefinition, ColumnType,
    ErrPacket, Handshake, HandshakeResponse41, OkPacket, Packet, ResultSet, StmtPrepareOk,
    SCRAMBLE_LEN,
};
use crate::query::executor::{Executor, QueryResult, SystemVariables};
use crate::query::parser::{self, Expr, Statement};
use crate::server::tls::{ConnStream, PrefixedStream};
use crate::storage::{
    ColumnSchema, ColumnType as StorageColumnType, InMemoryStorage, Transaction, Value,
};
use crate::{Error, Result};

/// `SERVER_STATUS_AUTOCOMMIT` — the baseline status flag on every OK/EOF.
const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;
/// `SERVER_MORE_RESULTS_EXISTS` — another result set follows (multi-statement).
const SERVER_MORE_RESULTS_EXISTS: u16 = 0x0008;

/// A statement prepared via `COM_STMT_PREPARE`, awaiting `COM_STMT_EXECUTE`.
struct PreparedStatement {
    /// The parsed statement, still holding `?` placeholders.
    statement: Statement,
    /// How many `?` parameters the client must supply on execute.
    param_count: usize,
}

/// Read chunk size for filling the packet buffer from the socket.
const READ_CHUNK: usize = 4096;

/// Represents a single connected client for its whole lifetime.
pub struct Connection {
    stream: ConnStream,
    /// `Some` if the server offers TLS: used to advertise `CLIENT_SSL` and to
    /// upgrade the connection when the client sends an SSLRequest.
    tls_acceptor: Option<TlsAcceptor>,
    /// Server system variables surfaced to `SELECT @@...` (and the version
    /// string used in the handshake). Built once from `Config`.
    system_variables: SystemVariables,
    connection_id: u32,
    /// Packet sequence id; increments by one for every packet sent or
    /// received during an exchange (see `protocol::packet`). Reset to 0 at
    /// the start of each new command (see `run_command_loop`).
    sequence_id: u8,
    /// Bytes read from the socket but not yet consumed as a full packet.
    /// Persisted across reads so any bytes the client pipelines ahead of a
    /// response are not lost.
    read_buf: Vec<u8>,
    /// Reused across every response so a multi-packet result set is one
    /// `write_all` + one `flush` instead of one pair per packet
    /// (PERFORMANCE_DURABILITY_PLAN.md P2): `write_packet` and `send_result`
    /// both encode into this buffer (clearing it first — `Vec::clear` keeps
    /// the allocation, so a connection that has sent one large result set
    /// doesn't pay to reallocate for the next one).
    out_buf: Vec<u8>,
    /// The scramble sent to the client for the auth exchange currently in
    /// progress (replaced if an auth-switch happens).
    scramble: [u8; SCRAMBLE_LEN],
    /// The auth plugin advertised in the handshake (see
    /// `Config::default_auth_plugin`). Accounts using the other plugin are
    /// moved onto theirs via an auth-switch.
    default_auth_plugin: AuthPlugin,
    authenticator: Authenticator,
    /// The client's negotiated capability flags, set once authentication
    /// succeeds; used to pick result-set framing (e.g. `CLIENT_DEPRECATE_EOF`).
    client_capabilities: u32,
    /// Shared with every other connection on this server (see
    /// `server::Server::serve`) — this is what makes concurrent clients see
    /// each other's writes.
    storage: Arc<InMemoryStorage>,
    /// `Some` between `BEGIN`/`START TRANSACTION` and the matching
    /// `COMMIT`/`ROLLBACK`; `None` means autocommit (the default).
    transaction: Option<Transaction>,
    /// Statements prepared on this connection, keyed by statement id.
    /// Connection-scoped (as in MySQL): dropped when the connection closes.
    prepared: HashMap<u32, PreparedStatement>,
    /// Source of the next prepared-statement id.
    next_statement_id: u32,
    /// Reject any packet whose declared payload exceeds this many bytes,
    /// before buffering it (see `Config::max_allowed_packet`).
    max_allowed_packet: usize,
    /// Shared logging + metrics (see `observability`).
    observability: Arc<Observability>,
}

impl Connection {
    /// Wrap a freshly accepted TCP stream.
    pub fn new(
        stream: TcpStream,
        config: &Config,
        connection_id: u32,
        storage: Arc<InMemoryStorage>,
        observability: Arc<Observability>,
    ) -> Self {
        Connection {
            stream: ConnStream::Plain(stream),
            tls_acceptor: config.tls.as_ref().map(|t| t.acceptor().clone()),
            system_variables: SystemVariables::new(
                config.server_version.clone(),
                config.max_allowed_packet as u64,
            ),
            connection_id,
            sequence_id: 0,
            read_buf: Vec::new(),
            out_buf: Vec::new(),
            scramble: [0u8; SCRAMBLE_LEN],
            default_auth_plugin: config.default_auth_plugin,
            authenticator: Authenticator::new(&config.users),
            client_capabilities: 0,
            storage,
            transaction: None,
            prepared: HashMap::new(),
            next_statement_id: 1,
            max_allowed_packet: config.max_allowed_packet,
            observability,
        }
    }

    /// Drive the connection through its lifecycle: handshake, authenticate,
    /// then (if authenticated) the command phase (`COM_QUERY`, `COM_QUIT`, ...).
    pub async fn handle(&mut self) -> Result<()> {
        let response = self.perform_handshake().await?;
        let outcome = self.authenticate(&response).await?;
        if outcome == AuthOutcome::Ok {
            self.client_capabilities = response.capability_flags;
            self.run_command_loop().await?;
        }
        Ok(())
    }

    /// Send the server's `HandshakeV10` and read the client's response. If
    /// the server offers TLS and the client's response is an SSLRequest
    /// (`CLIENT_SSL` set), upgrade the connection to TLS and read the real
    /// `HandshakeResponse41` over the encrypted channel. Returns the parsed
    /// response for the caller to authenticate.
    pub async fn perform_handshake(&mut self) -> Result<HandshakeResponse41> {
        let handshake = Handshake::new(
            self.connection_id,
            self.system_variables.version.clone(),
            self.tls_acceptor.is_some(),
            self.default_auth_plugin.wire_name(),
        );
        self.scramble = handshake.auth_plugin_data;
        let packet = handshake.to_packet(self.sequence_id)?;
        self.write_packet(&packet).await?;
        self.sequence_id = self.sequence_id.wrapping_add(1);

        let mut response_packet = self.read_handshake_packet().await?;

        // An SSLRequest carries `CLIENT_SSL` in its capability flags and is
        // sent *before* the full response. Upgrade, then read the real
        // response over TLS.
        if self.tls_acceptor.is_some() && response_has_ssl_flag(&response_packet.payload) {
            self.upgrade_to_tls().await?;
            response_packet = self.read_handshake_packet().await?;
        }

        HandshakeResponse41::parse(&response_packet.payload)
    }

    /// Read one handshake-phase packet and advance the sequence id, verifying
    /// it matches the expected next id.
    async fn read_handshake_packet(&mut self) -> Result<Packet> {
        let packet = self.read_packet().await?;
        if packet.sequence_id != self.sequence_id {
            return Err(Error::Protocol(format!(
                "expected sequence id {}, got {}",
                self.sequence_id, packet.sequence_id
            )));
        }
        self.sequence_id = self.sequence_id.wrapping_add(1);
        Ok(packet)
    }

    /// Perform the TLS handshake, replacing the plain stream with a TLS one.
    /// Any bytes already buffered (e.g. a pipelined ClientHello) are replayed
    /// into the TLS handshake via `PrefixedStream` so none are lost.
    async fn upgrade_to_tls(&mut self) -> Result<()> {
        let acceptor = self.tls_acceptor.clone().ok_or_else(|| {
            Error::Protocol("TLS upgrade requested but TLS is not configured".to_string())
        })?;

        let plain = match std::mem::replace(&mut self.stream, ConnStream::Upgrading) {
            ConnStream::Plain(tcp) => tcp,
            other => {
                self.stream = other;
                return Err(Error::Protocol(
                    "TLS upgrade attempted on an already-upgraded connection".to_string(),
                ));
            }
        };

        let prefix = std::mem::take(&mut self.read_buf);
        let prefixed = PrefixedStream::new(prefix, plain);
        let tls = acceptor
            .accept(prefixed)
            .await
            .map_err(|e| Error::Protocol(format!("TLS handshake failed: {e}")))?;
        self.stream = ConnStream::Tls(Box::new(tls));
        Ok(())
    }

    /// Authenticate the client and send the verdict.
    ///
    /// The client computed its handshake auth-response with the plugin the
    /// server advertised (or one it declared). If that isn't the plugin the
    /// account is configured for, send an `AuthSwitchRequest` with a fresh
    /// scramble and read the recomputed response. Then verify: on success send
    /// an OK packet (preceded, for `caching_sha2_password`, by an
    /// `AuthMoreData` fast-auth-success signal); on failure send an ERR.
    pub async fn authenticate(&mut self, response: &HandshakeResponse41) -> Result<AuthOutcome> {
        // The plugin the client used for its handshake response: what it
        // declared, or — if it declared nothing or a plugin we don't know —
        // the one the server advertised as its default.
        let client_plugin = response
            .auth_plugin_name
            .as_deref()
            .and_then(AuthPlugin::from_wire_name)
            .unwrap_or(self.default_auth_plugin);

        // The account's configured plugin. For an unknown user, pretend it
        // matches the client's so no switch happens and verification fails
        // closed — this avoids leaking whether the account exists.
        let target_plugin = self
            .authenticator
            .plugin_for(&response.username)
            .unwrap_or(client_plugin);

        let auth_response = if client_plugin == target_plugin {
            response.auth_response.clone()
        } else {
            self.scramble = generate_scramble();
            let switch = AuthSwitchRequest {
                plugin_name: target_plugin.wire_name().to_string(),
                auth_plugin_data: self.scramble.to_vec(),
            };
            self.write_packet(&switch.to_packet(self.sequence_id)?)
                .await?;
            self.sequence_id = self.sequence_id.wrapping_add(1);

            let switch_response = self.read_packet().await?;
            if switch_response.sequence_id != self.sequence_id {
                return Err(Error::Protocol(format!(
                    "expected sequence id {}, got {}",
                    self.sequence_id, switch_response.sequence_id
                )));
            }
            self.sequence_id = self.sequence_id.wrapping_add(1);
            switch_response.payload
        };

        let outcome = self.authenticator.authenticate(
            &response.username,
            target_plugin,
            &auth_response,
            &self.scramble,
        );

        match outcome {
            AuthOutcome::Ok => {
                // caching_sha2_password precedes the terminal OK with a
                // fast-auth-success signal, as a real 8.0 server does on a
                // cache hit.
                if target_plugin == AuthPlugin::CachingSha2Password {
                    self.write_packet(
                        &AuthMoreData::fast_auth_success().to_packet(self.sequence_id)?,
                    )
                    .await?;
                    self.sequence_id = self.sequence_id.wrapping_add(1);
                }
                self.write_packet(&OkPacket::new().to_packet(self.sequence_id)?)
                    .await?;
            }
            AuthOutcome::Denied => {
                let err = ErrPacket::access_denied(&response.username);
                self.write_packet(&err.to_packet(self.sequence_id)?).await?;
            }
        }
        self.sequence_id = self.sequence_id.wrapping_add(1);

        Ok(outcome)
    }

    /// Read and dispatch `COM_*` packets until the client disconnects
    /// (`COM_QUIT`) or an unrecoverable protocol error occurs. Each command
    /// is its own packet sequence, starting back at 0.
    async fn run_command_loop(&mut self) -> Result<()> {
        loop {
            let packet = self.read_packet().await?;
            if packet.sequence_id != 0 {
                return Err(Error::Protocol(format!(
                    "expected a fresh command sequence starting at 0, got {}",
                    packet.sequence_id
                )));
            }
            self.sequence_id = 1;

            let Some(&command) = packet.payload.first() else {
                return Err(Error::Protocol("empty command packet".to_string()));
            };

            match command {
                COM_QUIT => return Ok(()),
                COM_PING => {
                    self.write_packet(&OkPacket::new().to_packet(self.sequence_id)?)
                        .await?;
                }
                COM_QUERY => {
                    let sql = std::str::from_utf8(&packet.payload[1..]).map_err(|_| {
                        Error::Protocol("COM_QUERY: payload is not valid UTF-8".to_string())
                    })?;
                    self.handle_query(sql).await?;
                }
                COM_STMT_PREPARE => self.handle_stmt_prepare(&packet.payload).await?,
                COM_STMT_EXECUTE => self.handle_stmt_execute(&packet.payload).await?,
                COM_STMT_CLOSE => self.handle_stmt_close(&packet.payload),
                COM_STMT_RESET => self.handle_stmt_reset(&packet.payload).await?,
                other => {
                    let err = ErrPacket::new(1047, b"08S01", format!("Unknown command {other:#x}"));
                    self.write_packet(&err.to_packet(self.sequence_id)?).await?;
                }
            }
        }
    }

    /// Parse and execute a text query — one statement, or several
    /// `;`-separated statements when the client negotiated
    /// `CLIENT_MULTI_STATEMENTS`. Each statement's outcome is sent in turn,
    /// with `SERVER_MORE_RESULTS_EXISTS` set on all but the last so the
    /// client knows to read another result set. A failing statement aborts
    /// the batch (an ERR is sent and the rest are skipped), matching MySQL.
    /// A query-level error never drops the connection.
    async fn handle_query(&mut self, sql: &str) -> Result<()> {
        let statements = match parser::parse_many(sql) {
            Ok(statements) => statements,
            Err(e) => return self.send_result(Err(e), false, false, Some(sql)).await,
        };

        if statements.len() > 1 && self.client_capabilities & CLIENT_MULTI_STATEMENTS == 0 {
            let err = Error::Parse(
                "multiple statements in one query require CLIENT_MULTI_STATEMENTS".to_string(),
            );
            return self.send_result(Err(err), false, false, Some(sql)).await;
        }

        let total = statements.len();
        for (i, statement) in statements.into_iter().enumerate() {
            let outcome = self.execute_statement(statement).await;
            let failed = outcome.is_err();
            // "More results follow" only if this one succeeded and another
            // remains — an error ends the batch.
            let more_results = !failed && i + 1 < total;
            self.send_result(outcome, false, more_results, Some(sql))
                .await?;
            if failed {
                break;
            }
        }
        Ok(())
    }

    /// Send a statement's outcome to the client: an OK packet (DDL/DML/
    /// transaction control — no columns), a result set (`SELECT`, in text or
    /// binary form per `binary`), or an ERR packet on failure. `more_results`
    /// sets `SERVER_MORE_RESULTS_EXISTS` in the status flags so the client
    /// reads a following result set (multi-statement). `sql` is the original
    /// query text, included in the debug log on failure (`None` from the
    /// prepared-statement path, which doesn't retain it). A query-level error
    /// is reported to the client without dropping the connection.
    async fn send_result(
        &mut self,
        outcome: Result<QueryResult>,
        binary: bool,
        more_results: bool,
        sql: Option<&str>,
    ) -> Result<()> {
        let status_flags = if more_results {
            SERVER_STATUS_AUTOCOMMIT | SERVER_MORE_RESULTS_EXISTS
        } else {
            SERVER_STATUS_AUTOCOMMIT
        };

        match outcome {
            Ok(result) if result.columns.is_empty() => {
                self.observability.metrics.query_executed();
                let ok = OkPacket {
                    affected_rows: result.rows_affected,
                    status_flags,
                    ..OkPacket::new()
                };
                self.write_packet(&ok.to_packet(self.sequence_id)?).await?;
                self.sequence_id = self.sequence_id.wrapping_add(1);
            }
            Ok(result) => {
                self.observability.metrics.query_executed();
                let result_set = ResultSet {
                    columns: result.columns.iter().map(column_definition).collect(),
                    rows: result
                        .rows
                        .into_iter()
                        .map(|row| row.into_iter().map(value_to_cell).collect())
                        .collect(),
                };
                let deprecate_eof = self.client_capabilities & CLIENT_DEPRECATE_EOF != 0;
                self.out_buf.clear();
                let next_seq = if binary {
                    result_set.encode_binary_into(
                        &mut self.out_buf,
                        deprecate_eof,
                        status_flags,
                        self.sequence_id,
                    )?
                } else {
                    result_set.encode_text_into(
                        &mut self.out_buf,
                        deprecate_eof,
                        status_flags,
                        self.sequence_id,
                    )?
                };
                self.flush_out_buf().await?;
                self.sequence_id = next_seq;
            }
            Err(e) => {
                self.observability.metrics.error();
                // Truncate defensively: a client can send an arbitrarily long
                // query, and this is a log line, not a buffer we need to
                // preserve exactly.
                const MAX_LOGGED_SQL: usize = 500;
                let truncated_sql = sql.map(|s| match s.char_indices().nth(MAX_LOGGED_SQL) {
                    Some((byte_idx, _)) => format!("{}…", &s[..byte_idx]),
                    None => s.to_string(),
                });
                let mut fields = vec![
                    ("connection_id", self.connection_id.to_string()),
                    ("error", e.to_string()),
                ];
                if let Some(sql) = &truncated_sql {
                    fields.push(("sql", sql.clone()));
                }
                let field_refs: Vec<(&str, &str)> =
                    fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
                self.observability
                    .logger
                    .log(LogLevel::Debug, "query_error", &field_refs);
                self.write_packet(&ErrPacket::from_error(&e).to_packet(self.sequence_id)?)
                    .await?;
                self.sequence_id = self.sequence_id.wrapping_add(1);
            }
        }
        Ok(())
    }

    /// Run an already-parsed statement: transaction-control statements are
    /// handled directly (they change what `self.transaction` subsequent
    /// statements use); everything else goes through an `Executor` against
    /// either the shared storage (autocommit) or this connection's
    /// `Transaction` overlay. A write's target table is locked first — for
    /// the duration of just this statement in autocommit, or for the whole
    /// transaction's lifetime otherwise — so no other writer can interleave
    /// with it (see `InMemoryStorage::lock_table`).
    async fn execute_statement(&mut self, statement: Statement) -> Result<QueryResult> {
        match &statement {
            Statement::Begin => return self.begin_transaction().await,
            Statement::Commit => return self.commit_transaction().await,
            Statement::Rollback => return self.rollback_transaction().await,
            _ => {}
        }

        let insert_table = match &statement {
            Statement::Insert { table, .. } => Some(table.clone()),
            _ => None,
        };
        let _autocommit_guard = match (&self.transaction, &insert_table) {
            (Some(tx), Some(table)) => {
                tx.ensure_locked(table).await?;
                None
            }
            (None, Some(table)) => Some(self.storage.lock_table(table).await?),
            _ => None,
        };

        match &self.transaction {
            Some(tx) => {
                Executor::new(tx, &self.system_variables)
                    .execute(statement)
                    .await
            }
            None => {
                Executor::new(self.storage.as_ref(), &self.system_variables)
                    .execute(statement)
                    .await
            }
        }
    }

    /// `COM_STMT_PREPARE`: parse the statement (allowing `?` placeholders),
    /// store it, and reply with `COM_STMT_PREPARE_OK` plus one column
    /// definition per parameter. We report `num_columns = 0` and let the
    /// client read the actual result columns from the `COM_STMT_EXECUTE`
    /// response — a widely-supported approach that avoids resolving a
    /// statement's output shape before it runs.
    async fn handle_stmt_prepare(&mut self, payload: &[u8]) -> Result<()> {
        let sql = match std::str::from_utf8(&payload[1..]) {
            Ok(sql) => sql,
            Err(_) => {
                return self
                    .send_stmt_error(&Error::Protocol(
                        "COM_STMT_PREPARE: payload is not valid UTF-8".to_string(),
                    ))
                    .await;
            }
        };

        let (statement, param_count) = match parser::parse_prepared(sql) {
            Ok(parsed) => parsed,
            Err(e) => return self.send_stmt_error(&e).await,
        };

        let statement_id = self.next_statement_id;
        self.next_statement_id = self.next_statement_id.wrapping_add(1);
        self.prepared.insert(
            statement_id,
            PreparedStatement {
                statement,
                param_count,
            },
        );

        let ok = StmtPrepareOk {
            statement_id,
            num_columns: 0,
            num_params: param_count as u16,
        };
        self.write_packet(&ok.to_packet(self.sequence_id)?).await?;
        self.sequence_id = self.sequence_id.wrapping_add(1);

        if param_count > 0 {
            // One placeholder column definition per parameter, then an EOF
            // under classic framing.
            for _ in 0..param_count {
                let def = ColumnDefinition::new("?", ColumnType::VarString);
                self.write_packet(&def.to_packet(self.sequence_id)?).await?;
                self.sequence_id = self.sequence_id.wrapping_add(1);
            }
            if self.client_capabilities & CLIENT_DEPRECATE_EOF == 0 {
                self.write_packet(&eof_packet(self.sequence_id)).await?;
                self.sequence_id = self.sequence_id.wrapping_add(1);
            }
        }
        Ok(())
    }

    /// `COM_STMT_EXECUTE`: decode the bound parameters, substitute them into
    /// the prepared statement, run it, and reply with a binary-protocol
    /// result set (or OK). Unknown statement ids and bad parameters become
    /// ERR packets, not dropped connections.
    async fn handle_stmt_execute(&mut self, payload: &[u8]) -> Result<()> {
        if payload.len() < 5 {
            return self
                .send_stmt_error(&Error::Protocol(
                    "COM_STMT_EXECUTE: truncated header".to_string(),
                ))
                .await;
        }
        let statement_id = u32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]);

        let param_count = match self.prepared.get(&statement_id) {
            Some(p) => p.param_count,
            None => {
                return self
                    .send_stmt_error(&Error::Execution(format!(
                    "Unknown prepared statement handler ({statement_id}) given to COM_STMT_EXECUTE"
                )))
                    .await;
            }
        };

        let outcome = match parse_execute_params(payload, param_count) {
            Ok(params) => {
                let exprs: Vec<Expr> = params.into_iter().map(cell_to_expr).collect();
                let template = &self.prepared[&statement_id].statement;
                match parser::bind_parameters(template.clone(), &exprs) {
                    Ok(bound) => self.execute_statement(bound).await,
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        };

        // Prepared statements execute one at a time — never part of a
        // multi-statement batch — so there are never more results to follow.
        self.send_result(outcome, true, false, None).await
    }

    /// `COM_STMT_CLOSE`: deallocate the statement. No response, per protocol.
    fn handle_stmt_close(&mut self, payload: &[u8]) {
        if payload.len() >= 5 {
            let statement_id = u32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]);
            self.prepared.remove(&statement_id);
        }
    }

    /// `COM_STMT_RESET`: our prepared statements hold no per-execution state
    /// (no long-data parameters), so a reset is a no-op that just acks OK.
    async fn handle_stmt_reset(&mut self, payload: &[u8]) -> Result<()> {
        let known = payload.len() >= 5 && {
            let id = u32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]);
            self.prepared.contains_key(&id)
        };
        if known {
            self.write_packet(&OkPacket::new().to_packet(self.sequence_id)?)
                .await?;
        } else {
            let err = ErrPacket::new(
                1243,
                b"HY000",
                "Unknown prepared statement handler given to COM_STMT_RESET",
            );
            self.write_packet(&err.to_packet(self.sequence_id)?).await?;
        }
        self.sequence_id = self.sequence_id.wrapping_add(1);
        Ok(())
    }

    /// Send an ERR packet for a prepared-statement command failure.
    async fn send_stmt_error(&mut self, err: &Error) -> Result<()> {
        self.write_packet(&ErrPacket::from_error(err).to_packet(self.sequence_id)?)
            .await?;
        self.sequence_id = self.sequence_id.wrapping_add(1);
        Ok(())
    }

    /// `BEGIN` / `START TRANSACTION`. Starting one while another is already
    /// open implicitly commits the old one first (matches real MySQL).
    async fn begin_transaction(&mut self) -> Result<QueryResult> {
        if let Some(old) = self.transaction.take() {
            old.commit().await?;
        }
        self.transaction = Some(Transaction::new(Arc::clone(&self.storage)));
        Ok(QueryResult::default())
    }

    async fn commit_transaction(&mut self) -> Result<QueryResult> {
        if let Some(tx) = self.transaction.take() {
            tx.commit().await?;
        }
        Ok(QueryResult::default())
    }

    async fn rollback_transaction(&mut self) -> Result<QueryResult> {
        if let Some(tx) = self.transaction.take() {
            tx.rollback();
        }
        Ok(QueryResult::default())
    }

    async fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.out_buf.clear();
        packet.encode_into(&mut self.out_buf);
        self.flush_out_buf().await
    }

    /// Write `self.out_buf`'s current contents (however many packets the
    /// caller encoded into it) to the socket in one `write_all` + one
    /// `flush`, then leave the buffer as-is for the next caller to `clear`.
    async fn flush_out_buf(&mut self) -> Result<()> {
        self.stream.write_all(&self.out_buf).await?;
        // Flush so the bytes actually leave — a TLS stream buffers plaintext
        // until flushed (a no-op cost for plain TCP).
        self.stream.flush().await?;
        Ok(())
    }

    async fn read_packet(&mut self) -> Result<Packet> {
        let mut chunk = [0u8; READ_CHUNK];
        loop {
            // Enforce `max_allowed_packet` as soon as the 4-byte header is
            // available — before buffering the (possibly huge) payload — so a
            // client can't force the server to allocate an oversized packet.
            if let Some(declared) = declared_payload_len(&self.read_buf) {
                if declared > self.max_allowed_packet {
                    return Err(Error::Protocol(format!(
                        "packet payload of {declared} bytes exceeds max_allowed_packet ({})",
                        self.max_allowed_packet
                    )));
                }
            }

            if let Some((packet, consumed)) = Packet::parse(&self.read_buf)? {
                self.read_buf.drain(..consumed);
                return Ok(packet);
            }
            let n = self.stream.read(&mut chunk).await?;
            if n == 0 {
                return Err(Error::Protocol(
                    "connection closed before a full packet was received".to_string(),
                ));
            }
            self.read_buf.extend_from_slice(&chunk[..n]);
        }
    }
}

/// The declared payload length from a packet header, once at least the
/// 4-byte header has been buffered (else `None`). The 3-byte little-endian
/// length lets us reject an oversized packet before reading its payload.
fn declared_payload_len(buf: &[u8]) -> Option<usize> {
    (buf.len() >= 4).then(|| u32::from_le_bytes([buf[0], buf[1], buf[2], 0]) as usize)
}

/// Whether a handshake-response payload has `CLIENT_SSL` set in its leading
/// 4-byte capability flags — i.e. it's an SSLRequest asking to upgrade to
/// TLS. A too-short payload can't be an SSLRequest, so returns `false`.
fn response_has_ssl_flag(payload: &[u8]) -> bool {
    payload.len() >= 4
        && u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) & CLIENT_SSL != 0
}

/// Map a storage column schema to its wire-protocol column definition,
/// reporting the correct MySQL type code per column (an INT column as
/// `LONGLONG`, everything else — `VARCHAR`, `DECIMAL`, `DATE` — as
/// `VAR_STRING`). `DECIMAL`/`DATE` values are always sent as text (see
/// `value_to_cell`): `VAR_STRING` is a legitimate, universally-supported wire
/// representation for them, and it lets both types reuse the existing
/// text/binary row encoding exactly (`protocol::resultset`) with no new
/// type-specific binary layout to hand-roll and get subtly wrong.
fn column_definition(schema: &ColumnSchema) -> ColumnDefinition {
    let column_type = match schema.column_type {
        StorageColumnType::Int => ColumnType::LongLong,
        StorageColumnType::Varchar | StorageColumnType::Decimal(_) | StorageColumnType::Date => {
            ColumnType::VarString
        }
    };
    ColumnDefinition::new(schema.name.clone(), column_type)
}

/// Convert a storage value into a protocol result-set cell. `Decimal`/`Date`
/// go out as text (see `column_definition` for why that's correct, not a
/// shortcut).
fn value_to_cell(value: Value) -> Cell {
    match value {
        Value::Int(n) => Cell::Int(n),
        Value::Varchar(s) => Cell::Text(s),
        Value::Null => Cell::Null,
        decimal_or_date @ (Value::Decimal(..) | Value::Date(_)) => {
            Cell::Text(decimal_or_date.to_display_string().unwrap_or_default())
        }
    }
}

/// Convert a decoded prepared-statement parameter into the SQL literal it
/// binds to.
fn cell_to_expr(cell: Cell) -> Expr {
    match cell {
        Cell::Int(n) => Expr::Integer(n),
        Cell::Text(s) => Expr::String(s),
        Cell::Null => Expr::Null,
    }
}

/// A classic (non-`CLIENT_DEPRECATE_EOF`) EOF packet: `0xFE` + warnings +
/// status flags.
fn eof_packet(sequence_id: u8) -> Packet {
    let mut payload = vec![0xfe];
    payload.extend_from_slice(&0u16.to_le_bytes()); // warnings
    payload.extend_from_slice(&0x0002u16.to_le_bytes()); // SERVER_STATUS_AUTOCOMMIT
    Packet::new(sequence_id, payload)
}
