//! End-to-end mTLS verification: real tonic server + client + rcgen CA.
//!
//! The unit tests in `src/tls.rs` cover the LOADING logic (partial
//! config errors, missing files error, etc). This integration test
//! proves the OUTPUT of `load_server_tls` / `load_client_tls`
//! actually works in a real handshake:
//!
//! - server with `ServerTlsConfig + client_ca_root` + client with
//!   matching CA-signed cert → handshake succeeds
//! - client WITHOUT a cert → server rejects (mTLS, not just TLS)
//! - client with a cert from a DIFFERENT CA → server rejects
//!
//! Certs are generated in-memory via rcgen (no disk I/O, no external
//! PKI). This is what a real cert-manager CA Issuer does — rcgen
//! just does it inline.

use std::io::Write;

use rio_common::tls::{TlsConfig, load_client_tls, load_server_tls};
use tonic_health::pb::{HealthCheckRequest, health_client::HealthClient};

/// A test PKI: one CA + server leaf + client leaf, all in-memory.
/// Generated fresh per test — no shared state.
struct TestPki {
    ca_pem: String,
    server_cert_pem: String,
    server_key_pem: String,
    client_cert_pem: String,
    client_key_pem: String,
}

impl TestPki {
    /// Generate a CA and two leaf certs signed by it.
    ///
    /// `server_san`: goes in the server cert's subjectAltName. The
    /// client's `domain_name` must match this (or :authority must
    /// match it). Tests use "localhost" since we connect to
    /// `127.0.0.1` and set `domain_name("localhost")`.
    fn generate(server_san: &str) -> Self {
        // CA: self-signed, isCA: true. `CertifiedIssuer::self_signed`
        // builds both the CA Certificate and an `Issuer` wrapper that
        // can sign leaf certs. The wrapper derefs to `Issuer` (what
        // `signed_by` wants) and as-refs to `Certificate` (for .pem()).
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(vec![]).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        // Distinguished name: CN=test-ca. Not strictly needed
        // (rustls verifies by signature, not DN match), but
        // makes `openssl x509 -text` output readable if debugging.
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "test-ca");
        let ca = rcgen::CertifiedIssuer::self_signed(ca_params, ca_key).unwrap();

        // Server leaf: CA-signed, one SAN. `signed_by` takes
        // `&Issuer` (what CertifiedIssuer derefs to).
        let server_key = rcgen::KeyPair::generate().unwrap();
        let server_params = rcgen::CertificateParams::new(vec![server_san.to_string()]).unwrap();
        let server_cert = server_params.signed_by(&server_key, &ca).unwrap();

        // Client leaf: same CA, SAN "rio-test-client" (client
        // SAN doesn't matter for outbound verification — only
        // the signature chain does).
        let client_key = rcgen::KeyPair::generate().unwrap();
        let client_params =
            rcgen::CertificateParams::new(vec!["rio-test-client".to_string()]).unwrap();
        let client_cert = client_params.signed_by(&client_key, &ca).unwrap();

        Self {
            // CertifiedIssuer has .pem() for its own cert — that's
            // the CA PEM. Leaf certs use Certificate::pem().
            ca_pem: ca.pem(),
            server_cert_pem: server_cert.pem(),
            server_key_pem: server_key.serialize_pem(),
            client_cert_pem: client_cert.pem(),
            client_key_pem: client_key.serialize_pem(),
        }
    }

    /// Write cert+key+CA to tempfiles and return a TlsConfig for
    /// the SERVER side. The tempfiles live as long as the returned
    /// tuple (dropping them deletes the files).
    fn server_config(&self) -> (TlsConfig, [tempfile::NamedTempFile; 3]) {
        let cert = write_tmp(&self.server_cert_pem);
        let key = write_tmp(&self.server_key_pem);
        let ca = write_tmp(&self.ca_pem);
        let cfg = TlsConfig {
            cert_path: Some(cert.path().to_path_buf()),
            key_path: Some(key.path().to_path_buf()),
            ca_path: Some(ca.path().to_path_buf()),
        };
        (cfg, [cert, key, ca])
    }

    /// Same for the CLIENT side (uses client_cert/key, same CA).
    fn client_config(&self) -> (TlsConfig, [tempfile::NamedTempFile; 3]) {
        let cert = write_tmp(&self.client_cert_pem);
        let key = write_tmp(&self.client_key_pem);
        let ca = write_tmp(&self.ca_pem);
        let cfg = TlsConfig {
            cert_path: Some(cert.path().to_path_buf()),
            key_path: Some(key.path().to_path_buf()),
            ca_path: Some(ca.path().to_path_buf()),
        };
        (cfg, [cert, key, ca])
    }
}

