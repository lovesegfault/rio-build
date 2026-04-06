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

use tokio_stream::StreamExt;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tonic::{Request, Response, Status, Streaming};

use rio_proto::types;
use rio_proto::validated::ValidatedPathInfo;
use rio_proto::{
    AdminService, AdminServiceServer, SchedulerService, SchedulerServiceServer, StoreService,
    StoreServiceServer,
};

// ============================================================================
// MockStore
// ============================================================================

/// `(PathInfo, NAR bytes)` — stored value type for [`MockStore::paths`].
type StoredPath = (types::PathInfo, Vec<u8>);

/// `(drv_hash, output_name)` — key type for [`MockStore::realisations`].
/// Alias silences clippy::type_complexity on the nested generic field.
type RealisationKey = (Vec<u8>, String);

/// `(used_bytes, limit_bytes)` — value type for
/// [`MockStore::tenant_quotas`]. Alias silences clippy::type_complexity
/// on the nested `Arc<RwLock<HashMap<_, _>>>`.
type TenantQuotaEntry = (u64, Option<u64>);

/// In-memory store: `store_path -> (PathInfo, nar_bytes)`.
///
/// Records PutPath calls and supports prefix-match QueryPathInfo (for
/// hash-part lookups via QueryPathFromHashPart).
#[derive(Clone)]
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
    /// If true, get_path returns Unavailable. For FUSE fetch error-path tests.
    pub fail_get_path: Arc<AtomicBool>,
    /// If true, get_path returns garbage non-NAR bytes in the NarChunk.
    /// For NAR parse error tests (FUSE fetch → EIO on parse failure).
    pub get_path_garbage: Arc<AtomicBool>,
    /// If `get_path_gate_armed` is true, `GetPath` awaits this Notify before
    /// responding. Tests arm it at construction, spawn concurrent callers,
    /// then `.notify_waiters()` to release all at once. Distinct from
    /// `fail_get_path` (which errors immediately) — this holds-then-succeeds.
    pub get_path_gate: Arc<tokio::sync::Notify>,
    /// Whether `get_path_gate` is armed. When false, `GetPath` ignores the
    /// gate (backwards-compatible with existing tests).
    pub get_path_gate_armed: Arc<AtomicBool>,
    /// If true, `content_lookup` hangs indefinitely (awaits a Notify
    /// that never fires). For scheduler CA-compare timeout tests:
    /// proves the `tokio::time::timeout(DEFAULT_GRPC_TIMEOUT, ...)`
    /// wrapper is load-bearing — without it, a slow/hung store blocks
    /// the scheduler's completion path forever.
    pub content_lookup_hang: Arc<AtomicBool>,
    /// If true, `content_lookup` returns Unavailable. For scheduler
    /// CA-compare error-path tests (`Err(Status)` arm — counts as
    /// miss, doesn't block completion).
    pub fail_content_lookup: Arc<AtomicBool>,
    /// Number of `content_lookup` calls received (success, fail, or
    /// hang — incremented on entry). For scheduler CA-compare
    /// short-circuit tests: prove the loop broke after N calls
    /// instead of iterating all outputs.
    pub content_lookup_calls: Arc<AtomicU32>,
    /// CA realisations: (drv_hash, output_name) -> Realisation.
    /// Used by gateway wopRegisterDrvOutput/wopQueryRealisation tests.
    pub realisations: Arc<RwLock<HashMap<RealisationKey, types::Realisation>>>,
    /// Per-tenant quota: tenant_name -> (used_bytes, limit_bytes).
    /// Tests seed this directly; TenantQuota reads it verbatim.
    /// Absent key → NOT_FOUND (gateway treats as no-quota).
    pub tenant_quotas: Arc<RwLock<HashMap<String, TenantQuotaEntry>>>,
}

impl Default for MockStore {
    fn default() -> Self {
        Self {
            paths: Arc::default(),
            put_calls: Arc::default(),
            fail_next_puts: Arc::default(),
            fail_find_missing: Arc::default(),
            fail_query_path_info: Arc::default(),
            fail_get_path: Arc::default(),
            get_path_garbage: Arc::default(),
            get_path_gate: Arc::new(tokio::sync::Notify::new()),
            get_path_gate_armed: Arc::default(),
            content_lookup_hang: Arc::default(),
            fail_content_lookup: Arc::default(),
            content_lookup_calls: Arc::default(),
            realisations: Arc::default(),
            tenant_quotas: Arc::default(),
        }
    }
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

