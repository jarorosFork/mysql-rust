//! Integration test for Phase 9 TLS (`CLIENT_SSL`): a real client completes
//! the MySQL STARTTLS-style upgrade — plaintext `HandshakeV10`, SSLRequest,
//! TLS handshake, then the auth response and a query over the encrypted
//! channel — using rustls on the client side and a self-signed cert
//! generated at test time with rcgen.

use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName,
};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

use mysql_rust::auth::caching_sha2;
use mysql_rust::auth::native_password::compute_auth_response;
use mysql_rust::config::{Config, TlsConfig, UserCredential};
use mysql_rust::observability::LogLevel;
use mysql_rust::protocol::capabilities::{
    CLIENT_DEPRECATE_EOF, CLIENT_PLUGIN_AUTH, CLIENT_PROTOCOL_41, CLIENT_SECURE_CONNECTION,
    CLIENT_SSL,
};
use mysql_rust::protocol::Packet;
use mysql_rust::server::Server;

/// A self-signed cert + key (DER).
struct TestCert {
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
}

fn generate_cert() -> TestCert {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("generate self-signed cert");
    let cert_der = cert.cert.der().clone();
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));
    TestCert { cert_der, key_der }
}

/// A minimal async MySQL client that reads/writes whole packets over any
/// async stream (plain TCP before the upgrade, TLS after).
struct AsyncClient<S> {
    stream: S,
    buf: Vec<u8>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncClient<S> {
    fn new(stream: S) -> Self {
        AsyncClient {
            stream,
            buf: Vec::new(),
        }
    }

    async fn read_packet(&mut self) -> Packet {
        let mut chunk = [0u8; 4096];
        loop {
            if let Some((packet, consumed)) = Packet::parse(&self.buf).expect("framing") {
                self.buf.drain(..consumed);
                return packet;
            }
            let n = self.stream.read(&mut chunk).await.expect("read");
            assert!(n > 0, "server closed before a full packet");
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    async fn write_packet(&mut self, packet: &Packet) {
        self.stream
            .write_all(&packet.encode())
            .await
            .expect("write");
        self.stream.flush().await.expect("flush");
    }
}

fn extract_scramble(payload: &[u8]) -> [u8; 20] {
    let mut pos = 1;
    let version_end = payload[pos..].iter().position(|&b| b == 0).unwrap() + pos;
    pos = version_end + 1 + 4;
    let mut scramble = [0u8; 20];
    scramble[..8].copy_from_slice(&payload[pos..pos + 8]);
    pos += 8 + 1 + 2 + 1 + 2 + 2 + 1 + 10;
    scramble[8..20].copy_from_slice(&payload[pos..pos + 12]);
    scramble
}

#[tokio::test]
async fn client_completes_tls_handshake_auth_and_query() {
    let cert = generate_cert();
    let tls = TlsConfig::from_der(vec![cert.cert_der.clone()], cert.key_der.clone_key())
        .expect("build server TLS config");

    let config = Config {
        users: vec![UserCredential::with_password("alice", "s3cret")],
        log_level: LogLevel::Error,
        tls: Some(tls),
        ..Config::default()
    };

    // Bind an ephemeral port, then serve in the background.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_task = tokio::spawn(async move {
        let server = Server::new(config);
        let _ = server
            .serve(listener, async {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    // --- Client side ---
    let tcp = TcpStream::connect(addr).await.expect("connect");
    let mut plain = AsyncClient::new(tcp);

    // 1. Read the plaintext HandshakeV10 and pull out the auth scramble. (The
    //    upgrade succeeding below is itself proof the server advertised and
    //    honored CLIENT_SSL.)
    let handshake = plain.read_packet().await;
    let scramble = extract_scramble(&handshake.payload);

    // 2. Send an SSLRequest: the 32-byte HandshakeResponse41 header with
    //    CLIENT_SSL set and NO username — this triggers the upgrade.
    let caps = CLIENT_PROTOCOL_41
        | CLIENT_PLUGIN_AUTH
        | CLIENT_SECURE_CONNECTION
        | CLIENT_DEPRECATE_EOF
        | CLIENT_SSL;
    let mut ssl_request = Vec::new();
    ssl_request.extend_from_slice(&caps.to_le_bytes());
    ssl_request.extend_from_slice(&16_777_216u32.to_le_bytes());
    ssl_request.push(45);
    ssl_request.extend_from_slice(&[0u8; 23]);
    plain.write_packet(&Packet::new(1, ssl_request)).await;

    // 3. TLS handshake over the same socket. Trust our self-signed cert.
    let mut roots = RootCertStore::empty();
    roots.add(cert.cert_der.clone()).expect("add root");
    let client_config = ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));

    assert!(
        plain.buf.is_empty(),
        "no server bytes should precede the TLS handshake"
    );
    let tcp = plain.stream;
    let domain = ServerName::try_from("localhost").expect("server name");
    let tls_stream = connector
        .connect(domain, tcp)
        .await
        .expect("client TLS handshake");
    let mut client = AsyncClient::new(tls_stream);

    // 4. Send the real HandshakeResponse41 over TLS (seq id 2), with auth.
    let auth_response = compute_auth_response(Some(b"s3cret"), &scramble);
    let mut response = Vec::new();
    response.extend_from_slice(&caps.to_le_bytes());
    response.extend_from_slice(&16_777_216u32.to_le_bytes());
    response.push(45);
    response.extend_from_slice(&[0u8; 23]);
    response.extend_from_slice(b"alice\0");
    response.push(auth_response.len() as u8);
    response.extend_from_slice(&auth_response);
    response.extend_from_slice(b"mysql_native_password\0");
    client.write_packet(&Packet::new(2, response)).await;

    // 5. Auth OK, then a query over the encrypted channel.
    let verdict = client.read_packet().await;
    assert_eq!(verdict.payload[0], 0x00, "auth should succeed over TLS");

    let mut query = vec![0x03u8]; // COM_QUERY
    query.extend_from_slice(b"SELECT 1");
    client.write_packet(&Packet::new(0, query)).await;

    let count = client.read_packet().await;
    assert_eq!(count.payload, vec![1], "one column");
    let _coldef = client.read_packet().await;
    let row = client.read_packet().await;
    assert_eq!(row.payload, vec![1, b'1'], "row value '1'");

    // Close the TLS connection so the server's connection task sees EOF and
    // finishes, letting the shutdown drain complete promptly.
    drop(client);
    shutdown_tx.send(()).ok();
    server_task.await.expect("server task");
}

/// The Phase 9 acceptance criterion: a client connects over TLS and
/// authenticates with `caching_sha2_password` — the MySQL 8.0 default — then
/// runs a query over the encrypted channel.
#[tokio::test]
async fn client_completes_tls_handshake_with_caching_sha2_password() {
    let cert = generate_cert();
    let tls = TlsConfig::from_der(vec![cert.cert_der.clone()], cert.key_der.clone_key())
        .expect("build server TLS config");

    let config = Config {
        users: vec![UserCredential::with_caching_sha2_password(
            "alice", "s3cret",
        )],
        log_level: LogLevel::Error,
        tls: Some(tls),
        ..Config::default()
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_task = tokio::spawn(async move {
        let server = Server::new(config);
        let _ = server
            .serve(listener, async {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    let tcp = TcpStream::connect(addr).await.expect("connect");
    let mut plain = AsyncClient::new(tcp);

    let handshake = plain.read_packet().await;
    let scramble = extract_scramble(&handshake.payload);

    // SSLRequest: header with CLIENT_SSL, no username, triggers the upgrade.
    let caps = CLIENT_PROTOCOL_41
        | CLIENT_PLUGIN_AUTH
        | CLIENT_SECURE_CONNECTION
        | CLIENT_DEPRECATE_EOF
        | CLIENT_SSL;
    let mut ssl_request = Vec::new();
    ssl_request.extend_from_slice(&caps.to_le_bytes());
    ssl_request.extend_from_slice(&16_777_216u32.to_le_bytes());
    ssl_request.push(45);
    ssl_request.extend_from_slice(&[0u8; 23]);
    plain.write_packet(&Packet::new(1, ssl_request)).await;

    let mut roots = RootCertStore::empty();
    roots.add(cert.cert_der.clone()).expect("add root");
    let client_config = ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));

    let tcp = plain.stream;
    let domain = ServerName::try_from("localhost").expect("server name");
    let tls_stream = connector
        .connect(domain, tcp)
        .await
        .expect("client TLS handshake");
    let mut client = AsyncClient::new(tls_stream);

    // Real HandshakeResponse41 over TLS (seq id 2), caching_sha2 fast-auth.
    let reply = caching_sha2::scramble(Some(b"s3cret"), &scramble);
    let mut response = Vec::new();
    response.extend_from_slice(&caps.to_le_bytes());
    response.extend_from_slice(&16_777_216u32.to_le_bytes());
    response.push(45);
    response.extend_from_slice(&[0u8; 23]);
    response.extend_from_slice(b"alice\0");
    response.push(reply.len() as u8);
    response.extend_from_slice(&reply);
    response.extend_from_slice(b"caching_sha2_password\0");
    client.write_packet(&Packet::new(2, response)).await;

    // Fast-auth success, then OK — both over the encrypted channel.
    let more = client.read_packet().await;
    assert_eq!(
        more.payload,
        vec![0x01, 0x03],
        "expected fast-auth success over TLS"
    );
    let ok = client.read_packet().await;
    assert_eq!(
        ok.payload[0], 0x00,
        "caching_sha2 auth should succeed over TLS"
    );

    // A query over the encrypted channel confirms the session is live.
    let mut query = vec![0x03u8]; // COM_QUERY
    query.extend_from_slice(b"SELECT 1");
    client.write_packet(&Packet::new(0, query)).await;

    let count = client.read_packet().await;
    assert_eq!(count.payload, vec![1], "one column");
    let _coldef = client.read_packet().await;
    let row = client.read_packet().await;
    assert_eq!(row.payload, vec![1, b'1'], "row value '1'");

    drop(client);
    shutdown_tx.send(()).ok();
    server_task.await.expect("server task");
}
