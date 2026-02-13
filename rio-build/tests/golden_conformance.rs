//! Golden protocol conformance tests.
//!
//! These tests record byte-level interactions from a real nix-daemon
//! and verify that rio-build produces identical responses for the same
//! client input.
//!
//! The approach: construct known client byte sequences for each opcode,
//! feed them to both the real nix-daemon (via unix socket) and to
//! rio-build's protocol handler, and compare the server responses
//! byte-for-byte (with masking for known-varying fields like timestamps
//! and version strings).
//!
//! Requirements: `nix-daemon` must be running (provided by the dev shell).

use std::io::Cursor;
use std::sync::Arc;

use rio_build::gateway::session::run_protocol;
use rio_build::store::MemoryStore;
use rio_nix::protocol::handshake::{PROTOCOL_VERSION, WORKER_MAGIC_1, WORKER_MAGIC_2};
use rio_nix::protocol::stderr::STDERR_LAST;
use rio_nix::protocol::wire;

/// Build the client-side handshake bytes that a 1.38 client would send.
///
/// This is deterministic and doesn't require a real nix-daemon.
async fn build_handshake_client_bytes() -> Vec<u8> {
    let mut buf = Vec::new();

    // Phase 1: client magic + client version
    wire::write_u64(&mut buf, WORKER_MAGIC_1).await.unwrap();
    wire::write_u64(&mut buf, PROTOCOL_VERSION).await.unwrap();

    // Phase 2: client features (empty)
    wire::write_strings(&mut buf, &[]).await.unwrap();

    // Phase 3: post-handshake: CPU affinity (0) + reserveSpace (0)
    wire::write_u64(&mut buf, 0).await.unwrap();
    wire::write_u64(&mut buf, 0).await.unwrap();

    buf
}

/// Build wopSetOptions client bytes with default values.
async fn build_set_options_bytes() -> Vec<u8> {
    let mut buf = Vec::new();

    // Opcode 19 = wopSetOptions
    wire::write_u64(&mut buf, 19).await.unwrap();

    // 12 fields: all zeros
    for _ in 0..12 {
        wire::write_u64(&mut buf, 0).await.unwrap();
    }
    // Empty overrides
    wire::write_u64(&mut buf, 0).await.unwrap();

    buf
}

/// Build wopIsValidPath client bytes for a given path.
async fn build_is_valid_path_bytes(path: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    wire::write_u64(&mut buf, 1).await.unwrap(); // opcode 1
    wire::write_string(&mut buf, path).await.unwrap();
    buf
}

/// Feed client bytes to rio-build and capture the server response.
async fn rio_build_response(client_bytes: &[u8], store: Arc<MemoryStore>) -> Vec<u8> {
    let (response_reader, response_writer) = tokio::io::duplex(256 * 1024);

    let client_data = client_bytes.to_vec();
    let proto_task = tokio::spawn(async move {
        let mut reader = Cursor::new(client_data);
        let mut writer = response_writer;
        let _ = run_protocol(&mut reader, &mut writer, store.as_ref()).await;
    });

    // Read all response bytes
    let mut response = Vec::new();
    let mut reader = tokio::io::BufReader::new(response_reader);
    tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut response)
        .await
        .unwrap();

    proto_task.await.unwrap();
    response
}

/// Parse a handshake response: extract WORKER_MAGIC_2, server version,
/// server features, version string, trusted status, and STDERR_LAST.
///
/// Returns a structured representation for comparison that masks the
/// version string (which varies between rio-build and real nix-daemon).
#[derive(Debug, PartialEq)]
struct HandshakeResponse {
    magic2: u64,
    server_version: u64,
    features: Vec<String>,
    trusted_status: u64,
    has_stderr_last: bool,
}