    /// Seed a path with a single-file NAR built from `content`.
    ///
    /// Wraps the `make_nar + make_path_info + seed` combo that
    /// every wire_opcodes test does. Returns (nar_bytes, nar_hash) for
    /// tests that need to assert on wire-level bytes.
    pub fn seed_with_content(&self, path: &str, content: &[u8]) -> (Vec<u8>, [u8; 32]) {
        let (nar, hash) = crate::fixtures::make_nar(content);
        self.seed(
            crate::fixtures::make_path_info(path, &nar, hash),
            nar.clone(),
        );
        (nar, hash)
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
        // Mirror real store (put_path.rs:206-211): hash-upfront was removed
        // pre-phase3a. A non-empty metadata.nar_hash means an un-updated
        // client. The real store rejects it; the mock must too, or a
        // regression in chunk_nar_for_put / do_upload_streaming that
        // stops zeroing the metadata hash goes green.
        if !info.nar_hash.is_empty() {
            return Err(Status::invalid_argument(
                "PutPath metadata.nar_hash must be empty (send hash in trailer)",
            ));
        }
        // Drain NAR chunks + trailer. Hash the chunks as they arrive and
        // verify against the trailer — mirrors real store's independent
        // digest check. Without this, a test that sends a bogus trailer
        // hash goes green against the mock but red against rio-store.
        use sha2::{Digest, Sha256};
        let mut nar = Vec::new();
        let mut hasher = Sha256::new();
        let mut trailer: Option<types::PutPathTrailer> = None;
        let mut info = info;
        while let Some(msg) = stream.message().await? {
            match msg.msg {
                Some(types::put_path_request::Msg::NarChunk(chunk)) => {
                    hasher.update(&chunk);
                    nar.extend_from_slice(&chunk);
                }
                Some(types::put_path_request::Msg::Trailer(t)) => {
                    trailer = Some(t);
                }
                _ => {}
            }
        }
        // Real store rejects missing-trailer as a protocol violation
        // (truncated stream / client gave up mid-upload). Current mock
        // silently accepted — that's how test_upload_output_nar_serialize_error
        // passed: ENOENT in spawn_blocking → channel drops → no trailer →
        // mock says Ok(created=true). The worker's OWN dump_task error
        // saves the test, but the mock's "ok" was a lie.
        let Some(t) = trailer else {
            return Err(Status::invalid_argument(
                "PutPath stream closed without trailer",
            ));
        };
        let computed: [u8; 32] = hasher.finalize().into();
        if computed.as_slice() != t.nar_hash.as_slice() {
            return Err(Status::invalid_argument(format!(
                "PutPath trailer hash mismatch: computed {}, trailer {}",
                hex::encode(computed),
                hex::encode(&t.nar_hash),
            )));
        }
        info.nar_hash = t.nar_hash;
        info.nar_size = t.nar_size;
        self.put_calls.write().unwrap().push(info.clone());
        let store_path = info.store_path.clone();
        self.paths.write().unwrap().insert(store_path, (info, nar));
        Ok(Response::new(types::PutPathResponse { created: true }))
    }

