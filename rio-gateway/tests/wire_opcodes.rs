//! Byte-level opcode tests for the rio-gateway Nix worker protocol handler.
//!
//! These tests construct raw wire bytes for each opcode, feed them through
//! `run_protocol` via a DuplexStream, and assert the response bytes match the
//! Nix worker protocol spec. This catches framing and encoding bugs that
//! high-level integration tests hide.
//!
//! Test structure:
//!   - TestHarness: wraps duplex stream + mock gRPC servers + spawned protocol task
//!   - drain_stderr_until_last: consumes STDERR messages until STDERR_LAST
//!   - drain_stderr_expecting_error: consumes STDERR messages expecting STDERR_ERROR
//!   - Per-opcode tests: happy path + error path for each

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncWriteExt, DuplexStream};
use tonic::transport::{Channel, Server};
use tonic::{Request, Response, Status, Streaming};

use rio_nix::nar::NarNode;
use rio_nix::protocol::client::{StderrMessage, read_stderr_message};
use rio_nix::protocol::handshake::{PROTOCOL_VERSION, WORKER_MAGIC_1, WORKER_MAGIC_2};
use rio_nix::protocol::stderr::{STDERR_LAST, StderrError};
use rio_nix::protocol::wire;
use rio_proto::scheduler::scheduler_service_client::SchedulerServiceClient;
use rio_proto::scheduler::scheduler_service_server::{SchedulerService, SchedulerServiceServer};
use rio_proto::store::store_service_client::StoreServiceClient;
use rio_proto::store::store_service_server::{StoreService, StoreServiceServer};
use rio_proto::types;

// ===========================================================================
// Mock StoreService
// ===========================================================================

/// Mock store that can be pre-seeded with paths and NAR data. Unlike the
/// minimal mock in integration_distributed.rs, this one stores actual NAR
/// bytes so GetPath can stream them back (needed for NarFromPath tests).
#[derive(Clone)]
struct MockStore {
    /// store_path -> (PathInfo, NAR bytes)
    #[allow(clippy::type_complexity)]
    paths: Arc<RwLock<HashMap<String, (types::PathInfo, Vec<u8>)>>>,
    /// Recorded PutPath calls: store_path -> PathInfo
    put_calls: Arc<RwLock<Vec<types::PathInfo>>>,
}

