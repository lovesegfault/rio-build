//! In-memory [`StoreService`] + [`ChunkService`] mock.

use std::collections::{HashMap, HashSet};
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
    /// Paths that `find_missing_paths` reports as indeterminate
    /// (probe got 429/5xx/deadline). Tests assert the scheduler
    /// optimistically tries the substitute fetch instead of immediate
    /// build dispatch. Same `⊆ missing` filter as `substitutable`;
    /// takes precedence — a path in BOTH is reported indeterminate-
    /// only by `find_missing_paths`, while `substitute_path` still
    /// succeeds (mirrors "HEAD 429'd but GET works").
    /// `r[sched.merge.substitute-probe-indeterminate+2]`
    pub indeterminate: Arc<RwLock<Vec<String>>>,
    /// merged_bug_028: per-tenant FMP scripting. Key = the
    /// `x-rio-probe-tenant-id` header value; the listed paths are
    /// reported missing-and-unsubstitutable FOR THAT TENANT (forced
    /// into `missing_paths`, excluded from substitutable/
    /// indeterminate) regardless of `paths`/`substitutable` — the
    /// mock twin of the store's per-tenant sig-visibility gate and
    /// per-tenant upstream view. Requests with no/other tenant header
    /// answer from the global state as before.
    pub per_tenant_unobtainable: Arc<RwLock<HashMap<String, Vec<String>>>>,
    /// Per-path `SubstitutePath` shape: `(nar_size, progress_ticks)`.
    /// BLAKE3 digest → chunk bytes. dataplane2: backs the in-memory
    /// `ChunkService.GetChunk` impl. Seed via [`MockStore::seed_chunked`].
    pub chunks: Arc<RwLock<HashMap<Vec<u8>, Vec<u8>>>>,
    /// nar_hash → NAR index. Backs `GetNarIndex`/`GetNarIndexBatch` —
    /// the builder's castore root-node fallback for closure paths the
    /// scheduler dispatched without a `root_node`. Seed via
    /// [`MockStore::seed_nar_index`]; an unseeded hash is reported as
    /// "no index" (NotFound / `index: None`), which is exactly the
    /// indexer-lag case the fallback's error path guards.
    pub nar_indexes: Arc<RwLock<HashMap<Vec<u8>, types::NarIndex>>>,
    /// `TailLog` script: derivation (verbatim request key — the mock
    /// does no hash normalization) → the chunks one subscription
    /// receives. The stream serves the scripted chunks then, mirroring
    /// the real server, either ends (`follow: false` — the CLI's /
    /// dashboard's one-shot drain) or stays open (`follow: true` — a
    /// live subscription); an unscripted `follow: true` derivation gets
    /// an empty held-open stream so background subscriptions for
    /// derivations a test doesn't care about neither error nor spin in
    /// a reconnect loop. Seed via [`MockStore::seed_tail_log`].
    pub tail_logs: Arc<RwLock<HashMap<String, Vec<rio_proto::store::TailLogChunk>>>>,
}

/// One recorded `PutPathChunked` call: the assignment-token header (if
/// any), the `Begin` frame as received, and the chunk digests received
/// in wire order. Tests assert on the Begin's outputs/novel/closure and
/// on which chunk bodies were actually streamed.
#[derive(Clone)]
pub struct RecordedChunkedPut {
    pub token: Option<String>,
    pub begin: types::PutPathChunkedBegin,
    pub chunk_digests: Vec<Vec<u8>>,
}