    async fn put_path_batch(
        &self,
        request: Request<Streaming<types::PutPathBatchRequest>>,
    ) -> Result<Response<types::PutPathBatchResponse>, Status> {
        // Mirror put_path's logic, routed by output_index. Populates the
        // same `put_calls` list (one entry per output) so worker tests
        // asserting `put_calls.len() == N` work regardless of whether
        // the worker chose batch or independent PutPath.
        //
        // Atomicity: on ANY validation failure, nothing is recorded
        // (matches real store's one-transaction semantics). The mock
        // can't race with itself the way the real PG-backed store can,
        // so "commit all at the end" is sufficient.
        use sha2::{Digest, Sha256};
        use std::collections::BTreeMap;

        // `fail_next_puts` injection — decrement once for the whole
        // batch (not per output). Batch is one RPC.
        if self
            .fail_next_puts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok()
        {
            return Err(Status::unavailable("mock: injected batch put failure"));
        }

        let mut stream = request.into_inner();
        let mut outs: BTreeMap<
            u32,
            (
                Option<types::PathInfo>,
                Vec<u8>,
                Sha256,
                Option<types::PutPathTrailer>,
            ),
        > = BTreeMap::new();

        while let Some(msg) = stream.message().await? {
            let idx = msg.output_index;
            let Some(inner) = msg.inner.and_then(|i| i.msg) else {
                return Err(Status::invalid_argument("batch: inner must be set"));
            };
            let (info, nar, hasher, trailer) = outs
                .entry(idx)
                .or_insert_with(|| (None, Vec::new(), Sha256::new(), None));
            match inner {
                types::put_path_request::Msg::Metadata(m) => {
                    let i = m
                        .info
                        .ok_or_else(|| Status::invalid_argument("batch: missing PathInfo"))?;
                    if !i.nar_hash.is_empty() {
                        return Err(Status::invalid_argument(
                            "batch: metadata.nar_hash must be empty",
                        ));
                    }
                    *info = Some(i);
                }
                types::put_path_request::Msg::NarChunk(chunk) => {
                    hasher.update(&chunk);
                    nar.extend_from_slice(&chunk);
                }
                types::put_path_request::Msg::Trailer(t) => *trailer = Some(t),
            }
        }

        // Validate all BEFORE recording any (atomicity).
        let mut staged: Vec<(u32, types::PathInfo, Vec<u8>)> = Vec::new();
        for (idx, (info, nar, hasher, trailer)) in outs {
            let mut info = info.ok_or_else(|| {
                Status::invalid_argument(format!("batch: output {idx} no metadata"))
            })?;
            let t = trailer.ok_or_else(|| {
                Status::invalid_argument(format!("batch: output {idx} no trailer"))
            })?;
            let computed: [u8; 32] = hasher.finalize().into();
            if computed.as_slice() != t.nar_hash.as_slice() {
                return Err(Status::invalid_argument(format!(
                    "batch: output {idx} hash mismatch"
                )));
            }
            info.nar_hash = t.nar_hash;
            info.nar_size = t.nar_size;
            staged.push((idx, info, nar));
        }

        // Commit all.
        let max_idx = staged.iter().map(|(i, _, _)| *i).max().unwrap_or(0);
        let mut created = vec![false; max_idx as usize + 1];
        for (idx, info, nar) in staged {
            self.put_calls.write().unwrap().push(info.clone());
            let store_path = info.store_path.clone();
            self.paths.write().unwrap().insert(store_path, (info, nar));
            created[idx as usize] = true;
        }
        Ok(Response::new(types::PutPathBatchResponse { created }))
    }

    type GetPathStream =
        tokio_stream::wrappers::ReceiverStream<Result<types::GetPathResponse, Status>>;

