//! In-memory [`StoreService`] + [`ChunkService`] mock.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tonic::{Request, Response, Status, Streaming};

use rio_proto::types;
use rio_proto::validated::ValidatedPathInfo;
use rio_proto::{ChunkService, StoreService};

/// `(PathInfo, NAR bytes)` — stored value type for [`MockStoreState::paths`].
type StoredPath = (types::PathInfo, Vec<u8>);

/// `(drv_hash, output_name)` — key type for [`MockStoreState::realisations`].
/// Alias silences clippy::type_complexity on the nested generic field.
type RealisationKey = (Vec<u8>, String);

/// `(used_bytes, limit_bytes)` — value type for
/// [`MockStoreState::tenant_quotas`]. Alias silences clippy::type_complexity
/// on the nested `Arc<RwLock<HashMap<_, _>>>`.
type TenantQuotaEntry = (u64, Option<u64>);

/// In-memory data the mock serves. Tests seed these directly (or via
/// [`MockStore::seed`] / [`MockStore::seed_chunked`]) and the
/// [`StoreService`] / [`ChunkService`] impls read from them.
#[derive(Clone, Default)]
pub struct MockStoreState {
    /// store_path -> (PathInfo, NAR bytes)
    pub paths: Arc<RwLock<HashMap<String, StoredPath>>>,
    /// CA realisations: (drv_hash, output_name) -> Realisation.
    /// Used by gateway wopRegisterDrvOutput/wopQueryRealisation tests.
    pub realisations: Arc<RwLock<HashMap<RealisationKey, types::Realisation>>>,
    /// Per-tenant quota: tenant_name -> (used_bytes, limit_bytes).
    /// Tests seed this directly; TenantQuota reads it verbatim.
    /// Absent key → NOT_FOUND (gateway treats as no-quota).
    pub tenant_quotas: Arc<RwLock<HashMap<String, TenantQuotaEntry>>>,
    /// Paths that `find_missing_paths` reports as substitutable.
    /// Tests seed directly; only paths that are ALSO in the missing
    /// set (not in `self.paths`) land in `substitutable_paths` — a
    /// present path is never substitutable.
    pub substitutable: Arc<RwLock<Vec<String>>>,
    /// BLAKE3 digest → chunk bytes. dataplane2: backs the in-memory
    /// `ChunkService.GetChunk` impl. Seed via [`MockStore::seed_chunked`].
    pub chunks: Arc<RwLock<HashMap<Vec<u8>, Vec<u8>>>>,
}

/// Call recorders. The [`StoreService`] / [`ChunkService`] impls write
/// to these on every RPC; tests read them to assert on call counts,
/// arguments, and ordering.
#[derive(Clone, Default)]
pub struct MockStoreCalls {
    /// Every PutPath metadata received (for assertions on upload count/contents).
    pub put_calls: Arc<RwLock<Vec<types::PathInfo>>>,
    /// Records every `query_path_info` call's requested path. For
    /// verifying r[sched.merge.substitute-fetch]'s eager-fetch loop.
    pub qpi_calls: Arc<RwLock<Vec<String>>>,
    /// Number of `batch_query_path_info` calls received. For I-110
    /// tests proving the builder uses one batch RPC per BFS layer
    /// (not N per-path RPCs).
    pub batch_qpi_calls: Arc<AtomicU32>,
    /// Number of `batch_get_manifest` calls received. For I-110c
    /// tests proving the builder calls it once before the warm loop.
    pub batch_manifest_calls: Arc<AtomicU32>,
    /// Number of `find_missing_paths` calls received (incremented on
    /// entry, before the fail-injection check). For I-163 tests
    /// proving deferred FODs use the batch pre-pass (1 RPC) and skip
    /// the per-FOD `fod_outputs_in_store` fallback (would be N+1).
    pub find_missing_calls: Arc<AtomicU32>,
    /// `manifest_hint` from each `get_path` call (None if unset).
    /// I-110c: lets tests assert the FUSE fetch carried the primed
    /// hint.
    pub get_path_hints: Arc<RwLock<Vec<Option<types::ManifestHint>>>>,
    /// Number of `GetChunk` calls received. For dataplane2 tests
    /// proving the builder uses the parallel chunk path (≥1) and
    /// fan-out hits this for every chunk in the manifest.
    pub get_chunk_calls: Arc<AtomicU32>,
}