impl MockStore {
    fn new() -> Self {
        Self {
            paths: Arc::new(RwLock::new(HashMap::new())),
            put_calls: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Pre-seed a path with PathInfo and NAR data.
    fn seed(&self, info: types::PathInfo, nar_data: Vec<u8>) {
        let store_path = info.store_path.clone();
        self.paths
            .write()
            .unwrap()
            .insert(store_path, (info, nar_data));
    }
}

#[tonic::async_trait]
impl StoreService for MockStore {
    async fn put_path(
        &self,
        request: Request<Streaming<types::PutPathRequest>>,
    ) -> Result<Response<types::PutPathResponse>, Status> {
        let mut stream = request.into_inner();
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty PutPath stream"))?;
        let info = match first.msg {
            Some(types::put_path_request::Msg::Metadata(m)) => m
                .info
                .ok_or_else(|| Status::invalid_argument("PutPathMetadata missing PathInfo"))?,
            _ => return Err(Status::invalid_argument("first message must be metadata")),
        };
        // Drain NAR chunks
        let mut nar = Vec::new();
        while let Some(msg) = stream.message().await? {
            if let Some(types::put_path_request::Msg::NarChunk(chunk)) = msg.msg {
                nar.extend_from_slice(&chunk);
            }
        }
        self.put_calls.write().unwrap().push(info.clone());
        let store_path = info.store_path.clone();
        self.paths.write().unwrap().insert(store_path, (info, nar));
        Ok(Response::new(types::PutPathResponse { created: true }))
    }

    type GetPathStream =
        tokio_stream::wrappers::ReceiverStream<Result<types::GetPathResponse, Status>>;

    async fn get_path(
        &self,
        request: Request<types::GetPathRequest>,
    ) -> Result<Response<Self::GetPathStream>, Status> {
        let store_path = request.into_inner().store_path;
        let entry = self.paths.read().unwrap().get(&store_path).cloned();
        match entry {
            Some((info, nar)) => {
                let (tx, rx) = tokio::sync::mpsc::channel(4);
                tokio::spawn(async move {
                    let _ = tx
                        .send(Ok(types::GetPathResponse {
                            msg: Some(types::get_path_response::Msg::Info(info)),
                        }))
                        .await;
                    // Send NAR in 64 KiB chunks (matches real store)
                    for chunk in nar.chunks(64 * 1024) {
                        let _ = tx
                            .send(Ok(types::GetPathResponse {
                                msg: Some(types::get_path_response::Msg::NarChunk(chunk.to_vec())),
                            }))
                            .await;
                    }
                });
                Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
                    rx,
                )))
            }
            None => Err(Status::not_found(format!("not found: {store_path}"))),
        }
    }

    async fn query_path_info(
        &self,
        request: Request<types::QueryPathInfoRequest>,
    ) -> Result<Response<types::PathInfo>, Status> {
        let store_path = request.into_inner().store_path;
        let paths = self.paths.read().unwrap();
        // Exact match first, then prefix match (for QueryPathFromHashPart,
        // which queries /nix/store/{hash_part} expecting a prefix lookup).
        if let Some((info, _)) = paths.get(&store_path) {
            return Ok(Response::new(info.clone()));
        }
        for (k, (info, _)) in paths.iter() {
            if k.starts_with(&store_path) {
                return Ok(Response::new(info.clone()));
            }
        }
        Err(Status::not_found(format!("not found: {store_path}")))
    }

    async fn find_missing_paths(
        &self,
        request: Request<types::FindMissingPathsRequest>,
    ) -> Result<Response<types::FindMissingPathsResponse>, Status> {
        let requested = request.into_inner().store_paths;
        let paths = self.paths.read().unwrap();
        let missing: Vec<String> = requested
            .into_iter()
            .filter(|p| !paths.contains_key(p))
            .collect();
        Ok(Response::new(types::FindMissingPathsResponse {
            missing_paths: missing,
        }))
    }

    async fn content_lookup(
        &self,
        _request: Request<types::ContentLookupRequest>,
    ) -> Result<Response<types::ContentLookupResponse>, Status> {
        Ok(Response::new(types::ContentLookupResponse {
            store_path: String::new(),
            info: None,
        }))
    }
}

// ===========================================================================
// Mock SchedulerService (minimal: only SubmitBuild and CancelBuild)
// ===========================================================================

#[derive(Clone, Default)]
struct MockSchedulerOutcome {
    /// If set, SubmitBuild immediately fails with this status.
    submit_error: Option<tonic::Code>,
    /// If set, SubmitBuild sends BuildCompleted after BuildStarted.
    send_completed: bool,
    /// If set, SubmitBuild sends BuildFailed after BuildStarted.
    send_failed: bool,
}

#[derive(Clone)]
struct MockScheduler {
    outcome: Arc<RwLock<MockSchedulerOutcome>>,
    submit_calls: Arc<RwLock<Vec<types::SubmitBuildRequest>>>,
}

impl MockScheduler {
    fn new() -> Self {
        Self {
            outcome: Arc::new(RwLock::new(MockSchedulerOutcome::default())),
            submit_calls: Arc::new(RwLock::new(Vec::new())),
        }
    }

    #[allow(dead_code)] // used in Commit 22 build opcode tests
    fn set_outcome(&self, outcome: MockSchedulerOutcome) {
        *self.outcome.write().unwrap() = outcome;
    }
}

#[tonic::async_trait]
impl SchedulerService for MockScheduler {
    type SubmitBuildStream =
        tokio_stream::wrappers::ReceiverStream<Result<types::BuildEvent, Status>>;

