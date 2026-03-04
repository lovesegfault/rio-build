//! Mock gRPC services and server spawn helpers for tests.
//!
//! `MockStore` stores NAR bytes in-memory, records PutPath calls, and supports
//! prefix-match QueryPathInfo (for hash-part lookups).
//!
//! `MockScheduler` has a configurable `MockSchedulerOutcome` and records both
//! SubmitBuild and CancelBuild calls.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tonic::{Request, Response, Status, Streaming};

use rio_proto::types;
use rio_proto::validated::ValidatedPathInfo;
use rio_proto::{SchedulerService, SchedulerServiceServer, StoreService, StoreServiceServer};

// ============================================================================
// MockStore
// ============================================================================

/// `(PathInfo, NAR bytes)` — stored value type for [`MockStore::paths`].
type StoredPath = (types::PathInfo, Vec<u8>);

/// `(drv_hash, output_name)` — key type for [`MockStore::realisations`].
/// Alias silences clippy::type_complexity on the nested generic field.
type RealisationKey = (Vec<u8>, String);

/// In-memory store: `store_path -> (PathInfo, nar_bytes)`.
///
/// Records PutPath calls and supports prefix-match QueryPathInfo (for
/// hash-part lookups via QueryPathFromHashPart).
#[derive(Clone, Default)]
pub struct MockStore {
    /// store_path -> (PathInfo, NAR bytes)
    pub paths: Arc<RwLock<HashMap<String, StoredPath>>>,
    /// Every PutPath metadata received (for assertions on upload count/contents).
    pub put_calls: Arc<RwLock<Vec<types::PathInfo>>>,
    /// If > 0, put_path decrements and returns Unavailable. For retry tests.
    pub fail_next_puts: Arc<AtomicU32>,
    /// If true, find_missing_paths returns Unavailable. For scheduler
    /// cache-check error-path tests.
    pub fail_find_missing: Arc<AtomicBool>,
    /// If true, query_path_info returns Unavailable. For worker input-fetch
    /// error-path tests (distinguishing real gRPC errors from NotFound).
    pub fail_query_path_info: Arc<AtomicBool>,
    /// CA realisations: (drv_hash, output_name) -> Realisation.
    /// For E4's gateway wopRegisterDrvOutput/wopQueryRealisation tests.
    pub realisations: Arc<RwLock<HashMap<RealisationKey, types::Realisation>>>,
}

impl MockStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a path into the store. For tests that want a pre-populated store.
    ///
    /// Takes `ValidatedPathInfo` (matching what test fixtures produce) and
    /// converts to raw `PathInfo` internally — MockStore mocks the wire layer,
    /// which speaks raw proto types.
    pub fn seed(&self, info: ValidatedPathInfo, nar: Vec<u8>) {
        let store_path = info.store_path.to_string();
        self.paths
            .write()
            .unwrap()
            .insert(store_path, (info.into(), nar));
    }
}

