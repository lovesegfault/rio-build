//! Golden protocol conformance tests.
//!
//! Live-daemon conformance tests: start an isolated nix-daemon, exchange with
//! it, then compare its response field-by-field against rio-gateway's response
//! using the same client bytes. This validates wire format conformance against
//! the real implementation.
//!
//! Ported from rio-build/tests/golden_conformance.rs. Uses MockStore (gRPC)
//! instead of the monolith's local MemoryStore.

mod golden;

use std::io::Cursor;

use rio_test_support::grpc::{MockStore, spawn_mock_scheduler, spawn_mock_store};

// ============================================================================
// Gateway harness
// ============================================================================

/// Feed client bytes to rio-gateway and capture the server response.
///
/// Spawns a MockStore + MockScheduler, connects gRPC clients, and runs
/// `session::run_protocol` on a Cursor reader + duplex writer.
async fn gateway_response(client_bytes: &[u8], store: MockStore) -> Vec<u8> {
    // Spawn mock gRPC servers for this store + a dummy scheduler
    let store_addr = spawn_store_for(store).await;
    let (_sched, sched_addr, _sched_handle) = spawn_mock_scheduler().await;

    let mut store_client = rio_proto::client::connect_store(&store_addr.to_string())
        .await
        .expect("connect mock store");
    let mut scheduler_client = rio_proto::client::connect_scheduler(&sched_addr.to_string())
        .await
        .expect("connect mock scheduler");

    let (response_reader, response_writer) = tokio::io::duplex(256 * 1024);

    let client_data = client_bytes.to_vec();
    let proto_task = tokio::spawn(async move {
        let mut reader = Cursor::new(client_data);
        let mut writer = response_writer;
        let _ = rio_gateway::session::run_protocol(
            &mut reader,
            &mut writer,
            &mut store_client,
            &mut scheduler_client,
        )
        .await;
    });

    let mut response = Vec::new();
    let mut reader = tokio::io::BufReader::new(response_reader);
    tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut response)
        .await
        .unwrap();

    proto_task.await.unwrap();
    response
}

/// Spawn a pre-populated MockStore and return its address.
async fn spawn_store_for(store: MockStore) -> std::net::SocketAddr {
    use rio_proto::store::store_service_server::StoreServiceServer;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::transport::Server;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let incoming = TcpListenerStream::new(listener);
        Server::builder()
            .add_service(StoreServiceServer::new(store))
            .serve_with_incoming(incoming)
            .await
            .expect("mock store server");
    });
    tokio::task::yield_now().await;
    addr
}

// ============================================================================
// Live-daemon conformance tests
// ============================================================================

/// Fields that legitimately differ between nix-daemon and rio-gateway.
const SKIP_FIELDS: &[&str] = &["version_string", "trusted"];