/// Call recorders. The [`StoreService`] / [`ChunkService`] impls write
/// to these on every RPC; tests read them to assert on call counts,
/// arguments, and ordering.
#[derive(Clone, Default)]
pub struct MockStoreCalls {
    /// Every PutPath metadata received (for assertions on upload count/contents).
    pub put_calls: Arc<RwLock<Vec<types::PathInfo>>>,
    /// Every PutPathChunked stream received (P0586 builder upload path).
    pub put_chunked_calls: Arc<RwLock<Vec<RecordedChunkedPut>>>,
    /// Records every `query_path_info` call's requested path. (The
    /// walk-era eager-fetch loop this originally verified died with
    /// Phase D-prime; gateway and builder QPI assertions still read it.)
    pub qpi_calls: Arc<RwLock<Vec<String>>>,
    /// Per-path `query_path_info` attempt count, INCLUDING calls that
    /// fault-short-circuit (the early-return knobs above all skip
    /// `qpi_calls`). For structural retry assertions:
    /// `attempts[p] == N+1` proves N retries + 1 success without a
    /// process-global metrics recorder (the walk-era fanout-bound rule
    /// these served retired with Phase D-prime).
    pub qpi_attempts_by_path: Arc<RwLock<HashMap<String, u32>>>,
    /// Number of `batch_query_path_info` calls received. For I-110
    /// tests proving the builder uses one batch RPC per BFS layer
    /// (not N per-path RPCs).
    pub batch_qpi_calls: Arc<AtomicU32>,
    /// Number of `find_missing_paths` calls received (incremented on
    /// entry, before the fail-injection check). For I-163 tests
    /// proving deferred FODs use the batch pre-pass (1 RPC) and skip
    /// the per-FOD `fod_outputs_in_store` fallback (would be N+1).
    pub find_missing_calls: Arc<AtomicU32>,
    /// The `x-rio-probe-tenant-id` header of every FMP call, in
    /// arrival order (`None` = absent). merged_bug_028: asserts the
    /// dispatch/settlement probes ask once PER LIVE TENANT instead of
    /// one arbitrarily-picked tenant.
    pub find_missing_tenants: Arc<RwLock<Vec<Option<String>>>>,
    /// The `store_paths` of every ANSWERED `find_missing_paths` call,
    /// in arrival order. Fault-injected calls (`fail_find_missing`)
    /// increment `find_missing_calls` but are NOT logged here — the
    /// log records what the store actually adjudicated. For structural
    /// per-cycle admission assertions: the scheduler's dispatch-probe
    /// quota tests count ADMITTED candidates per tick from the request
    /// paths themselves instead of trusting scheduler-side bookkeeping.
    pub find_missing_paths_log: Arc<RwLock<Vec<Vec<String>>>>,
    /// Number of `get_path` calls received. Incremented on entry,
    /// BEFORE the `fail_get_path` early-return — distinguishes "client
    /// never reached the RPC" from "RPC returned Unavailable". For
    /// structural no-gRPC-contact assertions in `rio-builder` FUSE
    /// fetch tests (replaces wall-clock `elapsed < backoff_floor`
    /// asserts that flaked under full-gate parallel load).
    pub get_path_calls: Arc<AtomicU32>,
    /// `x-rio-assignment-token` value on each GetPath call (`None` =
    /// absent). For the builder's `.drv` fetch: the store's drv-blob
    /// GetPath fallback is tenant-scoped, so `fetch_drv_from_store`
    /// must present the assignment token.
    pub get_path_tokens: Arc<RwLock<Vec<Option<String>>>>,
    /// `x-rio-tenant-token` value on each QueryRealisation call
    /// (`None` = absent). For `r[gw.jwt.propagate]` — floating-CA
    /// output resolution in `wopBuildPathsWithResults`.
    pub query_realisation_metadata: Arc<RwLock<Vec<Option<String>>>>,
    /// Every `TailLog` request received, in arrival order. For
    /// asserting the gateway's per-derivation subscription lifecycle
    /// (one open per Started, `since_line` resumption, exec_id keying).
    pub tail_calls: Arc<RwLock<Vec<rio_proto::store::TailLogRequest>>>,
}