#[tonic::async_trait]
impl StoreService for MockStore {
    async fn put_path(
        &self,
        request: Request<Streaming<types::PutPathRequest>>,
    ) -> Result<Response<types::PutPathResponse>, Status> {
        // Injected failure for retry tests. fetch_update returns Err when
        // the closure returns None (counter is 0) — i.e., no failure to inject.
        if self
            .fail_next_puts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok()
        {
            return Err(Status::unavailable("mock: injected put failure"));
        }
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
        // Drain NAR chunks. Trailer carries hash/size (metadata hash is
        // always empty — hash-upfront was removed). Mirrors real-store
        // behavior so upload tests see the right recorded PathInfo.
        let mut nar = Vec::new();
        let mut info = info;
        while let Some(msg) = stream.message().await? {
            match msg.msg {
                Some(types::put_path_request::Msg::NarChunk(chunk)) => {
                    nar.extend_from_slice(&chunk);
                }
                Some(types::put_path_request::Msg::Trailer(t)) => {
                    info.nar_hash = t.nar_hash;
                    info.nar_size = t.nar_size;
                }
                _ => {}
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
        if self.fail_query_path_info.load(Ordering::SeqCst) {
            return Err(Status::unavailable(
                "mock: injected query_path_info failure",
            ));
        }
        let store_path = request.into_inner().store_path;
        let paths = self.paths.read().unwrap();
        // Exact match only. Hash-part prefix lookups go through
        // query_path_from_hash_part (below), not here — the gateway
        // switched to that dedicated RPC in phase2c.
        paths
            .get(&store_path)
            .map(|(info, _)| Response::new(info.clone()))
            .ok_or_else(|| Status::not_found(format!("not found: {store_path}")))
    }

    async fn find_missing_paths(
        &self,
        request: Request<types::FindMissingPathsRequest>,
    ) -> Result<Response<types::FindMissingPathsResponse>, Status> {
        if self.fail_find_missing.load(Ordering::SeqCst) {
            return Err(Status::unavailable("mock: injected find_missing failure"));
        }
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
        request: Request<types::ContentLookupRequest>,
    ) -> Result<Response<types::ContentLookupResponse>, Status> {
        let content_hash = request.into_inner().content_hash;
        // Scan stored paths for a nar_hash match. O(n) is fine for a
        // mock; real store has a PG index. First match wins (same
        // semantics as the real LIMIT 1 — multiple paths with same
        // content are all correct answers).
        let paths = self.paths.read().unwrap();
        for (store_path, (info, _nar)) in paths.iter() {
            if info.nar_hash == content_hash {
                return Ok(Response::new(types::ContentLookupResponse {
                    store_path: store_path.clone(),
                    info: Some(info.clone()),
                }));
            }
        }
        Ok(Response::new(types::ContentLookupResponse {
            store_path: String::new(),
            info: None,
        }))
    }

    async fn query_path_from_hash_part(
        &self,
        request: Request<types::QueryPathFromHashPartRequest>,
    ) -> Result<Response<types::PathInfo>, Status> {
        let hash_part = request.into_inner().hash_part;
        // Prefix-match: /nix/store/{hash}-...
        // This is the same prefix-lookup query_path_info already did for the
        // gateway's old workaround; now it's the dedicated RPC. The fallthrough
        // prefix-scan in query_path_info stays (some tests may still hit it)
        // but new tests should use this RPC.
        let prefix = format!("/nix/store/{hash_part}-");
        let paths = self.paths.read().unwrap();
        for (k, (info, _)) in paths.iter() {
            if k.starts_with(&prefix) {
                return Ok(Response::new(info.clone()));
            }
        }
        Err(Status::not_found(format!(
            "not found: hash_part {hash_part}"
        )))
    }

    async fn add_signatures(
        &self,
        request: Request<types::AddSignaturesRequest>,
    ) -> Result<Response<types::AddSignaturesResponse>, Status> {
        let req = request.into_inner();
        let mut paths = self.paths.write().unwrap();
        match paths.get_mut(&req.store_path) {
            Some((info, _)) => {
                info.signatures.extend(req.signatures);
                Ok(Response::new(types::AddSignaturesResponse {}))
            }
            None => Err(Status::not_found(format!("not found: {}", req.store_path))),
        }
    }

    async fn register_realisation(
        &self,
        request: Request<types::RegisterRealisationRequest>,
    ) -> Result<Response<types::RegisterRealisationResponse>, Status> {
        let r = request
            .into_inner()
            .realisation
            .ok_or_else(|| Status::invalid_argument("realisation required"))?;
        // Key by (drv_hash, output_name) — mirrors the real store's PK.
        let key = (r.drv_hash.clone(), r.output_name.clone());
        self.realisations.write().unwrap().insert(key, r);
        Ok(Response::new(types::RegisterRealisationResponse {}))
    }

    async fn query_realisation(
        &self,
        request: Request<types::QueryRealisationRequest>,
    ) -> Result<Response<types::Realisation>, Status> {
        let req = request.into_inner();
        let key = (req.drv_hash, req.output_name.clone());
        match self.realisations.read().unwrap().get(&key) {
            Some(r) => Ok(Response::new(r.clone())),
            None => Err(Status::not_found(format!(
                "no realisation for {}",
                req.output_name
            ))),
        }
    }
}

// ============================================================================
// MockScheduler
// ============================================================================

/// Configurable scheduler behavior for a single SubmitBuild stream.
#[derive(Clone, Default)]
pub struct MockSchedulerOutcome {
    /// If set, SubmitBuild immediately fails with this status code.
    pub submit_error: Option<tonic::Code>,
    /// If set, SubmitBuild sends BuildCompleted after BuildStarted.
    pub send_completed: bool,
    /// If set, SubmitBuild sends BuildFailed after BuildStarted.
    pub send_failed: bool,
    /// If set, the stream is closed immediately after BuildStarted (no
    /// terminal event). Simulates scheduler disconnect mid-build.
    pub close_stream_early: bool,
    /// If Some, send these events verbatim (ignoring the bool flags above)
    /// then close the stream. Empty build_id / zero sequence are
    /// auto-populated so tests only need to set the `event` oneof.
    pub scripted_events: Option<Vec<types::BuildEvent>>,
}

/// Mock scheduler that records SubmitBuild + CancelBuild calls and has a
/// configurable outcome for SubmitBuild streams.
#[derive(Clone, Default)]
pub struct MockScheduler {
    pub outcome: Arc<RwLock<MockSchedulerOutcome>>,
    /// Full SubmitBuild requests received (for inspecting DAG contents).
    pub submit_calls: Arc<RwLock<Vec<types::SubmitBuildRequest>>>,
    /// CancelBuild calls received: (build_id, reason).
    pub cancel_calls: Arc<RwLock<Vec<(String, String)>>>,
}

impl MockScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_outcome(&self, outcome: MockSchedulerOutcome) {
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
        self.submit_calls.write().unwrap().push(req);

        let outcome = self.outcome.read().unwrap().clone();
        if let Some(code) = outcome.submit_error {
            return Err(Status::new(code, "mock scheduler error"));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let build_id = "test-build-00000000-1111-2222-3333-444444444444".to_string();

        // Scripted mode: send events verbatim, auto-fill build_id/sequence, close.
        if let Some(events) = outcome.scripted_events {
            tokio::spawn(async move {
                for (seq, mut ev) in events.into_iter().enumerate() {
                    if ev.build_id.is_empty() {
                        ev.build_id = build_id.clone();
                    }
                    if ev.sequence == 0 {
                        ev.sequence = seq as u64;
                    }
                    if tx.send(Ok(ev)).await.is_err() {
                        return;
                    }
                }
                // tx drops → stream ends
            });
            return Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
                rx,
            )));
        }

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
            } else if outcome.close_stream_early {
                // Drop tx immediately: stream ends without a terminal event.
                drop(tx);
            } else {
                // Keep open so gateway blocks on build events until client disconnects.
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
        request: Request<types::CancelBuildRequest>,
    ) -> Result<Response<types::CancelBuildResponse>, Status> {
        let req = request.into_inner();
        self.cancel_calls
            .write()
            .unwrap()
            .push((req.build_id, req.reason));
        Ok(Response::new(types::CancelBuildResponse {
            cancelled: true,
        }))
    }
}