type OpcodeFieldParser = fn(
    &[u8],
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Vec<golden::ResponseField>> + '_>,
>;

/// Helper: run a live-daemon conformance test.
async fn run_live_conformance(
    opcode_bytes: Option<&[u8]>,
    store: MockStore,
    skip: &[&str],
    parse_opcode: OpcodeFieldParser,
) {
    let socket = golden::daemon::shared_daemon_socket();

    let (client_bytes, daemon_response) =
        golden::daemon::exchange_with_daemon(&socket, opcode_bytes)
            .await
            .expect("daemon exchange failed");

    let rio_response = gateway_response(&client_bytes, store).await;

    let (daemon_hs, daemon_rest) = golden::split_handshake(&daemon_response).await;
    let (rio_hs, rio_rest) = golden::split_handshake(&rio_response).await;

    let daemon_hs_fields = golden::parse_handshake_fields(&daemon_hs).await;
    let rio_hs_fields = golden::parse_handshake_fields(&rio_hs).await;
    golden::assert_fully_consumed(&daemon_hs, &daemon_hs_fields, "daemon handshake");
    golden::assert_fully_consumed(&rio_hs, &rio_hs_fields, "rio-gateway handshake");
    golden::assert_field_conformance(&daemon_hs_fields, &rio_hs_fields, skip);

    if !daemon_rest.is_empty() {
        let (daemon_so, daemon_op) = golden::split_set_options(&daemon_rest);
        let (rio_so, rio_op) = golden::split_set_options(&rio_rest);

        let daemon_so_fields = golden::parse_set_options_fields(&daemon_so).await;
        let rio_so_fields = golden::parse_set_options_fields(&rio_so).await;
        golden::assert_fully_consumed(&daemon_so, &daemon_so_fields, "daemon SetOptions");
        golden::assert_fully_consumed(&rio_so, &rio_so_fields, "rio-gateway SetOptions");
        golden::assert_field_conformance(&daemon_so_fields, &rio_so_fields, skip);

        if !daemon_op.is_empty() {
            let daemon_op_stripped = golden::strip_stderr_activity(&daemon_op).await;
            let daemon_op_fields = parse_opcode(&daemon_op_stripped).await;
            let rio_op_fields = parse_opcode(&rio_op).await;
            golden::assert_fully_consumed(&daemon_op_stripped, &daemon_op_fields, "daemon opcode");
            golden::assert_fully_consumed(&rio_op, &rio_op_fields, "rio-gateway opcode");
            golden::assert_field_conformance(&daemon_op_fields, &rio_op_fields, skip);
        }
    }
}

#[tokio::test]
async fn test_golden_live_handshake() {
    let socket = golden::daemon::shared_daemon_socket();
    let (_store, store_addr, _sh) = spawn_mock_store().await;
    let (_sched, sched_addr, _sch) = spawn_mock_scheduler().await;

    let (client_bytes, daemon_response) = golden::daemon::exchange_with_daemon(&socket, None)
        .await
        .expect("daemon exchange failed");

    let mut store_client = rio_proto::client::connect_store(&store_addr.to_string())
        .await
        .unwrap();
    let mut scheduler_client = rio_proto::client::connect_scheduler(&sched_addr.to_string())
        .await
        .unwrap();

    let (response_reader, response_writer) = tokio::io::duplex(256 * 1024);
    let client_data = client_bytes.clone();
    let proto_task = tokio::spawn(async move {
        let mut reader = Cursor::new(client_data);
        let mut writer = response_writer;
        let _ = rio_gateway::session::run_protocol(
            &mut reader,
            &mut writer,
            &mut store_client,
            &mut scheduler_client,
        )
        .await;
    });
    let mut rio_response = Vec::new();
    let mut reader = tokio::io::BufReader::new(response_reader);
    tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut rio_response)
        .await
        .unwrap();
    proto_task.await.unwrap();

    let daemon_fields = golden::parse_handshake_fields(&daemon_response).await;
    let rio_fields = golden::parse_handshake_fields(&rio_response).await;
    golden::assert_field_conformance(&daemon_fields, &rio_fields, SKIP_FIELDS);
}

#[tokio::test]
async fn test_golden_live_is_valid_path_found() {
    let test_path = golden::daemon::build_test_path();
    let path_info = golden::daemon::query_path_info_json(&test_path);
    let store = MockStore::new();
    golden::seed_mock_store_from(&store, &[path_info]);

    let op = golden::build_is_valid_path_bytes(&test_path).await;
    run_live_conformance(Some(&op), store, SKIP_FIELDS, |data| {
        Box::pin(golden::parse_is_valid_path_fields(data))
    })
    .await;
}

#[tokio::test]
async fn test_golden_live_is_valid_path_not_found() {
    let store = MockStore::new();
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
    let store = MockStore::new();
    golden::seed_mock_store_from(&store, &[path_info]);

    let op = golden::build_query_path_info_bytes(&test_path).await;

    // Also skip reg_time — mock store uses the JSON-queried time
    let skip: &[&str] = &["version_string", "trusted", "reg_time"];
    run_live_conformance(Some(&op), store, skip, |data| {
        Box::pin(golden::parse_query_path_info_fields(data))
    })
    .await;
}