/// Fault injection knobs. All default to "no fault"; tests flip them
/// to exercise error paths.
#[derive(Clone, Default)]
pub struct MockStoreFaults {
    /// If > 0, put_path decrements and returns Unavailable. For retry tests.
    pub fail_next_puts: Arc<AtomicU32>,
    /// If > 0, put_path decrements and returns `Aborted("concurrent
    /// PutPath in progress for this path; retry")` — matching the real
    /// store's placeholder-contention response (`put_path.rs`). For
    /// gateway I-068 retry tests.
    pub abort_next_puts: Arc<AtomicU32>,
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
    /// Per-NarChunk delay (millis) injected in `GetPath`'s stream. 0 = no
    /// delay. For I-211 progress-based timeout tests: with a multi-chunk
    /// NAR, `delay × chunk_count > idle_timeout` proves the fetch
    /// completes; `delay > idle_timeout` proves the per-chunk timeout
    /// trips on the first stalled chunk.
    pub get_path_chunk_delay_ms: Arc<AtomicU64>,
    /// If true, `GetChunk` returns Unimplemented (simulates an old
    /// store binary). For dataplane2 fallback tests — builder MUST
    /// fall back to `GetPath`.
    pub get_chunk_unimplemented: Arc<AtomicBool>,
}

/// In-memory store: `store_path -> (PathInfo, nar_bytes)`.
///
/// Records PutPath calls and supports prefix-match QueryPathInfo (for
/// hash-part lookups via QueryPathFromHashPart).
///
/// Fields are grouped by purpose:
/// - [`state`](Self::state): in-memory data the mock serves
/// - [`calls`](Self::calls): call recorders for test assertions
/// - [`faults`](Self::faults): fault injection knobs
#[derive(Clone, Default)]
pub struct MockStore {
    pub state: MockStoreState,
    pub calls: MockStoreCalls,
    pub faults: MockStoreFaults,
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
        self.state
            .paths
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

    /// Seed a path AND its chunked manifest. Splits `nar` into fixed
    /// `chunk_size` pieces (the real store uses FastCDC; fixed-size is
    /// fine for the mock — chunks are addressed by content hash, not
    /// boundary), populates `self.state.chunks`, and stores the chunk list
    /// alongside the inline blob so `batch_get_manifest` can return it.
    ///
    /// Returns the `Vec<ChunkRef>` so tests can prime the builder's
    /// hint cache directly.
    pub fn seed_chunked(
        &self,
        info: ValidatedPathInfo,
        nar: Vec<u8>,
        chunk_size: usize,
    ) -> Vec<types::ChunkRef> {
        let mut refs = Vec::new();
        let mut chunks = self.state.chunks.write().unwrap();
        for piece in nar.chunks(chunk_size) {
            let h = blake3::hash(piece);
            let digest = h.as_bytes().to_vec();
            chunks.insert(digest.clone(), piece.to_vec());
            refs.push(types::ChunkRef {
                hash: digest,
                size: piece.len() as u32,
            });
        }
        drop(chunks);
        self.seed(info, nar);
        refs
    }
}

#[tonic::async_trait]
impl ChunkService for MockStore {
    type GetChunkStream =
        tokio_stream::wrappers::ReceiverStream<Result<types::GetChunkResponse, Status>>;