/// Fault injection knobs. All default to "no fault"; tests flip them
/// to exercise error paths.
// r[impl ts.mock.store-faults]
#[derive(Clone, Default)]
pub struct MockStoreFaults {
    /// If > 0, put_path decrements and returns Unavailable. For retry tests.
    pub fail_next_puts: Arc<AtomicU32>,
    /// If > 0, put_path decrements and returns the store's typed
    /// NAR-budget shed (`ResourceExhausted` + "retry" text — the
    /// `rio_common::grpc::STORE_SHED_CLASSES` contract face, the
    /// merged_bug_097 regression class). For gateway shed-absorption
    /// tests.
    pub shed_next_puts: Arc<AtomicU32>,
    /// If > 0, put_path_chunked decrements and returns
    /// `FailedPrecondition` — a deterministic rejection, matching the
    /// real store's "PutPathChunked requires a chunk backend" gate on
    /// an inline-only deployment. The remaining count after the call
    /// is the structural attempt counter for asserting the builder
    /// does NOT burn its retry budget on a non-retryable status.
    pub reject_next_chunked_puts: Arc<AtomicU32>,
    /// If > 0, put_path decrements and returns `Aborted("concurrent
    /// PutPath in progress for this path; retry")` — matching the real
    /// store's placeholder-contention response (`put_path.rs`). For
    /// gateway I-068 retry tests.
    pub abort_next_puts: Arc<AtomicU32>,
    /// If true, find_missing_paths returns Unavailable. For scheduler
    /// cache-check error-path tests.
    pub fail_find_missing: Arc<AtomicBool>,
    /// merged_bug_003: if true, find_missing_paths behaves like the
    /// pre-Q3 store under service-token verification failure — it
    /// IGNORES the probe-tenant header (no upstream probe runs:
    /// substitutable/indeterminate come back empty, wire-identical to
    /// confirmed 404s) and echoes `probe_ran_tenant_scoped = false`.
    /// For scheduler tests proving the echo (not sender intent) gates
    /// confirmed-missing.
    pub drop_tenant_scope: Arc<AtomicBool>,
    /// If true, query_path_info returns Unavailable. For worker input-fetch
    /// error-path tests (distinguishing real gRPC errors from NotFound).
    pub fail_query_path_info: Arc<AtomicBool>,
    /// If true, query_path_info returns Internal (NOT transient per
    /// `rio_common::grpc::is_transient`). For substitute-fetch tests
    /// that want immediate failure without triggering the retry loop.
    pub fail_query_path_info_permanent: Arc<AtomicBool>,
    /// While `> 0`, query_path_info returns Unavailable and decrements.
    /// At 0, falls through to normal behavior. For retry-then-succeed
    /// tests (transient overload absorbed by backoff).
    pub fail_query_path_info_n_times: Arc<std::sync::atomic::AtomicU32>,
    /// While `std::time::Instant::now() < deadline`, query_path_info
    /// returns `ResourceExhausted`. At/after the deadline, falls
    /// through to normal behavior. Time-based (not count-based) so
    /// tests can model "store is overloaded for N seconds" against the
    /// caller's wall-clock retry budget — the count-based knob above
    /// can't express that with concurrent callers (each decrements).
    /// `std::time::Instant` (not tokio's) because the scheduler's
    /// substitute-fetch tests run on real time (ephemeral PG + paused
    /// time don't compose; see scheduler `merge.rs::transient_retry`).
    pub fail_qpi_resource_exhausted_until: Arc<RwLock<Option<std::time::Instant>>>,
    /// While > 0, query_path_info returns `ResourceExhausted` for the
    /// first N attempts PER PATH (tracked via
    /// [`MockStoreCalls::qpi_attempts_by_path`]); attempt N+1 falls
    /// through. Count-based per-path (vs the time-gated knob above) so
    /// retry regression tests assert structurally — every path
    /// retried exactly N times — without a wall-clock window.
    pub fail_qpi_resource_exhausted_per_path_n: Arc<std::sync::atomic::AtomicU32>,
    /// While > 0, query_path_info / substitute_path return `NotFound`
    /// for the first N attempts PER PATH (tracked via
    /// [`MockStoreCalls::qpi_attempts_by_path`]); attempt N+1 falls
    /// through to normal behavior. Models the incident shape where the
    /// store short-circuits a substitution with NotFound without ever
    /// reaching the upstream and a later attempt succeeds. For
    /// retry-contradicted-NotFound tests (same per-path count shape as
    /// `fail_qpi_resource_exhausted_per_path_n`).
    pub fail_qpi_not_found_per_path_n: Arc<std::sync::atomic::AtomicU32>,
    /// Paths for which query_path_info / substitute_path always return
    /// `Internal` (non-transient, non-NotFound → no retry ladder).
    /// Per-path counterpart of `fail_query_path_info_permanent` for
    /// tests that need ONE path to fail hard while its siblings
    /// substitute normally — e.g. the end-to-end demotion tests, which
    /// would otherwise burn the scheduler's full ~32 s NotFound/
    /// transient backoff ladder in real time (the actor-driven tests
    /// can't virtualize the clock; ephemeral PG and paused time don't
    /// compose).
    pub fail_qpi_internal_paths: Arc<RwLock<HashSet<String>>>,
    /// If true, get_path returns Unavailable. For FUSE fetch error-path tests.
    pub fail_get_path: Arc<AtomicBool>,
    /// If `Some`, `get_path` returns this `tonic::Code`. Supersedes
    /// [`fail_get_path`](Self::fail_get_path) (the legacy
    /// `Unavailable`-only knob). For the scheduler's
    /// latch-on-definitive-error tests — e.g. `InvalidArgument` is the
    /// store's "store-path didn't parse" verdict and must classify as
    /// ENOENT-equivalent (negative-cache), NOT retry-worthy.
    pub get_path_status: Arc<std::sync::RwLock<Option<tonic::Code>>>,
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
    /// If `query_path_info_gate_armed` is true, `QueryPathInfo` awaits
    /// this Notify BEFORE the existing fault checks. Mirrors
    /// `get_path_gate`: tests arm it, drive the actor until the
    /// detached substitute-fetch task is parked here, then
    /// `.notify_waiters()` to release. Distinct from the `fail_*`
    /// knobs (which return immediately) — this holds-then-proceeds.
    pub query_path_info_gate: Arc<tokio::sync::Notify>,
    /// Whether `query_path_info_gate` is armed. When false,
    /// `QueryPathInfo` ignores the gate (backwards-compatible).
    pub query_path_info_gate_armed: Arc<AtomicBool>,
    /// Per-NarChunk delay (millis) injected in `GetPath`'s stream. 0 = no
    /// delay. For I-211 progress-based timeout tests: with a multi-chunk
    /// NAR, `delay × chunk_count > idle_timeout` proves the fetch
    /// completes; `delay > idle_timeout` proves the per-chunk timeout
    /// trips on the first stalled chunk.
    pub get_path_chunk_delay_ms: Arc<AtomicU64>,
    /// If true, `query_realisation` returns Unavailable. For gateway
    /// CA-resolution error-path tests (opcodes 36/40/41/43/46) — proving
    /// non-NotFound store errors surface as STDERR_ERROR rather than
    /// being swallowed as "no realisation".
    pub fail_query_realisation: Arc<AtomicBool>,
    /// Paths for which `put_path` returns `Ok(created:false)`
    /// IMMEDIATELY after reading Metadata, WITHOUT draining NarChunks.
    /// Mimics the real store's `AlreadyComplete` / Concurrent-race
    /// branches (`put_path/mod.rs`) when `drain_stream`'s 30s timeout
    /// expires. For gateway `grpc_put_path_streaming` wire-positioning
    /// tests: a `>16 MiB` entry that early-Ok's must leave the framed
    /// reader at exactly `nar_size` so the next entry's header parses.
    pub put_path_early_ok_paths: Arc<RwLock<HashSet<String>>>,
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
    /// boundary), populates `self.state.chunks`, and seeds the path's
    /// PathInfo + NAR via [`Self::seed`].
    ///
    /// Returns the `Vec<ChunkRef>` (per-chunk digest + size) so tests
    /// can assert against `ChunkService.GetChunk`.
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