#[tokio::test]
async fn test_golden_live_query_path_info_not_found() {
    let store = MockStore::new();
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
    let store = MockStore::new();
    golden::seed_mock_store_from(&store, &[path_info]);

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
    let store = MockStore::new();

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
    let store = MockStore::new();
    golden::seed_mock_store_from(&store, &[path_info]);

    let nonexistent = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-nonexistent-1.0";
    let op = golden::build_query_missing_bytes(&[&test_path, nonexistent]).await;
    run_live_conformance(Some(&op), store, SKIP_FIELDS, |data| {
        Box::pin(golden::parse_query_missing_fields(data))
    })
    .await;
}

/// NarFromPath golden test. Documents the wire-format: rio-gateway sends
/// STDERR_LAST then raw NAR bytes (matching nix-daemon); the old rio-build
/// monolith used STDERR_WRITE chunks.
#[tokio::test]
async fn test_golden_live_nar_from_path() {
    let test_path = golden::daemon::build_test_path();
    let path_info = golden::daemon::query_path_info_json(&test_path);
    let store = MockStore::new();
    golden::seed_mock_store_from(&store, &[path_info]);

    let socket = golden::daemon::shared_daemon_socket();

    let op = golden::build_nar_from_path_bytes(&test_path).await;
    let (client_bytes, daemon_response) =
        golden::daemon::exchange_with_daemon_nar(&socket, Some(&op))
            .await
            .expect("daemon exchange failed");

    let rio_response = gateway_response(&client_bytes, store).await;

    // Compare handshake
    let (daemon_hs, daemon_rest) = golden::split_handshake(&daemon_response).await;
    let (rio_hs, rio_rest) = golden::split_handshake(&rio_response).await;
    let daemon_hs_fields = golden::parse_handshake_fields(&daemon_hs).await;
    let rio_hs_fields = golden::parse_handshake_fields(&rio_hs).await;
    golden::assert_field_conformance(&daemon_hs_fields, &rio_hs_fields, SKIP_FIELDS);

    // Compare SetOptions
    let (daemon_so, daemon_op) = golden::split_set_options(&daemon_rest);
    let (rio_so, rio_op) = golden::split_set_options(&rio_rest);
    let daemon_so_fields = golden::parse_set_options_fields(&daemon_so).await;
    let rio_so_fields = golden::parse_set_options_fields(&rio_so).await;
    golden::assert_field_conformance(&daemon_so_fields, &rio_so_fields, SKIP_FIELDS);

    // Compare NAR content. Both sides now use STDERR_LAST + raw NAR (rio-gateway
    // was fixed to match the daemon; see wopNarFromPath handler).
    let daemon_op_fields = golden::parse_nar_from_path_fields(&daemon_op).await;
    let rio_op_fields = golden::parse_nar_from_path_fields(&rio_op).await;

    let daemon_nar = daemon_op_fields
        .iter()
        .find(|f| f.name == "nar_data")
        .expect("daemon response should have nar_data");
    let rio_nar = rio_op_fields
        .iter()
        .find(|f| f.name == "nar_data")
        .expect("rio-gateway response should have nar_data");
    assert_eq!(
        daemon_nar.bytes,
        rio_nar.bytes,
        "NAR content mismatch: daemon sent {} bytes, rio-gateway sent {} bytes",
        daemon_nar.bytes.len(),
        rio_nar.bytes.len()
    );
}

#[tokio::test]
async fn test_golden_live_query_path_from_hash_part_not_found() {
    let store = MockStore::new();
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
    let store = MockStore::new();
    golden::seed_mock_store_from(&store, &[path_info]);

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
    let store = MockStore::new();
    golden::seed_mock_store_from(&store, &[path_info]);

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
    let store = MockStore::new();
    golden::seed_mock_store_from(&store, &[path_info]);

    let op = golden::build_add_signatures_bytes(&test_path, &["cache.example.com:fakesig1"]).await;
    run_live_conformance(Some(&op), store, SKIP_FIELDS, |data| {
        Box::pin(golden::parse_add_signatures_fields(data))
    })
    .await;
}