    async fn submit_build(
        &self,
        request: Request<types::SubmitBuildRequest>,
    ) -> Result<Response<Self::SubmitBuildStream>, Status> {
        let req = request.into_inner();
        self.submit_calls.write().unwrap().push(req.clone());

        let outcome = self.outcome.read().unwrap().clone();
        if let Some(code) = outcome.submit_error {
            return Err(Status::new(code, "mock scheduler error"));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let build_id = "test-build-00000000-1111-2222-3333-444444444444".to_string();
        tokio::spawn(async move {
            let _ = tx
                .send(Ok(types::BuildEvent {
                    build_id: build_id.clone(),
                    sequence: 0,
                    timestamp: None,
                    event: Some(types::build_event::Event::Started(types::BuildStarted {
                        total_derivations: 1,
                        cached_derivations: 0,
                    })),
                }))
                .await;

            if outcome.send_completed {
                let _ = tx
                    .send(Ok(types::BuildEvent {
                        build_id: build_id.clone(),
                        sequence: 1,
                        timestamp: None,
                        event: Some(types::build_event::Event::Completed(
                            types::BuildCompleted {
                                output_paths: vec!["/nix/store/zzz-output".into()],
                            },
                        )),
                    }))
                    .await;
            } else if outcome.send_failed {
                let _ = tx
                    .send(Ok(types::BuildEvent {
                        build_id,
                        sequence: 1,
                        timestamp: None,
                        event: Some(types::build_event::Event::Failed(types::BuildFailed {
                            error_message: "mock build failure".into(),
                            failed_derivation: String::new(),
                        })),
                    }))
                    .await;
            } else {
                // Keep open
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    type WatchBuildStream =
        tokio_stream::wrappers::ReceiverStream<Result<types::BuildEvent, Status>>;

    async fn watch_build(
        &self,
        _request: Request<types::WatchBuildRequest>,
    ) -> Result<Response<Self::WatchBuildStream>, Status> {
        Err(Status::not_found("not implemented in mock"))
    }

    async fn query_build_status(
        &self,
        _request: Request<types::QueryBuildRequest>,
    ) -> Result<Response<types::BuildStatus>, Status> {
        Err(Status::not_found("not implemented in mock"))
    }

    async fn cancel_build(
        &self,
        _request: Request<types::CancelBuildRequest>,
    ) -> Result<Response<types::CancelBuildResponse>, Status> {
        Ok(Response::new(types::CancelBuildResponse {
            cancelled: true,
        }))
    }
}

// ===========================================================================
// Test Harness
// ===========================================================================

struct TestHarness {
    /// Client-side stream for writing opcodes and reading responses.
    stream: DuplexStream,
    /// Mock store (pre-seed paths, inspect put_calls).
    store: MockStore,
    /// Mock scheduler (set outcome, inspect submit_calls).
    #[allow(dead_code)] // used in Commit 22 build opcode tests
    scheduler: MockScheduler,
    /// Server task running run_protocol.
    server_task: tokio::task::JoinHandle<()>,
    /// gRPC server handles (abort on drop).
    store_handle: tokio::task::JoinHandle<()>,
    sched_handle: tokio::task::JoinHandle<()>,
}

impl TestHarness {
    /// Start mock gRPC servers, spawn run_protocol, and perform handshake +
    /// setOptions. Returns a harness with a ready-to-use client stream.
    async fn setup() -> Self {
        let (store, store_addr, store_handle) = start_mock_store().await;
        let (scheduler, sched_addr, sched_handle) = start_mock_scheduler().await;

        // Connect gRPC clients
        let store_channel = Channel::from_shared(format!("http://{store_addr}"))
            .unwrap()
            .connect()
            .await
            .expect("connect to mock store");
        let mut store_client = StoreServiceClient::new(store_channel);
        let sched_channel = Channel::from_shared(format!("http://{sched_addr}"))
            .unwrap()
            .connect()
            .await
            .expect("connect to mock scheduler");
        let mut scheduler_client = SchedulerServiceClient::new(sched_channel);

        // Duplex stream: client side stays here, server side goes to run_protocol
        let (client_stream, server_stream) = tokio::io::duplex(256 * 1024);

        let server_task = tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(server_stream);
            let result = rio_gateway::session::run_protocol(
                &mut reader,
                &mut writer,
                &mut store_client,
                &mut scheduler_client,
            )
            .await;
            // Clean EOF is expected. Unknown-opcode errors are also expected
            // (the handler sends STDERR_ERROR then returns an error to close
            // the connection — per protocol spec).
            if let Err(e) = &result {
                let is_eof = e
                    .downcast_ref::<rio_nix::protocol::wire::WireError>()
                    .is_some_and(|we| {
                        matches!(
                            we,
                            rio_nix::protocol::wire::WireError::Io(io)
                                if io.kind() == std::io::ErrorKind::UnexpectedEof
                        )
                    });
                let is_unknown_opcode = e.to_string().contains("unknown opcode");
                if !is_eof && !is_unknown_opcode {
                    panic!("unexpected server error: {e}");
                }
            }
        });

        let mut h = Self {
            stream: client_stream,
            store,
            scheduler,
            server_task,
            store_handle,
            sched_handle,
        };

        do_handshake(&mut h.stream).await;
        send_set_options(&mut h.stream).await;

        h
    }

    /// Finish the session: drop the client stream (EOF), await server task.
    async fn finish(self) {
        let TestHarness {
            stream,
            server_task,
            store_handle,
            sched_handle,
            ..
        } = self;
        drop(stream);
        server_task.await.expect("server task should not panic");
        store_handle.abort();
        sched_handle.abort();
    }
}

async fn start_mock_store() -> (MockStore, SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store = MockStore::new();
    let store_clone = store.clone();
    let handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        Server::builder()
            .add_service(StoreServiceServer::new(store_clone))
            .serve_with_incoming(incoming)
            .await
            .expect("mock store server");
    });
    tokio::task::yield_now().await;
    (store, addr, handle)
}

async fn start_mock_scheduler() -> (MockScheduler, SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let sched = MockScheduler::new();
    let sched_clone = sched.clone();
    let handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        Server::builder()
            .add_service(SchedulerServiceServer::new(sched_clone))
            .serve_with_incoming(incoming)
            .await
            .expect("mock scheduler server");
    });
    tokio::task::yield_now().await;
    (sched, addr, handle)
}

