//! Golden protocol conformance tests.
//!
//! This file contains two layers of golden tests:
//!
//! 1. **Structural tests** (existing): construct client bytes, feed them to
//!    rio-build, and verify the response has correct field values. These run
//!    without any external dependencies.
//!
//! 2. **Byte-conformance tests** (new): load recorded binary fixtures from a
//!    real nix-daemon and compare rio-build's response field-by-field against
//!    the recorded bytes. These require fixtures in `tests/golden/fixtures/`
//!    (recorded with `RECORD_GOLDEN=1`).

mod golden;

use std::io::Cursor;
use std::sync::Arc;

use rio_build::gateway::session::run_protocol;
use rio_build::store::MemoryStore;
use rio_nix::protocol::handshake::{PROTOCOL_VERSION, WORKER_MAGIC_1, WORKER_MAGIC_2};
use rio_nix::protocol::stderr::STDERR_LAST;
use rio_nix::protocol::wire;

// ============================================================================
// Helpers (used by structural tests below)
// ============================================================================

/// Build the client-side handshake bytes that a 1.38 client would send.
async fn build_handshake_client_bytes() -> Vec<u8> {
    let mut buf = Vec::new();
    wire::write_u64(&mut buf, WORKER_MAGIC_1).await.unwrap();
    wire::write_u64(&mut buf, PROTOCOL_VERSION).await.unwrap();
    wire::write_strings(&mut buf, &[]).await.unwrap();
    wire::write_u64(&mut buf, 0).await.unwrap();
    wire::write_u64(&mut buf, 0).await.unwrap();
    buf
}

/// Build wopSetOptions client bytes with default values.
async fn build_set_options_bytes() -> Vec<u8> {
    let mut buf = Vec::new();
    wire::write_u64(&mut buf, 19).await.unwrap();
    for _ in 0..12 {
        wire::write_u64(&mut buf, 0).await.unwrap();
    }
    wire::write_u64(&mut buf, 0).await.unwrap();
    buf
}

/// Build wopIsValidPath client bytes for a given path.
async fn build_is_valid_path_bytes(path: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    wire::write_u64(&mut buf, 1).await.unwrap();
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

    let mut response = Vec::new();
    let mut reader = tokio::io::BufReader::new(response_reader);
    tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut response)
        .await
        .unwrap();

    proto_task.await.unwrap();
    response
}

// ============================================================================
// Structural golden tests (no external dependencies)
// ============================================================================

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

    let negotiated = server_version.min(PROTOCOL_VERSION);
    let features = if negotiated >= (1 << 8 | 38) {
        wire::read_strings(&mut reader).await.unwrap()
    } else {
        vec![]
    };

    let _version_string = wire::read_string(&mut reader).await.unwrap();
    let trusted_status = wire::read_u64(&mut reader).await.unwrap();
    let last = wire::read_u64(&mut reader).await.unwrap();

    HandshakeResponse {
        magic2,
        server_version,
        features,
        trusted_status,
        has_stderr_last: last == STDERR_LAST,
    }
}

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

    let _magic2 = wire::read_u64(&mut reader).await.unwrap();
    let _version = wire::read_u64(&mut reader).await.unwrap();
    let _features = wire::read_strings(&mut reader).await.unwrap();
    let _version_str = wire::read_string(&mut reader).await.unwrap();
    let _trusted = wire::read_u64(&mut reader).await.unwrap();
    let _last = wire::read_u64(&mut reader).await.unwrap();

    let last = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(last, STDERR_LAST, "expected STDERR_LAST for SetOptions");

    let last = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(last, STDERR_LAST, "expected STDERR_LAST for IsValidPath");
    let valid = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(valid, 0, "expected path to be invalid");
}

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

    let _magic2 = wire::read_u64(&mut reader).await.unwrap();
    let _version = wire::read_u64(&mut reader).await.unwrap();
    let _features = wire::read_strings(&mut reader).await.unwrap();
    let _version_str = wire::read_string(&mut reader).await.unwrap();
    let _trusted = wire::read_u64(&mut reader).await.unwrap();
    let _last = wire::read_u64(&mut reader).await.unwrap(); // handshake STDERR_LAST
    let _last = wire::read_u64(&mut reader).await.unwrap(); // SetOptions STDERR_LAST

    let last = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(last, STDERR_LAST, "expected STDERR_LAST");
    let valid = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(valid, 1, "expected path to be valid");
}

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

    let _magic2 = wire::read_u64(&mut reader).await.unwrap();
    let _version = wire::read_u64(&mut reader).await.unwrap();
    let _features = wire::read_strings(&mut reader).await.unwrap();
    let _version_str = wire::read_string(&mut reader).await.unwrap();
    let _trusted = wire::read_u64(&mut reader).await.unwrap();
    let _last = wire::read_u64(&mut reader).await.unwrap(); // handshake STDERR_LAST
    let _last = wire::read_u64(&mut reader).await.unwrap(); // SetOptions STDERR_LAST

    let last = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(last, STDERR_LAST);

    let valid = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(valid, 1, "path should be valid");

    let deriver = wire::read_string(&mut reader).await.unwrap();
    assert_eq!(deriver, deriver_path.to_string());

    let hash_str = wire::read_string(&mut reader).await.unwrap();
    assert_eq!(hash_str, nar_hash.to_hex());

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