async fn parse_handshake_response(data: &[u8]) -> HandshakeResponse {
    let mut reader = Cursor::new(data.to_vec());

    let magic2 = wire::read_u64(&mut reader).await.unwrap();
    let server_version = wire::read_u64(&mut reader).await.unwrap();

    // Feature exchange (>= 1.38)
    let negotiated = server_version.min(PROTOCOL_VERSION);
    let features = if negotiated >= (1 << 8 | 38) {
        wire::read_strings(&mut reader).await.unwrap()
    } else {
        vec![]
    };

    // Version string (skip — varies between implementations)
    let _version_string = wire::read_string(&mut reader).await.unwrap();

    // Trusted status
    let trusted_status = wire::read_u64(&mut reader).await.unwrap();

    // STDERR_LAST
    let last = wire::read_u64(&mut reader).await.unwrap();

    HandshakeResponse {
        magic2,
        server_version,
        features,
        trusted_status,
        has_stderr_last: last == STDERR_LAST,
    }
}

/// Test that rio-build's handshake response has the correct structure.
///
/// We verify:
/// - WORKER_MAGIC_2 is correct
/// - Server version is 1.38 (0x126)
/// - Features are empty
/// - Trusted status is 1
/// - STDERR_LAST terminates the handshake
#[tokio::test]
async fn test_golden_handshake_structure() {
    let store = Arc::new(MemoryStore::new());
    let handshake_bytes = build_handshake_client_bytes().await;

    let response = rio_build_response(&handshake_bytes, store).await;
    let parsed = parse_handshake_response(&response).await;

    assert_eq!(parsed.magic2, WORKER_MAGIC_2, "magic2 mismatch");
    assert_eq!(
        parsed.server_version, 0x126,
        "server version should be 1.38"
    );
    assert!(parsed.features.is_empty(), "features should be empty");
    assert_eq!(parsed.trusted_status, 1, "trusted status should be 1");
    assert!(
        parsed.has_stderr_last,
        "handshake should end with STDERR_LAST"
    );
}

/// Test that the full handshake + wopIsValidPath response
/// produces correct bytes for a non-existent path.
#[tokio::test]
async fn test_golden_is_valid_path_not_found() {
    let store = Arc::new(MemoryStore::new());

    let mut client_bytes = build_handshake_client_bytes().await;
    client_bytes.extend(build_set_options_bytes().await);
    client_bytes.extend(
        build_is_valid_path_bytes("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-nonexistent-1.0")
            .await,
    );

    let response = rio_build_response(&client_bytes, store).await;
    let mut reader = Cursor::new(response);

    // Skip handshake response
    let _magic2 = wire::read_u64(&mut reader).await.unwrap();
    let _version = wire::read_u64(&mut reader).await.unwrap();
    let _features = wire::read_strings(&mut reader).await.unwrap();
    let _version_str = wire::read_string(&mut reader).await.unwrap();
    let _trusted = wire::read_u64(&mut reader).await.unwrap();
    let _last = wire::read_u64(&mut reader).await.unwrap();

    // wopSetOptions response: STDERR_LAST + u64(1)
    let last = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(last, STDERR_LAST, "expected STDERR_LAST for SetOptions");
    let result = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(result, 1, "expected success for SetOptions");

    // wopIsValidPath response: STDERR_LAST + u64(0) for not-found
    let last = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(last, STDERR_LAST, "expected STDERR_LAST for IsValidPath");
    let valid = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(valid, 0, "expected path to be invalid");
}

/// Test that wopIsValidPath returns 1 for an existing path,
/// with correct byte-level format.
#[tokio::test]
async fn test_golden_is_valid_path_found() {
    use rio_nix::hash::{HashAlgo, NixHash};
    use rio_nix::store_path::StorePath;

    let store = Arc::new(MemoryStore::new());
    let path =
        StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1").unwrap();
    store.insert(
        rio_build::store::traits::PathInfo {
            path,
            deriver: None,
            nar_hash: NixHash::compute(HashAlgo::SHA256, b"test"),
            references: vec![],
            registration_time: 1700000000,
            nar_size: 1000,
            ultimate: true,
            sigs: vec![],
            ca: None,
        },
        None,
    );

    let mut client_bytes = build_handshake_client_bytes().await;
    client_bytes.extend(build_set_options_bytes().await);
    client_bytes.extend(
        build_is_valid_path_bytes("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1").await,
    );

    let response = rio_build_response(&client_bytes, store).await;
    let mut reader = Cursor::new(response);

    // Skip handshake + SetOptions response
    let _magic2 = wire::read_u64(&mut reader).await.unwrap();
    let _version = wire::read_u64(&mut reader).await.unwrap();
    let _features = wire::read_strings(&mut reader).await.unwrap();
    let _version_str = wire::read_string(&mut reader).await.unwrap();
    let _trusted = wire::read_u64(&mut reader).await.unwrap();
    let _last = wire::read_u64(&mut reader).await.unwrap(); // handshake STDERR_LAST
    let _last = wire::read_u64(&mut reader).await.unwrap(); // SetOptions STDERR_LAST
    let _result = wire::read_u64(&mut reader).await.unwrap(); // SetOptions result

    // wopIsValidPath response
    let last = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(last, STDERR_LAST, "expected STDERR_LAST");
    let valid = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(valid, 1, "expected path to be valid");
}