    async fn get_chunk(
        &self,
        request: Request<types::GetChunkRequest>,
    ) -> Result<Response<Self::GetChunkStream>, Status> {
        self.calls.get_chunk_calls.fetch_add(1, Ordering::SeqCst);
        if self.faults.get_chunk_unimplemented.load(Ordering::SeqCst) {
            return Err(Status::unimplemented("mock: GetChunk disabled"));
        }
        let digest = request.into_inner().digest;
        let data = self
            .state
            .chunks
            .read()
            .unwrap()
            .get(&digest)
            .cloned()
            .ok_or_else(|| Status::not_found("mock: chunk not found"))?;
        // Single-message "stream" — matches the real store (chunk.rs:315).
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        let _ = tx.send(Ok(types::GetChunkResponse { data })).await;
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
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
            .faults
            .fail_next_puts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok()
        {
            return Err(Status::unavailable("mock: injected put failure"));
        }
        if self
            .faults
            .abort_next_puts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok()
        {
            return Err(Status::aborted(
                "concurrent PutPath in progress for this path; retry",
            ));
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
        self.calls.put_calls.write().unwrap().push(info.clone());
        let store_path = info.store_path.clone();
        self.state
            .paths
            .write()
            .unwrap()
            .insert(store_path, (info, nar));
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
            .faults
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
            self.calls.put_calls.write().unwrap().push(info.clone());
            let store_path = info.store_path.clone();
            self.state
                .paths
                .write()
                .unwrap()
                .insert(store_path, (info, nar));
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
        if self.faults.fail_get_path.load(Ordering::SeqCst) {
            return Err(Status::unavailable("mock: injected get_path failure"));
        }
        // Slow-fetch gate: park until the test releases us. The fetcher thread
        // blocks inside block_on here, which is exactly the condition the
        // FUSE concurrent-waiter test needs — waiters park on the condvar
        // past at least one WAIT_SLICE before the gate opens.
        if self.faults.get_path_gate_armed.load(Ordering::SeqCst) {
            self.faults.get_path_gate.notified().await;
        }
        let req = request.into_inner();
        // I-110c: record the hint (or its absence) so tests can assert
        // the FUSE fetch carried what `prefetch_manifests` primed.
        self.calls
            .get_path_hints
            .write()
            .unwrap()
            .push(req.manifest_hint);
        let store_path = req.store_path;
        // Garbage mode: return a stream with valid PathInfo but garbage NAR
        // bytes, so collect_nar_stream succeeds but nar::parse fails.
        if self.faults.get_path_garbage.load(Ordering::SeqCst) {
            let entry = self.state.paths.read().unwrap().get(&store_path).cloned();
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
        let entry = self.state.paths.read().unwrap().get(&store_path).cloned();
        let chunk_delay = self.faults.get_path_chunk_delay_ms.load(Ordering::SeqCst);
        match entry {
            Some((info, nar)) => {
                // Channel depth 1 (not 4): with `chunk_delay > 0` the
                // sender must not race ahead of the receiver, or the
                // delay is masked by buffered chunks. Depth-1 backpressure
                // makes each delay observable as a true inter-recv gap.
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                tokio::spawn(async move {
                    let _ = tx
                        .send(Ok(types::GetPathResponse {
                            msg: Some(types::get_path_response::Msg::Info(info)),
                        }))
                        .await;
                    // Send NAR in 64 KiB chunks (matches real store)
                    for chunk in nar.chunks(64 * 1024) {
                        if chunk_delay > 0 {
                            tokio::time::sleep(Duration::from_millis(chunk_delay)).await;
                        }
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
        if self.faults.fail_query_path_info.load(Ordering::SeqCst) {
            return Err(Status::unavailable(
                "mock: injected query_path_info failure",
            ));
        }
        let store_path = request.into_inner().store_path;
        self.calls
            .qpi_calls
            .write()
            .unwrap()
            .push(store_path.clone());
        // Substitution side-effect simulation: if the path is seeded
        // as substitutable, materialize a minimal PathInfo on QPI —
        // mirrors the real store's try_substitute_on_miss. The
        // scheduler's r[sched.merge.substitute-fetch] eager-fetch
        // depends on this returning Some rather than NotFound.
        if self
            .state
            .substitutable
            .read()
            .unwrap()
            .contains(&store_path)
        {
            return Ok(Response::new(types::PathInfo {
                store_path: store_path.clone(),
                nar_hash: vec![0u8; 32],
                nar_size: 0,
                ..Default::default()
            }));
        }
        let paths = self.state.paths.read().unwrap();
        // Exact match only. Hash-part prefix lookups go through
        // query_path_from_hash_part (below), not here — the gateway
        // uses that dedicated RPC.
        paths
            .get(&store_path)
            .map(|(info, _)| Response::new(info.clone()))
            .ok_or_else(|| Status::not_found(format!("not found: {store_path}")))
    }

    async fn batch_query_path_info(
        &self,
        request: Request<types::BatchQueryPathInfoRequest>,
    ) -> Result<Response<types::BatchQueryPathInfoResponse>, Status> {
        // Reuse fail_query_path_info: a store that's Unavailable for
        // single-path QPI is Unavailable for batch QPI too. Keeps the
        // existing error-propagation tests valid for the batch path.
        if self.faults.fail_query_path_info.load(Ordering::SeqCst) {
            return Err(Status::unavailable(
                "mock: injected query_path_info failure",
            ));
        }
        self.calls.batch_qpi_calls.fetch_add(1, Ordering::SeqCst);
        let paths = self.state.paths.read().unwrap();
        let entries = request
            .into_inner()
            .store_paths
            .into_iter()
            .map(|store_path| {
                let info = paths.get(&store_path).map(|(info, _)| info.clone());
                types::PathInfoEntry { store_path, info }
            })
            .collect();
        Ok(Response::new(types::BatchQueryPathInfoResponse { entries }))
    }

    async fn batch_get_manifest(
        &self,
        request: Request<types::BatchGetManifestRequest>,
    ) -> Result<Response<types::BatchGetManifestResponse>, Status> {
        self.calls
            .batch_manifest_calls
            .fetch_add(1, Ordering::SeqCst);
        let paths = self.state.paths.read().unwrap();
        let entries = request
            .into_inner()
            .store_paths
            .into_iter()
            .map(|store_path| {
                // MockStore stores whole NARs in-memory — represent
                // as inline (no chunking in the mock).
                let hint = paths
                    .get(&store_path)
                    .map(|(info, nar)| types::ManifestHint {
                        info: Some(info.clone()),
                        chunks: Vec::new(),
                        inline_blob: nar.clone(),
                    });
                types::ManifestEntry { store_path, hint }
            })
            .collect();
        Ok(Response::new(types::BatchGetManifestResponse { entries }))
    }

    async fn find_missing_paths(
        &self,
        request: Request<types::FindMissingPathsRequest>,
    ) -> Result<Response<types::FindMissingPathsResponse>, Status> {
        self.calls.find_missing_calls.fetch_add(1, Ordering::SeqCst);
        if self.faults.fail_find_missing.load(Ordering::SeqCst) {
            return Err(Status::unavailable("mock: injected find_missing failure"));
        }
        let requested = request.into_inner().store_paths;
        let paths = self.state.paths.read().unwrap();
        let missing: Vec<String> = requested
            .into_iter()
            .filter(|p| !paths.contains_key(p))
            .collect();
        // Substitutable ⊆ missing: only report paths that were
        // requested-and-missing AND seeded as substitutable. A seeded
        // path not in this request's missing set stays out (the real
        // store only checks upstream for paths it doesn't have).
        let subs = self.state.substitutable.read().unwrap();
        let substitutable: Vec<String> = missing
            .iter()
            .filter(|p| subs.contains(p))
            .cloned()
            .collect();
        Ok(Response::new(types::FindMissingPathsResponse {
            missing_paths: missing,
            substitutable_paths: substitutable,
        }))
    }

    async fn query_path_from_hash_part(
        &self,
        request: Request<types::QueryPathFromHashPartRequest>,
    ) -> Result<Response<types::PathInfo>, Status> {
        let hash_part = request.into_inner().hash_part;
        // Prefix-match: find a stored path starting with /nix/store/{hash}-.
        let prefix = format!("/nix/store/{hash_part}-");
        let paths = self.state.paths.read().unwrap();
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
        let mut paths = self.state.paths.write().unwrap();
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
        self.state.realisations.write().unwrap().insert(key, r);
        Ok(Response::new(types::RegisterRealisationResponse {}))
    }

    async fn query_realisation(
        &self,
        request: Request<types::QueryRealisationRequest>,
    ) -> Result<Response<types::Realisation>, Status> {
        let req = request.into_inner();
        let key = (req.drv_hash, req.output_name.clone());
        match self.state.realisations.read().unwrap().get(&key) {
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
        match self.state.tenant_quotas.read().unwrap().get(name) {
            Some(&(used, limit)) => Ok(Response::new(types::TenantQuotaResponse {
                used_bytes: used,
                limit_bytes: limit,
            })),
            None => Err(Status::not_found(format!("mock: unknown tenant: {name}"))),
        }
    }
}