// ===========================================================================
// Protocol helpers
// ===========================================================================

async fn do_handshake(s: &mut DuplexStream) {
    wire::write_u64(s, WORKER_MAGIC_1).await.unwrap();
    wire::write_u64(s, PROTOCOL_VERSION).await.unwrap();
    s.flush().await.unwrap();

    let magic2 = wire::read_u64(s).await.unwrap();
    assert_eq!(magic2, WORKER_MAGIC_2);
    let _server_version = wire::read_u64(s).await.unwrap();

    let features: Vec<String> = vec![];
    wire::write_strings(s, &features).await.unwrap();
    s.flush().await.unwrap();
    let _server_features = wire::read_strings(s).await.unwrap();

    wire::write_u64(s, 0).await.unwrap(); // obsolete CPU affinity
    wire::write_u64(s, 0).await.unwrap(); // reserveSpace
    s.flush().await.unwrap();

    let _version = wire::read_string(s).await.unwrap();
    let _trusted = wire::read_u64(s).await.unwrap();

    let last = wire::read_u64(s).await.unwrap();
    assert_eq!(last, STDERR_LAST, "handshake should end with STDERR_LAST");
}

async fn send_set_options(s: &mut DuplexStream) {
    wire::write_u64(s, 19).await.unwrap(); // wopSetOptions
    for _ in 0..12 {
        wire::write_u64(s, 0).await.unwrap();
    }
    wire::write_u64(s, 0).await.unwrap(); // overrides count = 0
    s.flush().await.unwrap();

    let msg = wire::read_u64(s).await.unwrap();
    assert_eq!(msg, STDERR_LAST);
}

/// Drain STDERR messages until STDERR_LAST. Returns all non-Last messages.
/// Panics if STDERR_ERROR is received (use `drain_stderr_expecting_error` for
/// error-path tests).
async fn drain_stderr_until_last(s: &mut DuplexStream) -> Vec<StderrMessage> {
    let mut msgs = Vec::new();
    loop {
        match read_stderr_message(s).await.unwrap() {
            StderrMessage::Last => return msgs,
            StderrMessage::Error(e) => {
                panic!("unexpected STDERR_ERROR: {}", e.message());
            }
            other => msgs.push(other),
        }
    }
}

/// Drain STDERR messages expecting STDERR_ERROR. Returns the error.
/// Panics if STDERR_LAST is received first.
async fn drain_stderr_expecting_error(s: &mut DuplexStream) -> StderrError {
    loop {
        match read_stderr_message(s).await.unwrap() {
            StderrMessage::Error(e) => return e,
            StderrMessage::Last => panic!("expected STDERR_ERROR but got STDERR_LAST"),
            _ => {} // skip other messages
        }
    }
}

/// Construct a minimal NAR containing a single regular file with the given
/// contents. Returns the NAR bytes and the SHA-256 digest.
fn make_nar(contents: &[u8]) -> (Vec<u8>, [u8; 32]) {
    let node = NarNode::Regular {
        executable: false,
        contents: contents.to_vec(),
    };
    let mut buf = Vec::new();
    rio_nix::nar::serialize(&mut buf, &node).unwrap();
    let digest: [u8; 32] = Sha256::digest(&buf).into();
    (buf, digest)
}