fn write_tmp(content: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    f
}

/// Spawn a tonic server with the given TLS config on an ephemeral
/// port. Returns the bound address. Server runs until the test
/// process exits (detached spawn — fine for tests).
async fn spawn_server(server_tls: tonic::transport::ServerTlsConfig) -> std::net::SocketAddr {
    let (reporter, health_service) = tonic_health::server::health_reporter();
    reporter
        .set_serving::<tonic_health::pb::health_server::HealthServer<
            tonic_health::server::HealthService,
        >>()
        .await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .tls_config(server_tls)
            .unwrap()
            .add_service(health_service)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    addr
}

/// CryptoProvider install. Once per process — nextest runs each
/// test file in its own process, but tests within ONE file share.
/// OnceLock ensures we install exactly once regardless of test
/// execution order or concurrency.
fn ensure_crypto_provider() {
    static INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INIT.get_or_init(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

// r[verify sec.boundary.grpc-hmac]
// (shared rule with HMAC — this verifies the mTLS half)

/// Valid CA-signed client cert → handshake succeeds → RPC works.
/// This is the happy path proving the full loader + tonic wiring.
#[tokio::test]
async fn mtls_valid_client_cert_succeeds() {
    ensure_crypto_provider();
    let pki = TestPki::generate("localhost");

    let (server_cfg, _server_files) = pki.server_config();
    let server_tls = load_server_tls(&server_cfg).unwrap().unwrap();
    let addr = spawn_server(server_tls).await;

    let (client_cfg, _client_files) = pki.client_config();
    let client_tls = load_client_tls(&client_cfg, "localhost").unwrap().unwrap();

    let channel = tonic::transport::Channel::from_shared(format!("https://{addr}"))
        .unwrap()
        .tls_config(client_tls)
        .unwrap()
        .connect()
        .await
        .expect("connect with valid client cert should succeed");

    let mut client = HealthClient::new(channel);
    let resp = client
        .check(HealthCheckRequest {
            service: String::new(),
        })
        .await
        .expect("RPC over mTLS should work");
    // SERVING was set by the reporter above. This proves the
    // full round-trip: handshake → gRPC frame → handler → response.
    assert_eq!(
        resp.into_inner().status,
        tonic_health::pb::health_check_response::ServingStatus::Serving as i32
    );
}

/// Client with NO cert → server rejects. Proves mTLS (not just
/// encryption): the `.client_ca_root(ca)` call in load_server_tls
/// makes client certs REQUIRED.
#[tokio::test]
async fn mtls_no_client_cert_rejected() {
    ensure_crypto_provider();
    let pki = TestPki::generate("localhost");

    let (server_cfg, _server_files) = pki.server_config();
    let server_tls = load_server_tls(&server_cfg).unwrap().unwrap();
    let addr = spawn_server(server_tls).await;

    // Client config with CA only (so it trusts the server) but NO
    // identity (no client cert to present). This is what a K8s
    // probe looks like from the server's perspective — why we
    // need the plaintext health port.
    //
    // load_client_tls doesn't support "CA but no identity" (it
    // errors on partial config, correctly). Build ClientTlsConfig
    // directly.
    let ca = write_tmp(&pki.ca_pem);
    let client_tls = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(
            std::fs::read_to_string(ca.path()).unwrap(),
        ))
        .domain_name("localhost");

    let result = tonic::transport::Channel::from_shared(format!("https://{addr}"))
        .unwrap()
        .tls_config(client_tls)
        .unwrap()
        .connect()
        .await;

    // Connect itself might succeed (TCP + TLS handshake starts)
    // but either the handshake fails OR the first RPC fails. The
    // exact failure point depends on tonic/rustls internals —
    // what matters is SOMETHING fails before we get a successful
    // RPC response.
    match result {
        Err(_) => {
            // Handshake rejected at connect — clearest signal.
        }
        Ok(channel) => {
            // Handshake deferred to first RPC (tonic sometimes
            // lazily handshakes). The RPC should fail.
            let mut client = HealthClient::new(channel);
            let rpc_result = client
                .check(HealthCheckRequest {
                    service: String::new(),
                })
                .await;
            assert!(
                rpc_result.is_err(),
                "no client cert → server MUST reject; RPC somehow succeeded"
            );
        }
    }
}

/// Client cert signed by a DIFFERENT CA → server rejects. Proves
/// the server verifies the SIGNATURE CHAIN, not just "has a cert."
#[tokio::test]
async fn mtls_wrong_ca_client_cert_rejected() {
    ensure_crypto_provider();
    let pki = TestPki::generate("localhost");
    // A SECOND CA + client cert. Same SAN, valid cert — but the
    // server trusts pki.ca_pem, not this one.
    let rogue_pki = TestPki::generate("localhost");

    let (server_cfg, _server_files) = pki.server_config();
    let server_tls = load_server_tls(&server_cfg).unwrap().unwrap();
    let addr = spawn_server(server_tls).await;

    // Client config: TRUST the REAL CA (so client accepts the
    // server's cert — we're testing the OTHER direction), but
    // PRESENT the rogue cert.
    let rogue_cert = write_tmp(&rogue_pki.client_cert_pem);
    let rogue_key = write_tmp(&rogue_pki.client_key_pem);
    let real_ca = write_tmp(&pki.ca_pem);
    let client_cfg = TlsConfig {
        cert_path: Some(rogue_cert.path().to_path_buf()),
        key_path: Some(rogue_key.path().to_path_buf()),
        ca_path: Some(real_ca.path().to_path_buf()),
    };
    let client_tls = load_client_tls(&client_cfg, "localhost").unwrap().unwrap();

    let result = tonic::transport::Channel::from_shared(format!("https://{addr}"))
        .unwrap()
        .tls_config(client_tls)
        .unwrap()
        .connect()
        .await;

    // Same either-or as above — connect or RPC fails.
    match result {
        Err(_) => {}
        Ok(channel) => {
            let mut client = HealthClient::new(channel);
            let rpc_result = client
                .check(HealthCheckRequest {
                    service: String::new(),
                })
                .await;
            assert!(
                rpc_result.is_err(),
                "rogue-CA client cert → server MUST reject (chain verification)"
            );
        }
    }
}

/// Client with WRONG domain_name → CLIENT rejects the server's
/// cert (SAN mismatch). Tests the other direction of mTLS: the
/// client verifies the server too.
#[tokio::test]
async fn mtls_san_mismatch_rejected_by_client() {
    ensure_crypto_provider();
    let pki = TestPki::generate("localhost");

    let (server_cfg, _server_files) = pki.server_config();
    let server_tls = load_server_tls(&server_cfg).unwrap().unwrap();
    let addr = spawn_server(server_tls).await;

    let (client_cfg, _client_files) = pki.client_config();
    // Domain name WRONG — server cert has SAN "localhost", we
    // ask for "rio-wrong-name". rustls should reject.
    let client_tls = load_client_tls(&client_cfg, "rio-wrong-name")
        .unwrap()
        .unwrap();

    let result = tonic::transport::Channel::from_shared(format!("https://{addr}"))
        .unwrap()
        .tls_config(client_tls)
        .unwrap()
        .connect()
        .await;

    // This one should fail at connect (client-side verification
    // happens during handshake, not lazily).
    assert!(
        result.is_err(),
        "SAN mismatch → client should reject server cert"
    );
}
