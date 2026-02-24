//! Golden protocol conformance tests.
//!
//! This file contains two layers of golden tests:
//!
//! 1. **Structural tests**: construct client bytes, feed them to rio-build,
//!    and verify the response has correct field values. These run without
//!    any external dependencies.
//!
//! 2. **Live-daemon conformance tests**: start an isolated nix-daemon per
//!    test, exchange with it, then compare its response field-by-field against
//!    rio-build's response using the same client bytes. This validates wire
//!    format conformance against the real implementation.

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
        golden::build_is_valid_path_bytes(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-nonexistent-1.0",
        )
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
        rio_build::store::PathInfoBuilder::new(
            path,
            NixHash::compute(HashAlgo::SHA256, b"test"),
            1000,
        )
        .registration_time(1700000000)
        .ultimate(true)
        .build()
        .unwrap(),
        None,
    );

    let mut client_bytes = build_handshake_client_bytes().await;
    client_bytes.extend(build_set_options_bytes().await);
    client_bytes.extend(
        golden::build_is_valid_path_bytes(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1",
        )
        .await,
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
        rio_build::store::PathInfoBuilder::new(path, nar_hash.clone(), 42000)
            .deriver(Some(deriver_path.clone()))
            .references(vec![ref_path.clone()])
            .registration_time(1700000000)
            .ultimate(true)
            .sigs(vec!["sig1:abc".to_string()])
            .build()
            .unwrap(),
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
// Live-daemon conformance tests
// ============================================================================
//
// Each test starts an isolated nix-daemon, exchanges with it, then compares
// the daemon's response field-by-field against rio-build's response.

/// Fields that legitimately differ between nix-daemon and rio-build.
const SKIP_FIELDS: &[&str] = &["version_string", "trusted"];

/// Function pointer type for opcode response field parsers.
type OpcodeFieldParser = fn(
    &[u8],
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Vec<golden::ResponseField>> + '_>,
>;

/// Helper: run a live-daemon conformance test.
///
/// 1. Starts an isolated nix-daemon
/// 2. Exchanges with it using `opcode_bytes`
/// 3. Feeds the same client bytes to rio-build with the given store
/// 4. Parses both responses and compares field-by-field
async fn run_live_conformance(
    opcode_bytes: Option<&[u8]>,
    store: Arc<MemoryStore>,
    skip: &[&str],
    parse_opcode: OpcodeFieldParser,
) {
    let socket = golden::daemon::shared_daemon_socket();

    // Exchange with real daemon
    let (client_bytes, daemon_response) =
        golden::daemon::exchange_with_daemon(&socket, opcode_bytes)
            .await
            .expect("daemon exchange failed");

    // Exchange with rio-build using the same client bytes
    let rio_response = rio_build_response(&client_bytes, store).await;

    // Build skip list
    let skip_strings: Vec<String> = skip.iter().map(|s| (*s).to_string()).collect();

    // Split and compare handshake
    let (daemon_hs, daemon_rest) = golden::split_handshake(&daemon_response).await;
    let (rio_hs, rio_rest) = golden::split_handshake(&rio_response).await;

    let daemon_hs_fields = golden::parse_handshake_fields(&daemon_hs).await;
    let rio_hs_fields = golden::parse_handshake_fields(&rio_hs).await;
    golden::assert_fully_consumed(&daemon_hs, &daemon_hs_fields, "daemon handshake");
    golden::assert_fully_consumed(&rio_hs, &rio_hs_fields, "rio-build handshake");
    golden::assert_field_conformance(&daemon_hs_fields, &rio_hs_fields, &skip_strings);

    // Compare SetOptions + opcode (if present)
    if !daemon_rest.is_empty() {
        let (daemon_so, daemon_op) = golden::split_set_options(&daemon_rest);
        let (rio_so, rio_op) = golden::split_set_options(&rio_rest);

        let daemon_so_fields = golden::parse_set_options_fields(&daemon_so).await;
        let rio_so_fields = golden::parse_set_options_fields(&rio_so).await;
        golden::assert_fully_consumed(&daemon_so, &daemon_so_fields, "daemon SetOptions");
        golden::assert_fully_consumed(&rio_so, &rio_so_fields, "rio-build SetOptions");
        golden::assert_field_conformance(&daemon_so_fields, &rio_so_fields, &skip_strings);

        if !daemon_op.is_empty() {
            // Strip STDERR activity messages from the daemon's response —
            // the real daemon may send START_ACTIVITY/STOP_ACTIVITY before
            // STDERR_LAST, while rio-build skips those.
            let daemon_op_stripped = golden::strip_stderr_activity(&daemon_op).await;
            let daemon_op_fields = parse_opcode(&daemon_op_stripped).await;
            let rio_op_fields = parse_opcode(&rio_op).await;
            golden::assert_fully_consumed(&daemon_op_stripped, &daemon_op_fields, "daemon opcode");
            golden::assert_fully_consumed(&rio_op, &rio_op_fields, "rio-build opcode");
            golden::assert_field_conformance(&daemon_op_fields, &rio_op_fields, &skip_strings);
        }
    }
}

#[tokio::test]
async fn test_golden_live_handshake() {
    let socket = golden::daemon::shared_daemon_socket();
    let store = Arc::new(MemoryStore::new());

    // Exchange with real daemon (handshake only, no opcode)
    let (client_bytes, daemon_response) = golden::daemon::exchange_with_daemon(&socket, None)
        .await
        .expect("daemon exchange failed");

    // Exchange with rio-build
    let rio_response = rio_build_response(&client_bytes, store).await;

    // Compare only handshake fields (response includes SetOptions too, but we only parse handshake)
    let daemon_fields = golden::parse_handshake_fields(&daemon_response).await;
    let rio_fields = golden::parse_handshake_fields(&rio_response).await;
    let skip: Vec<String> = SKIP_FIELDS.iter().map(|s| (*s).to_string()).collect();
    golden::assert_field_conformance(&daemon_fields, &rio_fields, &skip);
}

#[tokio::test]
async fn test_golden_live_is_valid_path_found() {
    let test_path = golden::daemon::build_test_path();
    let path_info = golden::daemon::query_path_info_json(&test_path);
    let store = golden::build_memory_store_from(&[path_info]);

    let op = golden::build_is_valid_path_bytes(&test_path).await;
    run_live_conformance(Some(&op), store, SKIP_FIELDS, |data| {
        Box::pin(golden::parse_is_valid_path_fields(data))
    })
    .await;
}

#[tokio::test]
async fn test_golden_live_is_valid_path_not_found() {
    let store = Arc::new(MemoryStore::new());
    let nonexistent = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-nonexistent-1.0";

    let op = golden::build_is_valid_path_bytes(nonexistent).await;
    run_live_conformance(Some(&op), store, SKIP_FIELDS, |data| {
        Box::pin(golden::parse_is_valid_path_fields(data))
    })
    .await;
}

#[tokio::test]
async fn test_golden_live_query_path_info() {
    let test_path = golden::daemon::build_test_path();
    let path_info = golden::daemon::query_path_info_json(&test_path);
    let store = golden::build_memory_store_from(&[path_info]);

    let op = golden::build_query_path_info_bytes(&test_path).await;

    // Also skip reg_time — MemoryStore uses the JSON-queried time
    let skip: &[&str] = &["version_string", "trusted", "reg_time"];
    run_live_conformance(Some(&op), store, skip, |data| {
        Box::pin(golden::parse_query_path_info_fields(data))
    })
    .await;
}

#[tokio::test]
async fn test_golden_live_query_path_info_not_found() {
    let store = Arc::new(MemoryStore::new());
    let nonexistent = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-nonexistent-1.0";

    let op = golden::build_query_path_info_bytes(nonexistent).await;
    run_live_conformance(Some(&op), store, SKIP_FIELDS, |data| {
        Box::pin(golden::parse_query_path_info_fields(data))
    })
    .await;
}

#[tokio::test]
async fn test_golden_live_query_valid_paths() {
    let test_path = golden::daemon::build_test_path();
    let path_info = golden::daemon::query_path_info_json(&test_path);
    let store = golden::build_memory_store_from(&[path_info]);

    let nonexistent = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-nonexistent-1.0";
    let op = golden::build_query_valid_paths_bytes(&[&test_path, nonexistent], false).await;
    run_live_conformance(Some(&op), store, SKIP_FIELDS, |data| {
        Box::pin(golden::parse_query_valid_paths_fields(data))
    })
    .await;
}

#[tokio::test]
async fn test_golden_live_add_temp_root() {
    let test_path = golden::daemon::build_test_path();
    let store = Arc::new(MemoryStore::new());

    let op = golden::build_add_temp_root_bytes(&test_path).await;
    run_live_conformance(Some(&op), store, SKIP_FIELDS, |data| {
        Box::pin(golden::parse_add_temp_root_fields(data))
    })
    .await;
}

#[tokio::test]
async fn test_golden_live_query_missing() {
    let test_path = golden::daemon::build_test_path();
    let path_info = golden::daemon::query_path_info_json(&test_path);
    let store = golden::build_memory_store_from(&[path_info]);

    // Mix of existing and nonexistent paths: the existing path should not
    // appear in any output list, and the nonexistent opaque path should
    // appear in `unknown` (not `will_build`, since it's not a derivation).
    let nonexistent = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-nonexistent-1.0";
    let op = golden::build_query_missing_bytes(&[&test_path, nonexistent]).await;
    run_live_conformance(Some(&op), store, SKIP_FIELDS, |data| {
        Box::pin(golden::parse_query_missing_fields(data))
    })
    .await;
}

#[tokio::test]
async fn test_golden_live_nar_from_path() {
    use rio_build::store::MemoryStore;

    let test_path = golden::daemon::build_test_path();
    let path_info = golden::daemon::query_path_info_json(&test_path);
    let nar_data = golden::daemon::dump_nar(&test_path);

    // Build a MemoryStore with both metadata and NAR content
    let store = Arc::new(MemoryStore::new());
    let sp = rio_nix::store_path::StorePath::parse(&path_info.path).unwrap();
    let nar_hash = rio_nix::hash::NixHash::parse(&path_info.nar_hash).unwrap();
    let deriver = path_info
        .deriver
        .as_ref()
        .map(|d| rio_nix::store_path::StorePath::parse(d).unwrap());
    let references: Vec<rio_nix::store_path::StorePath> = path_info
        .references
        .iter()
        .map(|r| rio_nix::store_path::StorePath::parse(r).unwrap())
        .collect();
    store.insert(
        rio_build::store::PathInfoBuilder::new(sp, nar_hash, path_info.nar_size)
            .deriver(deriver)
            .references(references)
            .registration_time(path_info.registration_time)
            .ultimate(path_info.ultimate)
            .sigs(path_info.sigs.clone())
            .ca(path_info.ca.clone())
            .build()
            .unwrap(),
        Some(nar_data),
    );

    let socket = golden::daemon::shared_daemon_socket();

    let op = golden::build_nar_from_path_bytes(&test_path).await;
    let (client_bytes, daemon_response) =
        golden::daemon::exchange_with_daemon_nar(&socket, Some(&op))
            .await
            .expect("daemon exchange failed");

    let rio_response = rio_build_response(&client_bytes, store).await;

    let skip_strings: Vec<String> = SKIP_FIELDS.iter().map(|s| (*s).to_string()).collect();

    // Compare handshake
    let (daemon_hs, daemon_rest) = golden::split_handshake(&daemon_response).await;
    let (rio_hs, rio_rest) = golden::split_handshake(&rio_response).await;
    let daemon_hs_fields = golden::parse_handshake_fields(&daemon_hs).await;
    let rio_hs_fields = golden::parse_handshake_fields(&rio_hs).await;
    golden::assert_field_conformance(&daemon_hs_fields, &rio_hs_fields, &skip_strings);

    // Compare SetOptions
    let (daemon_so, daemon_op) = golden::split_set_options(&daemon_rest);
    let (rio_so, rio_op) = golden::split_set_options(&rio_rest);
    let daemon_so_fields = golden::parse_set_options_fields(&daemon_so).await;
    let rio_so_fields = golden::parse_set_options_fields(&rio_so).await;
    golden::assert_field_conformance(&daemon_so_fields, &rio_so_fields, &skip_strings);

    // Compare NarFromPath NAR content.
    //
    // Intentional wire format divergence: the nix-daemon sends STDERR_LAST
    // followed by raw NAR bytes (confirmed in nix src/libstore/daemon.cc),
    // while rio-build wraps the NAR in STDERR_WRITE chunks before
    // STDERR_LAST. Both formats are understood by the Nix client. We use
    // separate parsers but compare the extracted NAR content byte-for-byte.
    let daemon_op_fields = golden::parse_nar_from_path_daemon_fields(&daemon_op).await;
    let rio_op_fields = golden::parse_nar_from_path_fields(&rio_op).await;

    let daemon_nar = daemon_op_fields
        .iter()
        .find(|f| f.name == "nar_data")
        .expect("daemon response should have nar_data");
    let rio_nar = rio_op_fields
        .iter()
        .find(|f| f.name == "nar_data")
        .expect("rio-build response should have nar_data");
    assert_eq!(
        daemon_nar.bytes,
        rio_nar.bytes,
        "NAR content mismatch: daemon sent {} bytes, rio-build sent {} bytes",
        daemon_nar.bytes.len(),
        rio_nar.bytes.len()
    );
}

#[tokio::test]
async fn test_golden_live_query_path_from_hash_part_not_found() {
    let store = Arc::new(MemoryStore::new());
    // Use a hash part that doesn't exist — both daemon and rio-build return empty.
    let hash_part = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    let op = golden::build_query_path_from_hash_part_bytes(hash_part).await;
    run_live_conformance(Some(&op), store, SKIP_FIELDS, |data| {
        Box::pin(golden::parse_query_path_from_hash_part_fields(data))
    })
    .await;
}

#[tokio::test]
async fn test_golden_live_query_path_from_hash_part_found() {
    let test_path = golden::daemon::build_test_path();
    let path_info = golden::daemon::query_path_info_json(&test_path);
    let store = golden::build_memory_store_from(&[path_info]);

    let sp = rio_nix::store_path::StorePath::parse(&test_path).unwrap();
    let hash_part = sp.hash_part();

    let op = golden::build_query_path_from_hash_part_bytes(&hash_part).await;
    run_live_conformance(Some(&op), store, SKIP_FIELDS, |data| {
        Box::pin(golden::parse_query_path_from_hash_part_fields(data))
    })
    .await;
}

#[tokio::test]
async fn test_golden_live_query_path_from_hash_part_ca() {
    let test_path = golden::daemon::build_ca_test_path();
    let path_info = golden::daemon::query_path_info_json(&test_path);
    let store = golden::build_memory_store_from(&[path_info]);

    let sp = rio_nix::store_path::StorePath::parse(&test_path).unwrap();
    let hash_part = sp.hash_part();

    let op = golden::build_query_path_from_hash_part_bytes(&hash_part).await;
    run_live_conformance(Some(&op), store, SKIP_FIELDS, |data| {
        Box::pin(golden::parse_query_path_from_hash_part_fields(data))
    })
    .await;
}

#[tokio::test]
async fn test_golden_live_add_signatures() {
    let test_path = golden::daemon::build_test_path();
    let path_info = golden::daemon::query_path_info_json(&test_path);
    let store = golden::build_memory_store_from(&[path_info]);

    let op = golden::build_add_signatures_bytes(&test_path, &["cache.example.com:fakesig1"]).await;
    run_live_conformance(Some(&op), store, SKIP_FIELDS, |data| {
        Box::pin(golden::parse_add_signatures_fields(data))
    })
    .await;
}

#[tokio::test]
async fn test_golden_add_signatures_persisted() {
    use rio_nix::hash::{HashAlgo, NixHash};
    use rio_nix::store_path::StorePath;

    // Pre-populate store with a path that has one existing signature
    let store = Arc::new(MemoryStore::new());
    let path =
        StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1").unwrap();
    store.insert(
        rio_build::store::PathInfoBuilder::new(
            path,
            NixHash::compute(HashAlgo::SHA256, b"test"),
            1000,
        )
        .registration_time(1700000000)
        .ultimate(true)
        .sigs(vec!["existing-key:existingsig".to_string()])
        .build()
        .unwrap(),
        None,
    );

    // Send AddSignatures with a new signature, then QueryPathInfo
    let mut client_bytes = build_handshake_client_bytes().await;
    client_bytes.extend(build_set_options_bytes().await);
    client_bytes.extend(
        golden::build_add_signatures_bytes(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1",
            &["new-key:newsig"],
        )
        .await,
    );

    // Append QueryPathInfo to verify signatures
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

    // Skip handshake
    let _magic2 = wire::read_u64(&mut reader).await.unwrap();
    let _version = wire::read_u64(&mut reader).await.unwrap();
    let _features = wire::read_strings(&mut reader).await.unwrap();
    let _version_str = wire::read_string(&mut reader).await.unwrap();
    let _trusted = wire::read_u64(&mut reader).await.unwrap();
    let _last = wire::read_u64(&mut reader).await.unwrap(); // handshake STDERR_LAST
    let _last = wire::read_u64(&mut reader).await.unwrap(); // SetOptions STDERR_LAST

    // AddSignatures response
    let last = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(last, STDERR_LAST, "expected STDERR_LAST for AddSignatures");
    let result = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(result, 1, "AddSignatures should return success");

    // QueryPathInfo response
    let last = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(last, STDERR_LAST, "expected STDERR_LAST for QueryPathInfo");
    let valid = wire::read_u64(&mut reader).await.unwrap();
    assert_eq!(valid, 1, "path should be valid");

    let _deriver = wire::read_string(&mut reader).await.unwrap();
    let _nar_hash = wire::read_string(&mut reader).await.unwrap();
    let _refs = wire::read_strings(&mut reader).await.unwrap();
    let _reg_time = wire::read_u64(&mut reader).await.unwrap();
    let _nar_size = wire::read_u64(&mut reader).await.unwrap();
    let _ultimate = wire::read_u64(&mut reader).await.unwrap();
    let sigs = wire::read_strings(&mut reader).await.unwrap();

    assert!(
        sigs.contains(&"existing-key:existingsig".to_string()),
        "original signature should be preserved, got: {sigs:?}"
    );
    assert!(
        sigs.contains(&"new-key:newsig".to_string()),
        "new signature should be added, got: {sigs:?}"
    );
    assert_eq!(
        sigs.len(),
        2,
        "should have exactly 2 signatures, got: {sigs:?}"
    );
}