/// Construct a PathInfo for a test path with the given NAR.
fn make_path_info(store_path: &str, nar: &[u8], nar_hash: [u8; 32]) -> types::PathInfo {
    types::PathInfo {
        store_path: store_path.to_string(),
        store_path_hash: vec![],
        deriver: String::new(),
        nar_hash: nar_hash.to_vec(),
        nar_size: nar.len() as u64,
        references: vec![],
        registration_time: 0,
        ultimate: false,
        signatures: vec![],
        content_address: String::new(),
    }
}

/// A valid-looking Nix store path (32-char nixbase32 hash + name).
const TEST_PATH_A: &str = "/nix/store/00000000000000000000000000000000-test-a";
const TEST_PATH_MISSING: &str = "/nix/store/11111111111111111111111111111111-missing";

// ===========================================================================
// Opcode tests: IsValidPath (1)
// ===========================================================================

#[tokio::test]
async fn test_is_valid_path_exists() {
    let mut h = TestHarness::setup().await;
    let (nar, hash) = make_nar(b"hello");
    h.store.seed(make_path_info(TEST_PATH_A, &nar, hash), nar);

    wire::write_u64(&mut h.stream, 1).await.unwrap(); // wopIsValidPath
    wire::write_string(&mut h.stream, TEST_PATH_A)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let valid = wire::read_bool(&mut h.stream).await.unwrap();
    assert!(valid, "seeded path should be valid");

    h.finish().await;
}

#[tokio::test]
async fn test_is_valid_path_missing() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 1).await.unwrap(); // wopIsValidPath
    wire::write_string(&mut h.stream, TEST_PATH_MISSING)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let valid = wire::read_bool(&mut h.stream).await.unwrap();
    assert!(!valid, "missing path should be invalid");

    h.finish().await;
}

// ===========================================================================
// Opcode tests: EnsurePath (10)
// ===========================================================================

#[tokio::test]
async fn test_ensure_path_exists() {
    let mut h = TestHarness::setup().await;
    let (nar, hash) = make_nar(b"ensure");
    h.store.seed(make_path_info(TEST_PATH_A, &nar, hash), nar);

    wire::write_u64(&mut h.stream, 10).await.unwrap(); // wopEnsurePath
    wire::write_string(&mut h.stream, TEST_PATH_A)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let result = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(result, 1, "EnsurePath should return 1 (success)");

    h.finish().await;
}

/// Phase 2a: EnsurePath is a stub that always returns success regardless of
/// whether the path exists. It reads the path argument and returns 1.
#[tokio::test]
async fn test_ensure_path_stub_always_succeeds() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 10).await.unwrap(); // wopEnsurePath
    wire::write_string(&mut h.stream, TEST_PATH_MISSING)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    // Phase 2a stub: always STDERR_LAST + 1, even for missing paths.
    drain_stderr_until_last(&mut h.stream).await;
    let result = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(result, 1, "EnsurePath stub returns 1 unconditionally");

    h.finish().await;
}

// ===========================================================================
// Opcode tests: QueryPathInfo (26)
// ===========================================================================

#[tokio::test]
async fn test_query_path_info_exists() {
    let mut h = TestHarness::setup().await;
    let (nar, hash) = make_nar(b"pathinfo");
    let info = make_path_info(TEST_PATH_A, &nar, hash);
    h.store.seed(info, nar.clone());

    wire::write_u64(&mut h.stream, 26).await.unwrap(); // wopQueryPathInfo
    wire::write_string(&mut h.stream, TEST_PATH_A)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;

    // Response: bool(valid) + if valid: deriver, hex_nar_hash, refs, regtime, nar_size, ultimate, sigs, ca
    let valid = wire::read_bool(&mut h.stream).await.unwrap();
    assert!(valid, "path should be valid");
    let deriver = wire::read_string(&mut h.stream).await.unwrap();
    assert_eq!(deriver, "");
    let nar_hash_hex = wire::read_string(&mut h.stream).await.unwrap();
    assert_eq!(nar_hash_hex, hex::encode(hash), "nar hash should match");
    let refs = wire::read_strings(&mut h.stream).await.unwrap();
    assert!(refs.is_empty());
    let _regtime = wire::read_u64(&mut h.stream).await.unwrap();
    let nar_size = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(nar_size, nar.len() as u64);
    let _ultimate = wire::read_bool(&mut h.stream).await.unwrap();
    let _sigs = wire::read_strings(&mut h.stream).await.unwrap();
    let _ca = wire::read_string(&mut h.stream).await.unwrap();

    h.finish().await;
}