    /// Seed a NAR index for `nar_hash`, served by `GetNarIndex` /
    /// `GetNarIndexBatch`.
    pub fn seed_nar_index(&self, nar_hash: [u8; 32], index: types::NarIndex) {
        self.state
            .nar_indexes
            .write()
            .unwrap()
            .insert(nar_hash.to_vec(), index);
    }
}

// r[impl ts.mock.store-chunk]
#[tonic::async_trait]
impl ChunkService for MockStore {
    type GetChunkStream =
        tokio_stream::wrappers::ReceiverStream<Result<types::GetChunkResponse, Status>>;

    async fn get_chunk(
        &self,
        request: Request<types::GetChunkRequest>,
    ) -> Result<Response<Self::GetChunkStream>, Status> {
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

    type GetChunksStream = tokio_stream::wrappers::ReceiverStream<Result<types::ChunkData, Status>>;

    /// Bidi-stream batch fetch (P0568). Same lookup as `get_chunk` —
    /// reads the seeded `state.chunks` map, no fan-out (the real
    /// server's `K_server` parallelism is a perf concern, not a
    /// behavior the consumer can observe; `ChunkData` carries the
    /// digest precisely so completion order doesn't matter). Mirrors
    /// the real abort-on-first-miss contract so retry tests against
    /// the mock exercise the same code path as production.
    async fn get_chunks(
        &self,
        request: Request<Streaming<types::GetChunksRequest>>,
    ) -> Result<Response<Self::GetChunksStream>, Status> {
        let mut requests = request.into_inner();
        let chunks = std::sync::Arc::clone(&self.state.chunks);
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tokio::spawn(async move {
            'frames: while let Ok(Some(frame)) = requests.message().await {
                for digest in frame.digests {
                    let item = chunks
                        .read()
                        .unwrap()
                        .get(&digest)
                        .cloned()
                        .map(|data| types::ChunkData {
                            digest,
                            data: data.into(),
                        })
                        .ok_or_else(|| Status::not_found("mock: chunk not found"));
                    let is_err = item.is_err();
                    if tx.send(item).await.is_err() || is_err {
                        break 'frames;
                    }
                }
            }
        });
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    /// Durable-presence probe (P0586). The mock has no WAL window —
    /// a chunk in `state.chunks` is "durable". Bit i set ⇔ `digests[i]`
    /// is present, LSB-first within each byte (the `HasBitmap`
    /// contract shared with HasDirectories/HasBlobs).
    async fn has_chunks(
        &self,
        request: Request<types::HasChunksRequest>,
    ) -> Result<Response<types::HasBitmap>, Status> {
        let digests = request.into_inner().digests;
        let chunks = self.state.chunks.read().unwrap();
        let mut bitmap = vec![0u8; digests.len().div_ceil(8)];
        for (i, d) in digests.iter().enumerate() {
            if chunks.contains_key(d) {
                bitmap[i / 8] |= 1 << (i % 8);
            }
        }
        Ok(Response::new(types::HasBitmap { bitmap }))
    }
}

impl MockStore {
    /// Shared fault-injection block for `query_path_info` AND
    /// `substitute_path`. Tests written against the unary QPI's
    /// `fail_query_path_info*` knobs continue to work after the
    /// scheduler's closure walk switched to the streaming RPC.
    /// Records attempt under `qpi_attempts_by_path` BEFORE any short-
    /// circuit so retry tests can assert structurally.
    fn check_qpi_faults(&self, path: &str) -> Result<(), Status> {
        let attempt = {
            let mut m = self.calls.qpi_attempts_by_path.write().unwrap();
            let n = m.entry(path.to_string()).or_insert(0);
            *n += 1;
            *n
        };
        let per_path_n = self
            .faults
            .fail_qpi_resource_exhausted_per_path_n
            .load(Ordering::SeqCst);
        if per_path_n > 0 && attempt <= per_path_n {
            return Err(Status::resource_exhausted(
                "mock: injected query_path_info ResourceExhausted (per-path-n)",
            ));
        }
        let nf_per_path_n = self
            .faults
            .fail_qpi_not_found_per_path_n
            .load(Ordering::SeqCst);
        if nf_per_path_n > 0 && attempt <= nf_per_path_n {
            return Err(Status::not_found(
                "mock: injected query_path_info NotFound (per-path-n)",
            ));
        }
        if self
            .faults
            .fail_qpi_internal_paths
            .read()
            .unwrap()
            .contains(path)
        {
            return Err(Status::internal(
                "mock: injected query_path_info permanent failure (per-path)",
            ));
        }
        if self.faults.fail_query_path_info.load(Ordering::SeqCst) {
            return Err(Status::unavailable(
                "mock: injected query_path_info failure",
            ));
        }
        if self
            .faults
            .fail_query_path_info_permanent
            .load(Ordering::SeqCst)
        {
            return Err(Status::internal(
                "mock: injected query_path_info permanent failure",
            ));
        }
        if self
            .faults
            .fail_query_path_info_n_times
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(Status::unavailable(
                "mock: injected query_path_info transient failure (n_times)",
            ));
        }
        if let Some(until) = *self
            .faults
            .fail_qpi_resource_exhausted_until
            .read()
            .unwrap()
            && std::time::Instant::now() < until
        {
            return Err(Status::resource_exhausted(
                "mock: injected query_path_info ResourceExhausted (time-gated)",
            ));
        }
        Ok(())
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
            .shed_next_puts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok()
        {
            // The store's NAR-budget shed shape, verbatim class+invite
            // (put_path/common.rs): typed ResourceExhausted, "retry".
            return Err(Status::resource_exhausted(
                "PutPath: NAR buffer budget wait exceeded 180s (pod at its \
                 in-flight NAR-bytes bound); retry",
            ));
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
        let _ = rio_nix::store_path::StorePath::parse(&info.store_path)
            .map_err(|e| Status::invalid_argument(format!("mock: invalid store path: {e}")))?;
        // Early-Ok injection: real store returns Ok(created:false)
        // without draining when `claim_placeholder` → AlreadyComplete
        // and `drain_stream` times out. Record the call (gateway tests
        // assert on put_calls.len()) then return — chunks/trailer left
        // unread on the wire.
        if self
            .faults
            .put_path_early_ok_paths
            .read()
            .unwrap()
            .contains(&info.store_path)
        {
            self.calls.put_calls.write().unwrap().push(info.clone());
            return Ok(Response::new(types::PutPathResponse { created: false }));
        }
        // r[impl ts.mock.store-put-validate]
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
                    let _ = rio_nix::store_path::StorePath::parse(&i.store_path).map_err(|e| {
                        Status::invalid_argument(format!("mock: invalid store path: {e}"))
                    })?;
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
        // Count BEFORE any fault-injection early-return so tests can
        // assert "client never contacted gRPC" (== 0) vs "client did
        // and got Unavailable" (> 0).
        self.calls.get_path_calls.fetch_add(1, Ordering::SeqCst);
        self.calls.get_path_tokens.write().unwrap().push(
            request
                .metadata()
                .get(rio_proto::ASSIGNMENT_TOKEN_HEADER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
        );
        if let Some(code) = *self.faults.get_path_status.read().unwrap() {
            return Err(Status::new(code, "mock: injected get_path status"));
        }
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
        if self
            .faults
            .query_path_info_gate_armed
            .load(Ordering::SeqCst)
        {
            self.faults.query_path_info_gate.notified().await;
        }
        self.check_qpi_faults(&request.get_ref().store_path)?;
        let store_path = request.into_inner().store_path;
        let _ = rio_nix::store_path::StorePath::parse(&store_path)
            .map_err(|e| Status::invalid_argument(format!("mock: invalid store path: {e}")))?;
        self.calls
            .qpi_calls
            .write()
            .unwrap()
            .push(store_path.clone());
        // Substitution side-effect simulation: if the path is seeded
        // as substitutable, materialize a minimal PathInfo on QPI —
        // mirrors the real store's try_substitute_on_miss. Surviving
        // QPI consumers (gateway/builder paths) depend on this
        // returning Some rather than NotFound (the walk-era eager
        // fetch this originally served retired with Phase D-prime).
        if self
            .state
            .substitutable
            .read()
            .unwrap()
            .contains(&store_path)
        {
            return Ok(Response::new(types::PathInfo {
                store_path,
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
        if self
            .faults
            .fail_query_path_info_permanent
            .load(Ordering::SeqCst)
        {
            return Err(Status::internal(
                "mock: injected query_path_info permanent failure",
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
        let paths = self.state.paths.read().unwrap();
        let entries = request
            .into_inner()
            .store_paths
            .into_iter()
            .map(|store_path| {
                // PathInfo only — mirrors the real store, which never
                // returns manifest content from BatchGetManifest.
                let hint = paths
                    .get(&store_path)
                    .map(|(info, _nar)| types::ManifestHint {
                        info: Some(info.clone()),
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
        let mut probe_tenant = request
            .metadata()
            .get(rio_proto::PROBE_TENANT_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        self.calls
            .find_missing_tenants
            .write()
            .unwrap()
            .push(probe_tenant.clone());
        if self.faults.fail_find_missing.load(Ordering::SeqCst) {
            return Err(Status::unavailable("mock: injected find_missing failure"));
        }
        // merged_bug_003: the real store resolves tenant scope from
        // EITHER the gateway-forwarded JWT (merge-time probes, I-202)
        // or the service-token-gated probe header (dispatch/settlement
        // probes). The mock's proxy for "verified scope" is header
        // presence on either channel. `drop_tenant_scope` simulates
        // the pre-Q3 downgrade: both channels ignored, no upstream
        // probe runs, echo false — wire-identical to confirmed 404s
        // (empty substitutable/indeterminate) except for the echo.
        let jwt_scoped = request
            .metadata()
            .get(rio_proto::TENANT_TOKEN_HEADER)
            .is_some();
        if self.faults.drop_tenant_scope.load(Ordering::SeqCst) {
            probe_tenant = None;
        }
        let scoped = (probe_tenant.is_some() || jwt_scoped)
            && !self.faults.drop_tenant_scope.load(Ordering::SeqCst);
        let requested = request.into_inner().store_paths;
        for p in &requested {
            let _ = rio_nix::store_path::StorePath::parse(p)
                .map_err(|e| Status::invalid_argument(format!("mock: invalid store path: {e}")))?;
        }
        self.calls
            .find_missing_paths_log
            .write()
            .unwrap()
            .push(requested.clone());
        let paths = self.state.paths.read().unwrap();
        // merged_bug_028: the per-tenant unobtainable script overrides
        // the global state for this request's tenant.
        let forced: Vec<String> = probe_tenant
            .as_deref()
            .and_then(|t| {
                self.state
                    .per_tenant_unobtainable
                    .read()
                    .unwrap()
                    .get(t)
                    .cloned()
            })
            .unwrap_or_default();
        let missing: Vec<String> = requested
            .into_iter()
            .filter(|p| !paths.contains_key(p) || forced.contains(p))
            .collect();
        // Substitutable ⊆ missing: only report paths that were
        // requested-and-missing AND seeded as substitutable. A seeded
        // path not in this request's missing set stays out (the real
        // store only checks upstream for paths it doesn't have).
        // merged_bug_003: a scope-less request runs NO upstream probe
        // — empty substitutable/indeterminate, exactly the wire shape
        // the real store produces for anonymous callers.
        let (substitutable, indeterminate) = if scoped {
            let subs = self.state.substitutable.read().unwrap();
            let ind = self.state.indeterminate.read().unwrap();
            (
                missing
                    .iter()
                    .filter(|p| subs.contains(p) && !ind.contains(p) && !forced.contains(p))
                    .cloned()
                    .collect(),
                missing
                    .iter()
                    .filter(|p| ind.contains(p) && !forced.contains(p))
                    .cloned()
                    .collect(),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        Ok(Response::new(types::FindMissingPathsResponse {
            missing_paths: missing,
            substitutable_paths: substitutable,
            indeterminate_paths: indeterminate,
            probe_ran_tenant_scoped: scoped,
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
        self.calls.query_realisation_metadata.write().unwrap().push(
            request
                .metadata()
                .get(rio_proto::TENANT_TOKEN_HEADER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
        );
        if self.faults.fail_query_realisation.load(Ordering::SeqCst) {
            return Err(Status::unavailable(
                "mock: injected query_realisation failure",
            ));
        }
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

    async fn append_hw_perf_sample(
        &self,
        _request: Request<types::AppendHwPerfSampleRequest>,
    ) -> Result<Response<()>, Status> {
        // No mock state — the only caller (rio-builder hw_bench::report)
        // is best-effort and the bench's effect is read by the
        // SCHEDULER (HwTable::load), not anything that goes through
        // MockStore. Accept and discard.
        Ok(Response::new(()))
    }

    async fn get_nar_index(
        &self,
        request: Request<types::GetNarIndexRequest>,
    ) -> Result<Response<types::NarIndex>, Status> {
        let nar_hash = request.into_inner().nar_hash;
        self.state
            .nar_indexes
            .read()
            .unwrap()
            .get(&nar_hash)
            .cloned()
            .map(Response::new)
            .ok_or_else(|| Status::not_found("mock: no NAR index for that hash"))
    }

    type GetNarIndexBatchStream =
        tokio_stream::wrappers::ReceiverStream<Result<types::NarIndexResponse, Status>>;

    /// One `NarIndexResponse` per requested hash, request order
    /// preserved; `index: None` for unseeded hashes — same "absent =
    /// not indexed yet" contract as the real handler, which is what
    /// the builder's root-node fallback error path keys on.
    async fn get_nar_index_batch(
        &self,
        request: Request<types::GetNarIndexBatchRequest>,
    ) -> Result<Response<Self::GetNarIndexBatchStream>, Status> {
        let hashes = request.into_inner().nar_hashes;
        // Scope the read guard so it is dropped before the awaits below
        // (the guard is not Send and the future must be).
        let responses: Vec<types::NarIndexResponse> = {
            let indexes = self.state.nar_indexes.read().unwrap();
            hashes
                .into_iter()
                .map(|nar_hash| types::NarIndexResponse {
                    index: indexes.get(&nar_hash).cloned(),
                    nar_hash,
                })
                .collect()
        };
        let (tx, rx) = tokio::sync::mpsc::channel(responses.len().max(1));
        for resp in responses {
            let _ = tx.send(Ok(resp)).await;
        }
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    /// PutPathChunked (P0586 builder upload path). Mirrors the real
    /// store's wire contract closely enough that a client regression
    /// (wrong frame order, chunk body not matching its digest, missing
    /// or extra chunk frames) fails here instead of only against
    /// rio-store:
    ///
    /// - the first frame must be `Begin`;
    /// - every `novel` digest is 32 bytes and duplicate-free;
    /// - chunk frames arrive in exactly `novel` order, each body
    ///   hashing to its digest;
    /// - the stream carries exactly one frame per `novel` entry.
    ///
    /// Effects: received chunks land in `state.chunks` (so `HasChunks`
    /// reflects the upload), each output's PathInfo lands in
    /// `state.paths` (so `FindMissingPaths`/`QueryPathInfo` see it),
    /// and the whole call is recorded in `calls.put_chunked_calls`.
    /// The NAR bytes stored for each path are EMPTY — the mock does not
    /// regenerate NAR framing from the castore tree; tests that need
    /// `GetPath` round-trips drive the real rio-store handler instead.
    /// `created[i]` is false when the path was already present
    /// (idempotency mirror).
    async fn put_path_chunked(
        &self,
        request: Request<Streaming<types::PutPathChunkedRequest>>,
    ) -> Result<Response<types::PutPathChunkedResponse>, Status> {
        // Same transient-failure knob as PutPath/PutPathBatch — one
        // decrement per RPC, for retry tests.
        if self
            .faults
            .fail_next_puts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok()
        {
            return Err(Status::unavailable("mock: injected chunked put failure"));
        }
        // Deterministic-rejection knob: the real store's chunk-backend
        // gate (FAILED_PRECONDITION before reading any frame). One
        // decrement per RPC so tests can count attempts structurally.
        if self
            .faults
            .reject_next_chunked_puts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok()
        {
            return Err(Status::failed_precondition(
                "mock: PutPathChunked requires a chunk backend",
            ));
        }

        let token = request
            .metadata()
            .get(rio_proto::ASSIGNMENT_TOKEN_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut stream = request.into_inner();

        let begin = match stream.message().await? {
            Some(types::PutPathChunkedRequest {
                msg: Some(types::put_path_chunked_request::Msg::Begin(b)),
            }) => b,
            Some(_) => {
                return Err(Status::invalid_argument(
                    "mock: first PutPathChunked frame must be Begin",
                ));
            }
            None => {
                return Err(Status::invalid_argument(
                    "mock: empty PutPathChunked stream",
                ));
            }
        };
        if begin.outputs.is_empty() {
            return Err(Status::invalid_argument("mock: Begin carries no outputs"));
        }
        for o in &begin.outputs {
            let _ = rio_nix::store_path::StorePath::parse(&o.store_path)
                .map_err(|e| Status::invalid_argument(format!("mock: invalid store path: {e}")))?;
            if o.nar_hash.len() != 32 {
                return Err(Status::invalid_argument(
                    "mock: ChunkedOutput.nar_hash must be 32 bytes",
                ));
            }
        }
        let mut novel_seen = std::collections::HashSet::new();
        for d in &begin.novel {
            if d.len() != 32 {
                return Err(Status::invalid_argument(
                    "mock: novel digest must be 32 bytes",
                ));
            }
            if !novel_seen.insert(d.clone()) {
                return Err(Status::invalid_argument("mock: duplicate novel digest"));
            }
        }

        // Chunk frames: exactly one per novel digest, in novel order,
        // each body hashing to its digest.
        let mut received: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(begin.novel.len());
        while let Some(msg) = stream.message().await? {
            let chunk = match msg.msg {
                Some(types::put_path_chunked_request::Msg::Chunk(c)) => c,
                _ => {
                    return Err(Status::invalid_argument(
                        "mock: only Chunk frames may follow Begin",
                    ));
                }
            };
            let idx = received.len();
            let Some(expected) = begin.novel.get(idx) else {
                return Err(Status::invalid_argument(
                    "mock: chunk frame after novel was exhausted",
                ));
            };
            if &chunk.digest != expected {
                return Err(Status::invalid_argument(format!(
                    "mock: chunk frame {idx} out of novel order"
                )));
            }
            if blake3::hash(&chunk.data).as_bytes() != chunk.digest.as_slice() {
                return Err(Status::invalid_argument(format!(
                    "mock: chunk frame {idx} body does not hash to its digest"
                )));
            }
            received.push((chunk.digest.clone(), chunk.data.to_vec()));
        }
        if received.len() != begin.novel.len() {
            return Err(Status::failed_precondition(format!(
                "mock: stream ended after {} of {} novel chunks",
                received.len(),
                begin.novel.len()
            )));
        }

        // Commit: chunks become probe-visible, paths become present.
        {
            let mut chunks = self.state.chunks.write().unwrap();
            for (digest, data) in &received {
                chunks.insert(digest.clone(), data.clone());
            }
        }
        let mut created = Vec::with_capacity(begin.outputs.len());
        {
            let mut paths = self.state.paths.write().unwrap();
            for o in &begin.outputs {
                if paths.contains_key(&o.store_path) {
                    created.push(false);
                    continue;
                }
                let info = types::PathInfo {
                    store_path: o.store_path.clone(),
                    nar_hash: o.nar_hash.clone(),
                    nar_size: o.nar_size,
                    references: o.references.clone(),
                    deriver: begin.deriver.clone(),
                    ..Default::default()
                };
                paths.insert(o.store_path.clone(), (info, Vec::new()));
                created.push(true);
            }
        }
        self.calls
            .put_chunked_calls
            .write()
            .unwrap()
            .push(RecordedChunkedPut {
                token,
                begin,
                chunk_digests: received.into_iter().map(|(d, _)| d).collect(),
            });
        Ok(Response::new(types::PutPathChunkedResponse { created }))
    }
}

// r[impl ts.mock.store-chunk]
/// `LogService` half of the mock store: a scriptable `TailLog` for the
/// gateway's live-tail subscriptions. `AppendLog` is unimplemented —
/// the builder's upload client has its own purpose-built mock
/// (`rio-builder/src/log_upload.rs`) that needs ack-level control this
/// shared mock doesn't.
#[tonic::async_trait]
impl rio_proto::store::log_service_server::LogService for MockStore {
    type AppendLogStream =
        tokio_stream::wrappers::ReceiverStream<Result<rio_proto::store::AppendLogAck, Status>>;
    type TailLogStream =
        tokio_stream::wrappers::ReceiverStream<Result<rio_proto::store::TailLogChunk, Status>>;

    async fn append_log(
        &self,
        _request: Request<Streaming<rio_proto::store::AppendLogRequest>>,
    ) -> Result<Response<Self::AppendLogStream>, Status> {
        Err(Status::unimplemented(
            "MockStore does not accept log appends",
        ))
    }

    async fn tail_log(
        &self,
        request: Request<rio_proto::store::TailLogRequest>,
    ) -> Result<Response<Self::TailLogStream>, Status> {
        let req = request.into_inner();
        let chunks = self
            .state
            .tail_logs
            .read()
            .unwrap()
            .get(&req.derivation)
            .cloned()
            .unwrap_or_default();
        let follow = req.follow;
        self.calls.tail_calls.write().unwrap().push(req);
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            for chunk in chunks {
                if tx.send(Ok(chunk)).await.is_err() {
                    return;
                }
            }
            // follow=false (the CLI / dashboard one-shot drain): the
            // real server ends the stream after the stored chunks.
            // Returning drops `tx` → the client's `message()` yields
            // `None` → the drain loop completes.
            if !follow {
                return;
            }
            // follow=true: hold the stream open — a live subscription
            // against a still-running build. Dropping the client side
            // (the gateway aborting the subscription at build terminus)
            // tears it down; the server task parks on the closed
            // notification rather than leaking a busy loop.
            tx.closed().await;
        });
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }
}

impl MockStore {
    /// Script the chunks one `TailLog` subscription for `derivation`
    /// receives. The subscription serves them in order and then stays
    /// open (a live `follow` stream). The key is matched verbatim
    /// against `TailLogRequest.derivation` — the gateway sends the full
    /// drv path, so seed with the full drv path.
    pub fn seed_tail_log(&self, derivation: &str, chunks: Vec<rio_proto::store::TailLogChunk>) {
        self.state
            .tail_logs
            .write()
            .unwrap()
            .insert(derivation.to_string(), chunks);
    }

    /// One `TailLogChunk` of UTF-8 lines starting at `first_line`.
    /// Convenience for [`Self::seed_tail_log`] callers.
    pub fn tail_chunk(first_line: u64, lines: &[&str]) -> rio_proto::store::TailLogChunk {
        rio_proto::store::TailLogChunk {
            exec_id: String::new(),
            lines: lines.iter().map(|l| l.as_bytes().to_vec()).collect(),
            first_line_number: first_line,
            is_complete: false,
        }
    }
}