// ============================================================================
// Byte-conformance tests against recorded nix-daemon fixtures
// ============================================================================

/// Which opcode parser to use for a byte-conformance test.
enum OpcodeParser {
    None,
    IsValidPath,
    QueryPathInfo,
    QueryValidPaths,
    AddTempRoot,
}

impl OpcodeParser {
    async fn parse(&self, data: &[u8]) -> Vec<golden::ResponseField> {
        match self {
            OpcodeParser::None => vec![],
            OpcodeParser::IsValidPath => golden::parse_is_valid_path_fields(data).await,
            OpcodeParser::QueryPathInfo => golden::parse_query_path_info_fields(data).await,
            OpcodeParser::QueryValidPaths => golden::parse_query_valid_paths_fields(data).await,
            OpcodeParser::AddTempRoot => golden::parse_add_temp_root_fields(data).await,
        }
    }
}

/// Helper: load fixture, run rio-build, parse both responses, compare fields.
async fn run_byte_conformance(fixture_name: &str, parser: OpcodeParser) {
    if !golden::fixtures_exist(fixture_name) {
        eprintln!(
            "skipping byte conformance test '{fixture_name}': \
             fixtures not found (run with RECORD_GOLDEN=1 to record)"
        );
        return;
    }

    let fixture = golden::GoldenFixture::load(fixture_name);
    let store = fixture.build_memory_store();

    // Run rio-build with the same client bytes
    let actual_response = rio_build_response(&fixture.client_bytes, store).await;

    // Split into handshake + remainder for both
    let (expected_hs, expected_rest) = golden::split_handshake(&fixture.server_bytes).await;
    let (actual_hs, actual_rest) = golden::split_handshake(&actual_response).await;

    // Compare handshake fields
    let expected_hs_fields = golden::parse_handshake_fields(&expected_hs).await;
    let actual_hs_fields = golden::parse_handshake_fields(&actual_hs).await;
    golden::assert_field_conformance(&expected_hs_fields, &actual_hs_fields, &fixture.skip_fields);

    // If there's more data after the handshake (SetOptions + opcode), compare those too
    if !expected_rest.is_empty() {
        let (expected_so, expected_op) = golden::split_set_options(&expected_rest);
        let (actual_so, actual_op) = golden::split_set_options(&actual_rest);

        // SetOptions response is always the same format
        let expected_so_fields = golden::parse_set_options_fields(&expected_so).await;
        let actual_so_fields = golden::parse_set_options_fields(&actual_so).await;
        golden::assert_field_conformance(
            &expected_so_fields,
            &actual_so_fields,
            &fixture.skip_fields,
        );

        // Parse opcode-specific response
        if !expected_op.is_empty() {
            let expected_op_fields = parser.parse(&expected_op).await;
            let actual_op_fields = parser.parse(&actual_op).await;
            golden::assert_field_conformance(
                &expected_op_fields,
                &actual_op_fields,
                &fixture.skip_fields,
            );
        }
    }
}

#[tokio::test]
async fn test_byte_conformance_handshake() {
    // Handshake-only — no opcode response to parse
    if !golden::fixtures_exist("handshake") {
        eprintln!("skipping: fixtures not found (run with RECORD_GOLDEN=1 to record)");
        return;
    }

    let fixture = golden::GoldenFixture::load("handshake");
    let store = fixture.build_memory_store();
    let actual = rio_build_response(&fixture.client_bytes, store).await;

    let expected_fields = golden::parse_handshake_fields(&fixture.server_bytes).await;
    let actual_fields = golden::parse_handshake_fields(&actual).await;
    golden::assert_field_conformance(&expected_fields, &actual_fields, &fixture.skip_fields);
}

#[tokio::test]
async fn test_byte_conformance_set_options() {
    run_byte_conformance("set_options", OpcodeParser::None).await;
}

#[tokio::test]
async fn test_byte_conformance_is_valid_path_found() {
    run_byte_conformance("is_valid_path_found", OpcodeParser::IsValidPath).await;
}

#[tokio::test]
async fn test_byte_conformance_is_valid_path_not_found() {
    run_byte_conformance("is_valid_path_not_found", OpcodeParser::IsValidPath).await;
}

#[tokio::test]
async fn test_byte_conformance_query_path_info() {
    run_byte_conformance("query_path_info", OpcodeParser::QueryPathInfo).await;
}

#[tokio::test]
async fn test_byte_conformance_query_valid_paths() {
    run_byte_conformance("query_valid_paths", OpcodeParser::QueryValidPaths).await;
}

#[tokio::test]
async fn test_byte_conformance_add_temp_root() {
    run_byte_conformance("add_temp_root", OpcodeParser::AddTempRoot).await;
}