#[tokio::test]
async fn test_query_path_info_missing_returns_invalid() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 26).await.unwrap(); // wopQueryPathInfo
    wire::write_string(&mut h.stream, TEST_PATH_MISSING)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    // QueryPathInfo for missing path returns STDERR_LAST + valid=false
    // (not STDERR_ERROR — the Nix protocol uses valid=false here).
    drain_stderr_until_last(&mut h.stream).await;
    let valid = wire::read_bool(&mut h.stream).await.unwrap();
    assert!(!valid, "missing path should return valid=false");

    h.finish().await;
}

// ===========================================================================
// Opcode tests: QueryPathFromHashPart (29)
// ===========================================================================

#[tokio::test]
async fn test_query_path_from_hash_part_found() {
    let mut h = TestHarness::setup().await;
    let (nar, hash) = make_nar(b"hashpart");
    h.store.seed(make_path_info(TEST_PATH_A, &nar, hash), nar);

    // Hash part is the 32-char basename prefix
    let hash_part = "00000000000000000000000000000000";

    wire::write_u64(&mut h.stream, 29).await.unwrap(); // wopQueryPathFromHashPart
    wire::write_string(&mut h.stream, hash_part).await.unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let result = wire::read_string(&mut h.stream).await.unwrap();
    assert_eq!(result, TEST_PATH_A, "should return the full store path");

    h.finish().await;
}

#[tokio::test]
async fn test_query_path_from_hash_part_not_found() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 29).await.unwrap(); // wopQueryPathFromHashPart
    wire::write_string(&mut h.stream, "11111111111111111111111111111111")
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let result = wire::read_string(&mut h.stream).await.unwrap();
    assert_eq!(result, "", "not found should return empty string");

    h.finish().await;
}

// ===========================================================================
// Opcode tests: AddTempRoot (11)
// ===========================================================================

#[tokio::test]
async fn test_add_temp_root() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 11).await.unwrap(); // wopAddTempRoot
    wire::write_string(&mut h.stream, TEST_PATH_A)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let result = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(result, 1, "AddTempRoot always returns 1 (success)");

    h.finish().await;
}

// ===========================================================================
// Opcode tests: NarFromPath (38)
// ===========================================================================

#[tokio::test]
async fn test_nar_from_path_streams_chunks() {
    let mut h = TestHarness::setup().await;
    let (nar, hash) = make_nar(b"nar-from-path content");
    h.store
        .seed(make_path_info(TEST_PATH_A, &nar, hash), nar.clone());

    wire::write_u64(&mut h.stream, 38).await.unwrap(); // wopNarFromPath
    wire::write_string(&mut h.stream, TEST_PATH_A)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    // NarFromPath streams NAR data as STDERR_WRITE messages BEFORE STDERR_LAST.
    let msgs = drain_stderr_until_last(&mut h.stream).await;
    let mut received = Vec::new();
    for msg in msgs {
        match msg {
            StderrMessage::Write(data) => received.extend_from_slice(&data),
            other => panic!("unexpected message during NAR streaming: {other:?}"),
        }
    }
    assert_eq!(received, nar, "received NAR bytes should match seeded NAR");

    // No result data after STDERR_LAST for NarFromPath.
    h.finish().await;
}

#[tokio::test]
async fn test_nar_from_path_missing_returns_error() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 38).await.unwrap(); // wopNarFromPath
    wire::write_string(&mut h.stream, TEST_PATH_MISSING)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    let err = drain_stderr_expecting_error(&mut h.stream).await;
    assert!(
        !err.message().is_empty(),
        "error message should be non-empty"
    );

    h.finish().await;
}

// ===========================================================================
// Opcode tests: AddToStoreNar (39)
// ===========================================================================