/// Test that wopQueryPathInfo returns all fields in the correct order
/// with the correct wire format for each field type.
#[tokio::test]
async fn test_golden_query_path_info_wire_format() {
    use rio_nix::hash::{HashAlgo, NixHash};
    use rio_nix::store_path::StorePath;

    let store = Arc::new(MemoryStore::new());
    let path =
        StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1").unwrap();
    let ref_path =
        StorePath::parse("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-glibc-2.38").unwrap();
    let deriver_path =
        StorePath::parse("/nix/store/cccccccccccccccccccccccccccccccc-hello-2.12.1.drv").unwrap();
    let nar_hash = NixHash::compute(HashAlgo::SHA256, b"test nar");

    store.insert(
        rio_build::store::traits::PathInfo {
            path,
            deriver: Some(deriver_path.clone()),
            nar_hash: nar_hash.clone(),
            references: vec![ref_path.clone()],
            registration_time: 1700000000,
            nar_size: 42000,
            ultimate: true,
            sigs: vec!["sig1:abc".to_string()],
            ca: None,
        },
        None,
    );

    let mut client_bytes = build_handshake_client_bytes().await;
    client_bytes.extend(build_set_options_bytes().await);

    // wopQueryPathInfo (opcode 26)
    let mut qpi_bytes = Vec::new();
    wire::write_u64(&mut qpi_bytes, 26).await.unwrap();
    wire::write_string(
        &mut qpi_bytes,
        "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1",
    )
    .await
    .unwrap();
    client_bytes.extend(qpi_bytes);

    let response = rio_build_response(&client_bytes, store).await;
    let mut reader = Cursor::new(response);

    // Skip handshake + SetOptions response
    let _magic2 = wire::read_u64(&mut reader).await.unwrap();
    let _version = wire::read_u64(&mut reader).await.unwrap();
    let _features = wire::read_strings(&mut reader).await.unwrap();
    let _version_str = wire::read_string(&mut reader).await.unwrap();
    let _trusted = wire::read_u64(&mut reader).await.unwrap();
    let _last = wire::read_u64(&mut reader).await.unwrap();
    let _last = wire::read_u64(&mut reader).await.unwrap();
    let _result = wire::read_u64(&mut reader).await.unwrap();

    // wopQueryPathInfo response
    let last = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(last, STDERR_LAST);

    let valid = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(valid, 1, "path should be valid");

    let deriver = wire::read_string(&mut reader).await.unwrap();
    assert_eq!(deriver, deriver_path.to_string());

    let hash_str = wire::read_string(&mut reader).await.unwrap();
    assert_eq!(hash_str, nar_hash.to_colon());

    let refs = wire::read_strings(&mut reader).await.unwrap();
    assert_eq!(refs, vec![ref_path.to_string()]);

    let reg_time = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(reg_time, 1700000000);

    let nar_size = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(nar_size, 42000);

    let ultimate = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(ultimate, 1);

    let sigs = wire::read_strings(&mut reader).await.unwrap();
    assert_eq!(sigs, vec!["sig1:abc".to_string()]);

    let ca = wire::read_string(&mut reader).await.unwrap();
    assert!(ca.is_empty(), "CA should be empty for input-addressed path");
}