// ============================================================================
// spawn helpers
// ============================================================================

/// Spawn an in-process tonic server on a random port. Returns `(addr, handle)`.
///
/// Uses `yield_now()` for synchronization — the listener is already bound
/// and accepting before `spawn` returns, so a single yield is sufficient to
/// let the server task enter its accept loop. **Replaces the `sleep(50ms)`**
/// that ~6 test modules were using after their own hand-rolled spawn,
/// which was both slower and no more correct.
///
/// Accepts a prebuilt [`tonic::transport::server::Router`] so callers can
/// compose any number of services:
/// ```ignore
/// let router = Server::builder()
///     .add_service(FooServer::new(foo))
///     .add_service(BarServer::new(bar));
/// let (addr, handle) = spawn_grpc_server(router).await;
/// ```
pub async fn spawn_grpc_server(
    router: tonic::transport::server::Router,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let handle = tokio::spawn(async move {
        router
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .expect("in-process gRPC server");
    });
    tokio::task::yield_now().await;
    (addr, handle)
}

/// Spawn a MockStore on an ephemeral port. Returns `(store, addr, handle)`.
pub async fn spawn_mock_store()
-> anyhow::Result<(MockStore, SocketAddr, tokio::task::JoinHandle<()>)> {
    let store = MockStore::new();
    let router = Server::builder().add_service(StoreServiceServer::new(store.clone()));
    let (addr, handle) = spawn_grpc_server(router).await;
    Ok((store, addr, handle))
}

/// Spawn a MockScheduler on an ephemeral port. Returns `(scheduler, addr, handle)`.
pub async fn spawn_mock_scheduler()
-> anyhow::Result<(MockScheduler, SocketAddr, tokio::task::JoinHandle<()>)> {
    let sched = MockScheduler::new();
    let router = Server::builder().add_service(SchedulerServiceServer::new(sched.clone()));
    let (addr, handle) = spawn_grpc_server(router).await;
    Ok((sched, addr, handle))
}