#[tokio::test]
async fn test_add_to_store_nar_accepts_valid() {
    let mut h = TestHarness::setup().await;
    let (nar, hash) = make_nar(b"add-to-store-nar");

    wire::write_u64(&mut h.stream, 39).await.unwrap(); // wopAddToStoreNar
    wire::write_string(&mut h.stream, TEST_PATH_A)
        .await
        .unwrap(); // path
    wire::write_string(&mut h.stream, "").await.unwrap(); // deriver
    // narHash is hex-encoded SHA-256 (no algorithm prefix!)
    wire::write_string(&mut h.stream, &hex::encode(hash))
        .await
        .unwrap();
    wire::write_strings(&mut h.stream, &[]).await.unwrap(); // references
    wire::write_u64(&mut h.stream, 0).await.unwrap(); // registration_time
    wire::write_u64(&mut h.stream, nar.len() as u64)
        .await
        .unwrap(); // nar_size
    wire::write_bool(&mut h.stream, false).await.unwrap(); // ultimate
    wire::write_strings(&mut h.stream, &[]).await.unwrap(); // sigs
    wire::write_string(&mut h.stream, "").await.unwrap(); // ca
    wire::write_bool(&mut h.stream, false).await.unwrap(); // repair
    wire::write_bool(&mut h.stream, true).await.unwrap(); // dont_check_sigs
    // Framed NAR data: chunks of u64(len)+data, terminated by u64(0)
    wire::write_framed_stream(&mut h.stream, &nar, 8192)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    // AddToStoreNar has no result data after STDERR_LAST.

    // Verify the mock store received the PutPath call.
    let calls = h.store.put_calls.read().unwrap().clone();
    assert_eq!(calls.len(), 1, "store should receive one PutPath call");
    assert_eq!(calls[0].store_path, TEST_PATH_A);
    assert_eq!(calls[0].nar_hash, hash.to_vec());

    h.finish().await;
}

/// Gateway trusts client-declared narHash and passes it to the store.
/// Hash verification is the store's responsibility (validate.rs). This test
/// verifies the gateway passes the declared hash through unchanged.
#[tokio::test]
async fn test_add_to_store_nar_passes_declared_hash() {
    let mut h = TestHarness::setup().await;
    let (nar, _actual_hash) = make_nar(b"trust-test");
    let declared_hash = [0xABu8; 32]; // deliberately different from actual

    wire::write_u64(&mut h.stream, 39).await.unwrap();
    wire::write_string(&mut h.stream, TEST_PATH_A)
        .await
        .unwrap();
    wire::write_string(&mut h.stream, "").await.unwrap();
    wire::write_string(&mut h.stream, &hex::encode(declared_hash))
        .await
        .unwrap();
    wire::write_strings(&mut h.stream, &[]).await.unwrap();
    wire::write_u64(&mut h.stream, 0).await.unwrap();
    wire::write_u64(&mut h.stream, nar.len() as u64)
        .await
        .unwrap();
    wire::write_bool(&mut h.stream, false).await.unwrap();
    wire::write_strings(&mut h.stream, &[]).await.unwrap();
    wire::write_string(&mut h.stream, "").await.unwrap();
    wire::write_bool(&mut h.stream, false).await.unwrap();
    wire::write_bool(&mut h.stream, true).await.unwrap();
    wire::write_framed_stream(&mut h.stream, &nar, 8192)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;

    // Verify the DECLARED hash (not actual) was passed to the store.
    let calls = h.store.put_calls.read().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].nar_hash,
        declared_hash.to_vec(),
        "gateway should pass client-declared hash unchanged"
    );

    h.finish().await;
}

// ===========================================================================
// Opcode tests: AddMultipleToStore (44)
// ===========================================================================