    async fn get_path(
        &self,
        request: Request<types::GetPathRequest>,
    ) -> Result<Response<Self::GetPathStream>, Status> {
        if self.fail_get_path.load(Ordering::SeqCst) {
            return Err(Status::unavailable("mock: injected get_path failure"));
        }
        // Slow-fetch gate: park until the test releases us. The fetcher thread
        // blocks inside block_on here, which is exactly the condition the
        // FUSE concurrent-waiter test needs — waiters park on the condvar
        // past at least one WAIT_SLICE before the gate opens.
        if self.get_path_gate_armed.load(Ordering::SeqCst) {
            self.get_path_gate.notified().await;
        }
        let store_path = request.into_inner().store_path;
        // Garbage mode: return a stream with valid PathInfo but garbage NAR
        // bytes, so collect_nar_stream succeeds but nar::parse fails.
        if self.get_path_garbage.load(Ordering::SeqCst) {
            let entry = self.paths.read().unwrap().get(&store_path).cloned();
            if let Some((info, _real_nar)) = entry {
                let (tx, rx) = tokio::sync::mpsc::channel(4);
                tokio::spawn(async move {
                    let _ = tx
                        .send(Ok(types::GetPathResponse {
                            msg: Some(types::get_path_response::Msg::Info(info)),
                        }))
                        .await;
                    let _ = tx
                        .send(Ok(types::GetPathResponse {
                            msg: Some(types::get_path_response::Msg::NarChunk(
                                b"garbage-not-a-NAR".to_vec(),
                            )),
                        }))
                        .await;
                });
                return Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
                    rx,
                )));
            }
            return Err(Status::not_found(format!("not found: {store_path}")));
        }
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
        // uses that dedicated RPC.
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
        self.content_lookup_calls.fetch_add(1, Ordering::SeqCst);
        if self.content_lookup_hang.load(Ordering::SeqCst) {
            // Hang forever. Scheduler CA-compare tests use this to
            // prove the DEFAULT_GRPC_TIMEOUT wrapper is load-bearing.
            // futures::future::pending() — never resolves.
            std::future::pending::<()>().await;
        }
        if self.fail_content_lookup.load(Ordering::SeqCst) {
            return Err(Status::unavailable("mock: injected content_lookup failure"));
        }
        let req = request.into_inner();
        // Scan stored paths for a nar_hash match. O(n) is fine for a
        // mock; real store has a PG index. First match wins (same
        // semantics as the real LIMIT 1 — multiple paths with same
        // content are all correct answers).
        //
        // Respects exclude_store_path (store.content.self-exclude):
        // skip rows whose store_path equals the exclude. Empty
        // exclude = no filter (proto3 empty-string sentinel). The
        // real store's PG query has `AND n.store_path != $2`; this
        // mirrors it so scheduler CA-compare tests can assert
        // first-build → miss, second-build → match.
        let paths = self.paths.read().unwrap();
        for (store_path, (info, _nar)) in paths.iter() {
            if info.nar_hash == req.content_hash
                && (req.exclude_store_path.is_empty() || store_path != &req.exclude_store_path)
            {
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
        // Prefix-match: find a stored path starting with /nix/store/{hash}-.
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
        // Mirror the real store's validation at rio-store/src/grpc/mod.rs:464.
        // Without this, the mock accepts basenames that the real store rejects,
        // masking wire-format bugs (see phase4a §1.6: gateway passed basename
        // verbatim, real store returned invalid_argument, mock swallowed it).
        let _ = rio_nix::store_path::StorePath::parse(&r.output_path)
            .map_err(|e| Status::invalid_argument(format!("mock: invalid store path: {e}")))?;
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

    async fn tenant_quota(
        &self,
        request: Request<types::TenantQuotaRequest>,
    ) -> Result<Response<types::TenantQuotaResponse>, Status> {
        // Mirror the real store's NormalizedName normalization (trim +
        // empty-reject) so dual-mode tests that accidentally pass ""
        // get the same InvalidArgument. The mock doesn't depend on
        // rio-common, so inline the same semantics.
        let raw = request.into_inner().tenant_name;
        let name = raw.trim();
        if name.is_empty() {
            return Err(Status::invalid_argument("mock: tenant_name is empty"));
        }
        match self.tenant_quotas.read().unwrap().get(name) {
            Some(&(used, limit)) => Ok(Response::new(types::TenantQuotaResponse {
                used_bytes: used,
                limit_bytes: limit,
            })),
            None => Err(Status::not_found(format!("mock: unknown tenant: {name}"))),
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
    /// Inject `Err(Status)` into the SubmitBuild stream after sending N
    /// scripted events. Only applies in scripted mode. For gateway
    /// reconnect-on-transport-error tests.
    pub error_after_n: Option<(usize, tonic::Code)>,
    /// Sleep this long between each scripted event. Only applies in
    /// scripted mode. For mid-opcode-disconnect tests: gives the test
    /// time to drop the client stream while the scheduler is still
    /// sending Log events, so the gateway's next stderr.log write
    /// gets BrokenPipe → `StreamProcessError::Wire`. Without this,
    /// scripted events flood the mpsc channel synchronously and the
    /// race between client-drop and stream-EOF is nondeterministic.
    pub scripted_event_interval: Option<std::time::Duration>,
    /// If Some, WatchBuild returns a stream of these events (same
    /// build_id/sequence auto-fill as SubmitBuild scripted mode).
    /// For gateway reconnect tests: SubmitBuild stream errors →
    /// gateway calls WatchBuild → this stream delivers the remainder.
    pub watch_scripted_events: Option<Vec<types::BuildEvent>>,
    /// How many times watch_build fails with Unavailable before
    /// succeeding (or returning not_found if watch_scripted_events
    /// is None). Decremented on each call. For reconnect-exhausted tests.
    pub watch_fail_count: Arc<AtomicU32>,
    /// If true, SubmitBuild omits the `x-rio-build-id` initial-metadata
    /// header. For exercising the gateway's legacy first-event-peek
    /// fallback. Default false (header set — matches phase4a+ scheduler).
    /// `#[derive(Default)]` gives `bool::default() = false`, so existing
    /// tests using `..Default::default()` need no changes.
    pub suppress_build_id_header: bool,
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
    /// WatchBuild calls received: (build_id, since_sequence). For asserting
    /// the gateway's reconnect sent the correct resume point (see
    /// `r[gw.reconnect.since-seq]`).
    pub watch_calls: Arc<RwLock<Vec<(String, u64)>>>,
    /// ResolveTenant calls received: tenant_name. For asserting the
    /// gateway called resolve during SSH auth (JWT mint path).
    pub resolve_tenant_calls: Arc<RwLock<Vec<String>>>,
    /// UUID the mock returns for ResolveTenant. `None` → returns
    /// InvalidArgument "unknown tenant" (for testing the gateway's
    /// graceful-degrade path). Default is `None` — tests that don't
    /// set this get the unknown-tenant behavior, which is safe for
    /// existing tests that never call ResolveTenant anyway.
    pub resolve_tenant_uuid: Arc<RwLock<Option<uuid::Uuid>>>,
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
        // Clone before any spawn moves build_id. None = suppress
        // (legacy-scheduler simulation for fallback-path tests).
        let build_id_for_header = (!outcome.suppress_build_id_header).then(|| build_id.clone());

        // Scripted mode: send events verbatim, auto-fill build_id/sequence, close.
        if let Some(events) = outcome.scripted_events {
            let error_after_n = outcome.error_after_n;
            let interval = outcome.scripted_event_interval;
            let n_events = events.len();
            tokio::spawn(async move {
                for (seq, mut ev) in events.into_iter().enumerate() {
                    if let Some(d) = interval {
                        tokio::time::sleep(d).await;
                    }
                    // Error injection: after sending N events, send an Err(Status)
                    // into the stream. Gateway's process_build_events maps this
                    // to StreamProcessError::Transport → triggers the reconnect loop.
                    if let Some((n, code)) = error_after_n
                        && seq == n
                    {
                        let _ = tx
                            .send(Err(Status::new(code, "mock: injected stream error")))
                            .await;
                        return;
                    }
                    if ev.build_id.is_empty() {
                        ev.build_id = build_id.clone();
                    }
                    if ev.sequence == 0 {
                        ev.sequence = (seq as u64) + 1;
                    }
                    if tx.send(Ok(ev)).await.is_err() {
                        return;
                    }
                }
                // If error_after_n >= events.len(), fire after the last event.
                if let Some((n, code)) = error_after_n
                    && n >= n_events
                {
                    let _ = tx
                        .send(Err(Status::new(code, "mock: injected stream error")))
                        .await;
                }
                // tx drops → stream ends
            });
            let mut resp = Response::new(tokio_stream::wrappers::ReceiverStream::new(rx));
            if let Some(id) = build_id_for_header {
                resp.metadata_mut().insert(
                    rio_proto::BUILD_ID_HEADER,
                    id.parse().expect("test build_id is ASCII"),
                );
            }
            return Ok(resp);
        }

        tokio::spawn(async move {
            let _ = tx
                .send(Ok(types::BuildEvent {
                    build_id: build_id.clone(),
                    sequence: 1,
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
                        sequence: 2,
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
                        sequence: 2,
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

        let mut resp = Response::new(tokio_stream::wrappers::ReceiverStream::new(rx));
        if let Some(id) = build_id_for_header {
            resp.metadata_mut().insert(
                rio_proto::BUILD_ID_HEADER,
                id.parse().expect("test build_id is ASCII"),
            );
        }
        Ok(resp)
    }

    type WatchBuildStream =
        tokio_stream::wrappers::ReceiverStream<Result<types::BuildEvent, Status>>;

    async fn watch_build(
        &self,
        request: Request<types::WatchBuildRequest>,
    ) -> Result<Response<Self::WatchBuildStream>, Status> {
        let req = request.into_inner();
        let since = req.since_sequence;
        self.watch_calls
            .write()
            .unwrap()
            .push((req.build_id, since));

        let outcome = self.outcome.read().unwrap().clone();

        // Injected-failure countdown: decrement and return Unavailable while > 0.
        // For reconnect-exhausted tests (gateway retries watch_build up to
        // MAX_RECONNECT times before giving up).
        if outcome
            .watch_fail_count
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok()
        {
            return Err(Status::unavailable("mock: injected watch_build failure"));
        }

        // Scripted WatchBuild stream — same auto-fill pattern as SubmitBuild.
        // HONORS since_sequence: events with sequence ≤ `since` are skipped,
        // mirroring rio-scheduler's build_event_log replay. Auto-fill runs
        // FIRST (so `sequence: 0` in a scripted event becomes `(idx+1)`
        // before the filter checks it), then the filter compares the
        // FINAL sequence value against `since`.
        if let Some(events) = outcome.watch_scripted_events {
            let (tx, rx) = tokio::sync::mpsc::channel(32);
            let build_id = "test-build-00000000-1111-2222-3333-444444444444".to_string();
            tokio::spawn(async move {
                for (seq, mut ev) in events.into_iter().enumerate() {
                    if ev.build_id.is_empty() {
                        ev.build_id = build_id.clone();
                    }
                    if ev.sequence == 0 {
                        ev.sequence = (seq as u64) + 1;
                    }
                    // Real scheduler: strictly-greater-than filter.
                    if ev.sequence <= since {
                        continue;
                    }
                    if tx.send(Ok(ev)).await.is_err() {
                        return;
                    }
                }
            });
            return Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
                rx,
            )));
        }

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

    async fn resolve_tenant(
        &self,
        request: Request<rio_proto::scheduler::ResolveTenantRequest>,
    ) -> Result<Response<rio_proto::scheduler::ResolveTenantResponse>, Status> {
        let req = request.into_inner();
        self.resolve_tenant_calls
            .write()
            .unwrap()
            .push(req.tenant_name.clone());
        match *self.resolve_tenant_uuid.read().unwrap() {
            Some(id) => Ok(Response::new(rio_proto::scheduler::ResolveTenantResponse {
                tenant_id: id.to_string(),
            })),
            None => Err(Status::invalid_argument(format!(
                "unknown tenant: {}",
                req.tenant_name
            ))),
        }
    }
}

// ============================================================================
// MockAdmin
// ============================================================================

/// Minimal AdminService mock: returns empty-but-valid responses for all
/// unary RPCs. No configurable behavior — this is for CLI smoke tests
/// that just need "connects + non-error exit", not for asserting on
/// scheduler state (that's what the real AdminServiceImpl tests in
/// rio-scheduler/src/admin/tests.rs are for).
///
/// Streaming RPCs (GetBuildLogs, TriggerGC) return a stream with a
/// single terminal message so the client's drain loop exits cleanly.
#[derive(Clone, Default)]
pub struct MockAdmin {
    /// Every ClearPoison drv_hash received. For asserting the CLI
    /// passed through the positional arg correctly.
    pub clear_poison_calls: Arc<RwLock<Vec<String>>>,
}

impl MockAdmin {
    pub fn new() -> Self {
        Self::default()
    }
}

#[tonic::async_trait]
impl AdminService for MockAdmin {
    type GetBuildLogsStream =
        tokio_stream::wrappers::ReceiverStream<Result<types::BuildLogChunk, Status>>;
    type TriggerGCStream =
        tokio_stream::wrappers::ReceiverStream<Result<types::GcProgress, Status>>;

    async fn cluster_status(
        &self,
        _: Request<()>,
    ) -> Result<Response<types::ClusterStatusResponse>, Status> {
        Ok(Response::new(types::ClusterStatusResponse::default()))
    }

    async fn list_workers(
        &self,
        _: Request<types::ListWorkersRequest>,
    ) -> Result<Response<types::ListWorkersResponse>, Status> {
        Ok(Response::new(types::ListWorkersResponse::default()))
    }

    async fn list_builds(
        &self,
        _: Request<types::ListBuildsRequest>,
    ) -> Result<Response<types::ListBuildsResponse>, Status> {
        Ok(Response::new(types::ListBuildsResponse::default()))
    }

    async fn get_build_logs(
        &self,
        _: Request<types::GetBuildLogsRequest>,
    ) -> Result<Response<Self::GetBuildLogsStream>, Status> {
        // One chunk with one line, then EOF. Real server requires
        // derivation_path non-empty; the mock accepts anything (smoke
        // test doesn't validate server-side argument handling).
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let _ = tx
            .send(Ok(types::BuildLogChunk {
                derivation_path: "/nix/store/mock.drv".into(),
                lines: vec![b"mock log line".to_vec()],
                first_line_number: 0,
                is_complete: true,
            }))
            .await;
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    async fn trigger_gc(
        &self,
        _: Request<types::GcRequest>,
    ) -> Result<Response<Self::TriggerGCStream>, Status> {
        // Single is_complete=true frame so the CLI's drain loop sees
        // a clean terminal and doesn't emit the "closed without
        // is_complete" warning.
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let _ = tx
            .send(Ok(types::GcProgress {
                is_complete: true,
                ..Default::default()
            }))
            .await;
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    async fn drain_worker(
        &self,
        _: Request<types::DrainWorkerRequest>,
    ) -> Result<Response<types::DrainWorkerResponse>, Status> {
        Ok(Response::new(types::DrainWorkerResponse::default()))
    }

    async fn clear_poison(
        &self,
        request: Request<types::ClearPoisonRequest>,
    ) -> Result<Response<types::ClearPoisonResponse>, Status> {
        let req = request.into_inner();
        self.clear_poison_calls
            .write()
            .unwrap()
            .push(req.derivation_hash);
        Ok(Response::new(types::ClearPoisonResponse { cleared: false }))
    }

    async fn list_tenants(
        &self,
        _: Request<()>,
    ) -> Result<Response<types::ListTenantsResponse>, Status> {
        Ok(Response::new(types::ListTenantsResponse::default()))
    }

    async fn create_tenant(
        &self,
        request: Request<types::CreateTenantRequest>,
    ) -> Result<Response<types::CreateTenantResponse>, Status> {
        // Echo back a TenantInfo so the CLI's "returned no TenantInfo"
        // check passes.
        let req = request.into_inner();
        Ok(Response::new(types::CreateTenantResponse {
            tenant: Some(types::TenantInfo {
                tenant_id: "00000000-0000-0000-0000-000000000000".into(),
                tenant_name: req.tenant_name,
                ..Default::default()
            }),
        }))
    }

    async fn get_build_graph(
        &self,
        _: Request<types::GetBuildGraphRequest>,
    ) -> Result<Response<types::GetBuildGraphResponse>, Status> {
        Ok(Response::new(types::GetBuildGraphResponse::default()))
    }

    async fn get_size_class_status(
        &self,
        _: Request<types::GetSizeClassStatusRequest>,
    ) -> Result<Response<types::GetSizeClassStatusResponse>, Status> {
        Ok(Response::new(types::GetSizeClassStatusResponse::default()))
    }
}

/// Spawn a MockAdmin on an ephemeral port. Returns `(admin, addr, handle)`.
///
/// Plaintext — no TLS. For rio-cli smoke tests: run with no
/// `RIO_TLS__*` env vars and `load_client_tls` returns `None`.
pub async fn spawn_mock_admin()
-> anyhow::Result<(MockAdmin, SocketAddr, tokio::task::JoinHandle<()>)> {
    let admin = MockAdmin::new();
    let router = Server::builder().add_service(AdminServiceServer::new(admin.clone()));
    let (addr, handle) = spawn_grpc_server(router).await;
    Ok((admin, addr, handle))
}

// ============================================================================
// spawn helpers
// ============================================================================

/// Spawn an in-process tonic server on a random port. Returns `(addr, handle)`.
///
/// Uses `yield_now()` for synchronization — the listener is already bound
/// and accepting before `spawn` returns, so a single yield is sufficient to
/// let the server task enter its accept loop. No `sleep` needed.
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

/// Generic variant of [`spawn_grpc_server`] for routers with layers.
///
/// `Server::builder().layer(...)` changes `Router<Identity>` →
/// `Router<Stack<L, Identity>>`, which the non-generic
/// [`spawn_grpc_server`] can't accept. This variant passes the layer
/// parameter through to tonic's own generic `serve_with_incoming`.
///
/// ```ignore
/// let router = Server::builder()
///     .layer(InterceptorLayer::new(fake_interceptor))
///     .add_service(FooServer::new(foo));
/// let (addr, handle) = spawn_grpc_server_layered(router).await;
/// ```
///
/// The `where` clause mirrors tonic 0.14's `Router<L>::serve_with_incoming`
/// bounds — `L: Layer<Routes>` where `L::Service` is a tower `Service` over
/// `http::Request<tonic::body::Body>`. Callers don't spell the bounds;
/// inference carries them from the `.layer(...)` call site. If tonic's
/// bound changes in a minor bump, this compile-fails loudly rather than
/// silently diverging.
///
/// The non-generic [`spawn_grpc_server`] (≈ `L = Identity`) is kept as a
/// separate fn: Rust's default type parameters on functions are unstable,
/// and a `L = Identity` blanket would force existing callers to turbofish
/// or hit inference ambiguity.
///
/// `ResBody` is the layer's HTTP response body type — each layer can
/// transform the body (e.g., compression, tracing wrappers), so tonic's
/// generic `Router<L>` can't assume `tonic::body::Body`. Callers never
/// spell this: inference flows from `.layer(...)` through `L::Service`'s
/// `Response = http::Response<ResBody>` associated type. It's here only
/// to satisfy tonic's `serve_with_incoming` bound chain.
pub async fn spawn_grpc_server_layered<L, ResBody>(
    router: tonic::transport::server::Router<L>,
) -> (SocketAddr, tokio::task::JoinHandle<()>)
where
    L: tower::Layer<tonic::service::Routes> + Send + 'static,
    L::Service: tower::Service<http::Request<tonic::body::Body>, Response = http::Response<ResBody>>
        + Clone
        + Send
        + 'static,
    <L::Service as tower::Service<http::Request<tonic::body::Body>>>::Future: Send,
    <L::Service as tower::Service<http::Request<tonic::body::Body>>>::Error:
        Into<Box<dyn std::error::Error + Send + Sync>> + Send,
    ResBody: tonic::codegen::Body<Data = tonic::codegen::Bytes> + Send + 'static,
    ResBody::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
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

/// Spawn a MockStore + connect a StoreServiceClient in one call.
///
/// Extracts the spawn-then-connect combo that appears in worker upload
/// tests, executor input tests, gateway translate tests, and scheduler
/// coverage tests. Returns the JoinHandle so callers can abort/join.
pub async fn spawn_mock_store_with_client() -> anyhow::Result<(
    MockStore,
    rio_proto::StoreServiceClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
)> {
    let (store, addr, handle) = spawn_mock_store().await?;
    let client = rio_proto::client::connect_store(&addr.to_string()).await?;
    Ok((store, client, handle))
}

/// Spawn a MockStore over an in-process duplex transport (no real TCP).
///
/// Use this for `#[tokio::test(start_paused = true)]` tests. The regular
/// [`spawn_mock_store_with_client`] binds a real TCP socket, which makes
/// tokio's auto-advance fire `.timeout()` wrappers while kernel-side
/// accept/handshake are pending — spurious `DeadlineExceeded` on a
/// loaded CI runner (§2.7 `test-start-paused-real-tcp-spawn-blocking`).
///
/// The duplex halves are tokio tasks, so auto-advance sees them as
/// "not idle" while they're doing I/O. Same pattern as
/// `rio-worker/src/executor/daemon/stderr_loop.rs:559`.
///
/// No `JoinHandle` returned: the server task is fire-and-forget (dies
/// with the test). No port to clean up.
pub async fn spawn_mock_store_inproc() -> anyhow::Result<(
    MockStore,
    rio_proto::StoreServiceClient<tonic::transport::Channel>,
)> {
    use hyper_util::rt::TokioIo;
    use tonic::transport::Endpoint;

    let store = MockStore::new();
    let svc = StoreServiceServer::new(store.clone());

    // Channel of server-side duplex halves. Each client "connect" mints
    // a duplex pair, hands one half to the server via this channel.
    // Unbounded: connect is bounded by the test itself (1-3 connections
    // per test), no backpressure needed.
    //
    // Server side receives raw `DuplexStream` (tonic has a blanket
    // `Connected for DuplexStream` impl); only the client side needs
    // `TokioIo` wrapping (tonic's client transport expects hyper's
    // `Read`/`Write`, not tokio's `AsyncRead`/`AsyncWrite`).
    let (conn_tx, conn_rx) = tokio::sync::mpsc::unbounded_channel::<tokio::io::DuplexStream>();
    let incoming =
        tokio_stream::wrappers::UnboundedReceiverStream::new(conn_rx).map(Ok::<_, std::io::Error>);

    tokio::spawn(async move {
        Server::builder()
            .add_service(svc)
            .serve_with_incoming(incoming)
            .await
            .expect("in-process gRPC server");
    });
    tokio::task::yield_now().await;

    // Connector: on each poll, create a fresh duplex pair, ship the
    // server half, wrap the client half. URI is a dummy — tonic parses
    // it but the connector never uses it.
    //
    // 64 KiB duplex buffer: tonic's default HTTP/2 window is 64 KiB;
    // smaller than a NAR chunk (256 KiB) so the writer blocks briefly,
    // but that's fine under paused time (the blocked write is a tokio
    // task, not real I/O).
    let channel = Endpoint::try_from("http://inproc.mock")?
        .connect_with_connector(tower::service_fn(move |_: tonic::transport::Uri| {
            let conn_tx = conn_tx.clone();
            async move {
                let (client_io, server_io) = tokio::io::duplex(64 * 1024);
                conn_tx
                    .send(server_io)
                    .map_err(|_| std::io::Error::other("inproc server dropped"))?;
                Ok::<_, std::io::Error>(TokioIo::new(client_io))
            }
        }))
        .await?;

    Ok((store, rio_proto::StoreServiceClient::new(channel)))
}

/// Spawn a MockScheduler on an ephemeral port. Returns `(scheduler, addr, handle)`.
pub async fn spawn_mock_scheduler()
-> anyhow::Result<(MockScheduler, SocketAddr, tokio::task::JoinHandle<()>)> {
    let sched = MockScheduler::new();
    let router = Server::builder().add_service(SchedulerServiceServer::new(sched.clone()));
    let (addr, handle) = spawn_grpc_server(router).await;
    Ok((sched, addr, handle))
}