#[tokio::test]
async fn test_add_multiple_to_store_batch() {
    let mut h = TestHarness::setup().await;
    let (nar_a, hash_a) = make_nar(b"multi-a");
    let (nar_b, hash_b) = make_nar(b"multi-b");

    // Build the inner framed payload: concatenated entries (NO count prefix;
    // handler iterates until end-of-buffer). Each entry has per-entry
    // metadata + inner-framed NAR.
    let mut inner = Vec::new();

    // Entry 1
    wire::write_string(&mut inner, TEST_PATH_A).await.unwrap();
    wire::write_string(&mut inner, "").await.unwrap(); // deriver
    wire::write_string(&mut inner, &hex::encode(hash_a))
        .await
        .unwrap();
    wire::write_strings(&mut inner, &[]).await.unwrap(); // refs
    wire::write_u64(&mut inner, 0).await.unwrap(); // regtime
    wire::write_u64(&mut inner, nar_a.len() as u64)
        .await
        .unwrap(); // nar_size
    wire::write_bool(&mut inner, false).await.unwrap(); // ultimate
    wire::write_strings(&mut inner, &[]).await.unwrap(); // sigs
    wire::write_string(&mut inner, "").await.unwrap(); // ca
    wire::write_framed_stream(&mut inner, &nar_a, 8192)
        .await
        .unwrap();

    // Entry 2
    let test_path_b = "/nix/store/22222222222222222222222222222222-multi-b";
    wire::write_string(&mut inner, test_path_b).await.unwrap();
    wire::write_string(&mut inner, "").await.unwrap();
    wire::write_string(&mut inner, &hex::encode(hash_b))
        .await
        .unwrap();
    wire::write_strings(&mut inner, &[]).await.unwrap();
    wire::write_u64(&mut inner, 0).await.unwrap();
    wire::write_u64(&mut inner, nar_b.len() as u64)
        .await
        .unwrap();
    wire::write_bool(&mut inner, false).await.unwrap();
    wire::write_strings(&mut inner, &[]).await.unwrap();
    wire::write_string(&mut inner, "").await.unwrap();
    wire::write_framed_stream(&mut inner, &nar_b, 8192)
        .await
        .unwrap();

    // Send opcode + outer framing
    wire::write_u64(&mut h.stream, 44).await.unwrap(); // wopAddMultipleToStore
    wire::write_bool(&mut h.stream, false).await.unwrap(); // repair
    wire::write_bool(&mut h.stream, true).await.unwrap(); // dont_check_sigs
    wire::write_framed_stream(&mut h.stream, &inner, 8192)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;

    let calls = h.store.put_calls.read().unwrap().clone();
    assert_eq!(calls.len(), 2, "store should receive 2 PutPath calls");
    let paths: Vec<&str> = calls.iter().map(|c| c.store_path.as_str()).collect();
    assert!(paths.contains(&TEST_PATH_A));
    assert!(paths.contains(&test_path_b));

    h.finish().await;
}

// ===========================================================================
// Stub opcode tests: AddSignatures (37), RegisterDrvOutput (42),
// QueryRealisation (43)
// ===========================================================================

#[tokio::test]
async fn test_add_signatures_stub_returns_success() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 37).await.unwrap(); // wopAddSignatures
    wire::write_string(&mut h.stream, TEST_PATH_A)
        .await
        .unwrap();
    wire::write_strings(&mut h.stream, &["sig:fake".to_string()])
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let result = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(result, 1, "AddSignatures stub should return 1 (success)");

    h.finish().await;
}

#[tokio::test]
async fn test_register_drv_output_stub_reads_and_returns() {
    let mut h = TestHarness::setup().await;

    let realisation_json = r#"{"id":"sha256:abc!out","outPath":"/nix/store/xyz","signatures":[],"dependentRealisations":{}}"#;
    wire::write_u64(&mut h.stream, 42).await.unwrap(); // wopRegisterDrvOutput
    wire::write_string(&mut h.stream, realisation_json)
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    // RegisterDrvOutput stub has no result data.

    h.finish().await;
}

#[tokio::test]
async fn test_query_realisation_stub_returns_empty() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 43).await.unwrap(); // wopQueryRealisation
    wire::write_string(&mut h.stream, "sha256:abc!out")
        .await
        .unwrap();
    h.stream.flush().await.unwrap();

    drain_stderr_until_last(&mut h.stream).await;
    let count = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(count, 0, "QueryRealisation stub should return empty set");

    h.finish().await;
}

// ===========================================================================
// Unknown opcode test
// ===========================================================================

#[tokio::test]
async fn test_unknown_opcode_returns_stderr_error() {
    let mut h = TestHarness::setup().await;

    wire::write_u64(&mut h.stream, 99).await.unwrap(); // unknown opcode
    h.stream.flush().await.unwrap();

    let err = drain_stderr_expecting_error(&mut h.stream).await;
    assert!(
        err.message().contains("99")
            || err.message().to_lowercase().contains("unknown")
            || err.message().to_lowercase().contains("unimplemented"),
        "error should mention unknown/unimplemented opcode: {}",
        err.message()
    );

    h.finish().await;
}
