//! In-memory [`StoreService`] + [`ChunkService`] mock.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tonic::{Request, Response, Status, Streaming};

use prost::Message as _;
use rio_proto::types;
use rio_proto::validated::ValidatedPathInfo;
use rio_proto::{ChunkService, StoreService, castore};
use sha2::Digest as _;

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
    /// `r[sched.merge.substitute-probe-indeterminate]`
    pub indeterminate: Arc<RwLock<Vec<String>>>,
    /// Per-path `SubstitutePath` shape: `(nar_size, progress_ticks)`.
    /// When present, `substitute_path` streams each `(done, expected)`
    /// as a `Progress` message before the terminal `Info`, and uses
    /// `nar_size` for the terminal `Info.nar_size`. Absent key → no
    /// `Progress` emits, `nar_size = 1` (original behavior). Lets
    /// scheduler tests assert `walk_substitute_closure` aggregation
    /// invariants (done≤expected, monotone) without driving the real
    /// `Substituter`.
    #[allow(clippy::type_complexity)]
    pub subst_progress_ticks: Arc<RwLock<HashMap<String, (u64, Vec<(u64, u64)>)>>>,
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
    /// Per-path `query_path_info` attempt count, INCLUDING calls that
    /// fault-short-circuit (the early-return knobs above all skip
    /// `qpi_calls`). For r[sched.substitute.fanout-bound] structural
    /// retry assertions: `attempts[p] == N+1` proves N retries + 1
    /// success without a process-global metrics recorder.
    pub qpi_attempts_by_path: Arc<RwLock<HashMap<String, u32>>>,
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
    /// Number of `get_path` calls received. Incremented on entry,
    /// BEFORE the `fail_get_path` early-return — distinguishes "client
    /// never reached the RPC" from "RPC returned Unavailable". For
    /// structural no-gRPC-contact assertions in `rio-builder` FUSE
    /// fetch tests (replaces wall-clock `elapsed < backoff_floor`
    /// asserts that flaked under full-gate parallel load).
    pub get_path_calls: Arc<AtomicU32>,
    /// `x-rio-tenant-token` value on each QueryRealisation call
    /// (`None` = absent). For `r[gw.jwt.propagate]` — floating-CA
    /// output resolution in `wopBuildPathsWithResults`.
    pub query_realisation_metadata: Arc<RwLock<Vec<Option<String>>>>,
    /// One entry per `PutPathChunked` call that reached `Begin`
    /// processing: `(novel.len(), input_closure)`. For builder dedup
    /// tests (a re-upload of identical content must send zero novel
    /// chunks) and `Begin.input_closure` passthrough assertions.
    #[allow(clippy::type_complexity)]
    pub chunked_begins: Arc<RwLock<Vec<(usize, Vec<String>)>>>,
}

/// Fault injection knobs. All default to "no fault"; tests flip them
/// to exercise error paths.
// r[impl ts.mock.store-faults]
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
    /// the r[sched.substitute.fanout-bound] regression test asserts
    /// structurally — every path retried exactly N times — without a
    /// wall-clock window.
    pub fail_qpi_resource_exhausted_per_path_n: Arc<std::sync::atomic::AtomicU32>,
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
    /// If true, `put_path_chunked` returns the same `FailedPrecondition`
    /// a real store without a chunk backend emits
    /// (`CHUNKED_REQUIRES_BACKEND_MSG`). For builder fallback tests:
    /// the upload must degrade to the legacy `PutPath`/`PutPathBatch`
    /// path instead of failing the build.
    pub unimplement_put_path_chunked: Arc<AtomicBool>,
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

    /// Durable-presence probe (ADR-022 §6.2). The mock has no
    /// `durable` flag — every seeded chunk is "durable" (a bit is set
    /// iff the digest is in `state.chunks`). Tests that need the
    /// not-yet-durable distinction use the real store against
    /// ephemeral PG.
    async fn has_chunks(
        &self,
        request: Request<types::HasChunksRequest>,
    ) -> Result<Response<types::HasChunksResponse>, Status> {
        let digests = request.into_inner().digests;
        let chunks = self.state.chunks.read().unwrap();
        let mut bitmap = vec![0u8; digests.len().div_ceil(8)];
        for (i, d) in digests.iter().enumerate() {
            if chunks.contains_key(d) {
                bitmap[i / 8] |= 1 << (i % 8);
            }
        }
        Ok(Response::new(types::HasChunksResponse { bitmap }))
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

/// NAR wire string framing: `u64le(len) ++ bytes ++ pad-to-8`. Local
/// helper for the mock's chunked-NAR reconstruction — `rio_nix`'s
/// `sync_wire` module is `pub(super)`.
fn nar_wire_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    use rio_nix::protocol::wire::{ZERO_PAD, padding_len};
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
    let pad = padding_len(bytes.len());
    if pad > 0 {
        out.extend_from_slice(&ZERO_PAD[..pad]);
    }
}

fn nar_wire_str(out: &mut Vec<u8>, s: &str) {
    nar_wire_bytes(out, s.as_bytes());
}

/// Reconstruct one output's canonical NAR byte stream from its castore
/// root node + `Directory` bodies + chunked file contents. This is the
/// mock's *independent* implementation of the server's verify walk: a
/// builder whose fused walk emits wrong framing or misaligned chunks
/// produces a `nar_hash` that won't match the reconstruction, exactly
/// as the real store would reject it.
///
/// `next_chunk` is a cursor into the output's `chunk_manifest`: each
/// regular file consumes the contiguous run of chunks whose sizes sum
/// to its `FileEntry.size`.
struct NarRebuild<'a> {
    directories: &'a HashMap<[u8; 32], castore::Directory>,
    chunk_manifest: &'a [types::ChunkRef],
    chunk_bodies: &'a HashMap<Vec<u8>, Vec<u8>>,
    cursor: usize,
}

impl NarRebuild<'_> {
    fn file(
        &mut self,
        out: &mut Vec<u8>,
        size: u64,
        executable: bool,
        digest: &[u8],
    ) -> Result<(), Status> {
        use rio_nix::protocol::wire::{ZERO_PAD, padding_len};
        nar_wire_str(out, "regular");
        if executable {
            nar_wire_str(out, "executable");
            nar_wire_str(out, "");
        }
        nar_wire_str(out, "contents");
        out.extend_from_slice(&size.to_le_bytes());
        // Splice the contiguous chunk run for this file. Per-file
        // alignment: the run MUST sum to exactly `size`.
        let mut got: u64 = 0;
        let mut file_hasher = blake3::Hasher::new();
        while got < size {
            let c = self.chunk_manifest.get(self.cursor).ok_or_else(|| {
                Status::invalid_argument(format!(
                    "mock: chunk_manifest exhausted at {got}/{size} bytes into a file"
                ))
            })?;
            self.cursor += 1;
            let body = self.chunk_bodies.get(&c.hash).ok_or_else(|| {
                Status::invalid_argument(format!(
                    "mock: chunk {} not sent and not previously seeded",
                    hex::encode(&c.hash)
                ))
            })?;
            if body.len() != c.size as usize {
                return Err(Status::invalid_argument(format!(
                    "mock: chunk {} body is {} bytes but manifest says {}",
                    hex::encode(&c.hash),
                    body.len(),
                    c.size
                )));
            }
            out.extend_from_slice(body);
            file_hasher.update(body);
            got += u64::from(c.size);
        }
        if got != size {
            return Err(Status::invalid_argument(format!(
                "mock: file chunk run sums to {got}, FileEntry.size is {size} \
                 (chunks not per-file-aligned)"
            )));
        }
        if size > 0 && file_hasher.finalize().as_bytes() != digest {
            return Err(Status::failed_precondition(
                "mock: file digest mismatch (FileEntry.digest != blake3(contents))",
            ));
        }
        let pad = padding_len(size as usize);
        if pad > 0 {
            out.extend_from_slice(&ZERO_PAD[..pad]);
        }
        nar_wire_str(out, ")");
        Ok(())
    }

    fn dir(&mut self, out: &mut Vec<u8>, digest: &[u8]) -> Result<(), Status> {
        let key: [u8; 32] = digest
            .try_into()
            .map_err(|_| Status::invalid_argument("mock: dir_digest must be 32 bytes"))?;
        let dir = self.directories.get(&key).ok_or_else(|| {
            Status::invalid_argument(format!(
                "mock: Directory body {} not in Begin.directories",
                hex::encode(digest)
            ))
        })?;
        nar_wire_str(out, "directory");
        // Merge the three kind-partitioned lists back into byte-lex
        // name order (the canonical NAR entry order).
        enum E<'a> {
            D(&'a castore::DirectoryEntry),
            F(&'a castore::FileEntry),
            S(&'a castore::SymlinkEntry),
        }
        let mut entries: Vec<(&[u8], E<'_>)> = Vec::new();
        for d in &dir.directories {
            entries.push((&d.name, E::D(d)));
        }
        for f in &dir.files {
            entries.push((&f.name, E::F(f)));
        }
        for s in &dir.symlinks {
            entries.push((&s.name, E::S(s)));
        }
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (name, e) in entries {
            nar_wire_str(out, "entry");
            nar_wire_str(out, "(");
            nar_wire_str(out, "name");
            nar_wire_bytes(out, name);
            nar_wire_str(out, "node");
            nar_wire_str(out, "(");
            nar_wire_str(out, "type");
            match e {
                E::D(d) => self.dir(out, &d.digest)?,
                E::F(f) => self.file(out, f.size, f.executable, &f.digest)?,
                E::S(s) => {
                    nar_wire_str(out, "symlink");
                    nar_wire_str(out, "target");
                    nar_wire_bytes(out, &s.target);
                    nar_wire_str(out, ")");
                }
            }
            nar_wire_str(out, ")");
        }
        nar_wire_str(out, ")");
        Ok(())
    }
}

#[tonic::async_trait]
impl StoreService for MockStore {
    /// Models the server's verify-then-commit: collects the `Begin` +
    /// `Chunk` frames, checks the wire-order/digest invariants the real
    /// store enforces, *independently reconstructs* each output's NAR
    /// from the castore tree + chunk bodies, and rejects on
    /// `nar_hash`/`nar_size` disagreement. Committed NARs land in
    /// `state.paths` (so `GetPath`/`QueryPathInfo` serve them) and the
    /// chunk bodies in `state.chunks` (so a subsequent `HasChunks`
    /// reports them present — the dedup path).
    async fn put_path_chunked(
        &self,
        request: Request<Streaming<types::PutPathChunkedRequest>>,
    ) -> Result<Response<types::PutPathResponse>, Status> {
        // Capability gate FIRST — a store that structurally cannot do
        // chunked uploads rejects before any transient-fault modeling,
        // so tests that arm both `unimplement_put_path_chunked` and
        // `fail_next_puts` exercise the LEGACY path's retry behavior
        // (the chunked attempt never consumes a fail_next_puts charge).
        if self
            .faults
            .unimplement_put_path_chunked
            .load(Ordering::SeqCst)
        {
            // Models a pre-ADR-022 store / a store without a chunk
            // backend: the builder must fall back to the legacy path.
            return Err(Status::failed_precondition(format!(
                "{}; this store is inline-only",
                rio_proto::CHUNKED_REQUIRES_BACKEND_MSG
            )));
        }
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

        let mut stream = request.into_inner();
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty PutPathChunked stream"))?;
        let begin = match first.msg {
            Some(types::put_path_chunked_request::Msg::Begin(b)) => b,
            _ => {
                return Err(Status::invalid_argument(
                    "first PutPathChunked message must be Begin",
                ));
            }
        };

        self.calls
            .chunked_begins
            .write()
            .unwrap()
            .push((begin.novel.len(), begin.input_closure.clone()));

        // Index Directory bodies by their recomputed digest.
        let mut directories: HashMap<[u8; 32], castore::Directory> = HashMap::new();
        for d in &begin.directories {
            directories.insert(*blake3::hash(&d.encode_to_vec()).as_bytes(), d.clone());
        }

        // Drain Chunk frames: exactly one per Begin.novel entry, in
        // Begin.novel order, each hashing to its declared digest.
        let mut bodies: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        let mut next_novel = 0usize;
        while let Some(msg) = stream.message().await? {
            let chunk = match msg.msg {
                Some(types::put_path_chunked_request::Msg::Chunk(c)) => c,
                _ => return Err(Status::invalid_argument("mock: duplicate Begin")),
            };
            let expected = begin.novel.get(next_novel).ok_or_else(|| {
                Status::invalid_argument("mock: Chunk frame after novel exhausted")
            })?;
            if &chunk.digest != expected {
                return Err(Status::invalid_argument(format!(
                    "mock: chunk frame {} out of novel order: got {}, expected {}",
                    next_novel,
                    hex::encode(&chunk.digest),
                    hex::encode(expected),
                )));
            }
            if blake3::hash(&chunk.data).as_bytes() != chunk.digest.as_slice() {
                return Err(Status::invalid_argument(format!(
                    "mock: chunk {} bytes do not hash to declared digest",
                    hex::encode(&chunk.digest)
                )));
            }
            bodies.insert(chunk.digest.clone(), chunk.data);
            next_novel += 1;
        }
        if next_novel < begin.novel.len() {
            return Err(Status::failed_precondition(
                "mock: stream ended before all novel chunks arrived",
            ));
        }
        // Deduped chunks come from prior uploads / seeds.
        {
            let seeded = self.state.chunks.read().unwrap();
            for o in &begin.outputs {
                for c in &o.chunk_manifest {
                    if !bodies.contains_key(&c.hash)
                        && let Some(b) = seeded.get(&c.hash)
                    {
                        bodies.insert(c.hash.clone(), b.clone());
                    }
                }
            }
        }

        // Reconstruct + verify + stage each output.
        let mut staged: Vec<(types::PathInfo, Vec<u8>)> = Vec::new();
        let mut created = false;
        for o in &begin.outputs {
            let _ = rio_nix::store_path::StorePath::parse(&o.store_path)
                .map_err(|e| Status::invalid_argument(format!("mock: invalid store path: {e}")))?;
            let mut nar = Vec::new();
            nar_wire_str(&mut nar, "nix-archive-1");
            nar_wire_str(&mut nar, "(");
            nar_wire_str(&mut nar, "type");
            let mut rebuild = NarRebuild {
                directories: &directories,
                chunk_manifest: &o.chunk_manifest,
                chunk_bodies: &bodies,
                cursor: 0,
            };
            // `dir()`/`file()`/the symlink arm each emit the `)` that
            // closes the `(` opened above — same containment as a
            // directory entry's `node ( type … )`.
            match o.root_node.as_ref().and_then(|r| r.node.as_ref()) {
                Some(castore::root_node::Node::DirDigest(d)) => rebuild.dir(&mut nar, d)?,
                Some(castore::root_node::Node::File(f)) => {
                    rebuild.file(&mut nar, f.size, f.executable, &f.digest)?
                }
                Some(castore::root_node::Node::Symlink(s)) => {
                    nar_wire_str(&mut nar, "symlink");
                    nar_wire_str(&mut nar, "target");
                    nar_wire_bytes(&mut nar, &s.target);
                    nar_wire_str(&mut nar, ")");
                }
                None => return Err(Status::invalid_argument("mock: root_node must be set")),
            }
            if rebuild.cursor != o.chunk_manifest.len() {
                return Err(Status::invalid_argument(format!(
                    "mock: {} chunk_manifest entries not consumed by the tree walk",
                    o.chunk_manifest.len() - rebuild.cursor
                )));
            }

            let computed: [u8; 32] = sha2::Sha256::digest(&nar).into();
            if computed.as_slice() != o.nar_hash.as_slice() {
                return Err(Status::failed_precondition(format!(
                    "mock: NAR hash mismatch for {}: claimed {}, reconstructed {}",
                    o.store_path,
                    hex::encode(&o.nar_hash),
                    hex::encode(computed),
                )));
            }
            if nar.len() as u64 != o.nar_size {
                return Err(Status::failed_precondition(format!(
                    "mock: NAR size mismatch for {}: claimed {}, reconstructed {}",
                    o.store_path,
                    o.nar_size,
                    nar.len(),
                )));
            }
            created |= !self.state.paths.read().unwrap().contains_key(&o.store_path);
            staged.push((
                types::PathInfo {
                    store_path: o.store_path.clone(),
                    nar_hash: o.nar_hash.clone(),
                    nar_size: o.nar_size,
                    references: o.refs.clone(),
                    deriver: begin.deriver.clone(),
                    ..Default::default()
                },
                nar,
            ));
        }

        // Commit all (atomic — nothing recorded if any output failed).
        self.state.chunks.write().unwrap().extend(bodies);
        for (info, nar) in staged {
            self.calls.put_calls.write().unwrap().push(info.clone());
            self.state
                .paths
                .write()
                .unwrap()
                .insert(info.store_path.clone(), (info, nar));
        }
        Ok(Response::new(types::PutPathResponse { created }))
    }

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
        // and got Unavailable" (> 0). `get_path_hints` below is pushed
        // only on the success path so it can't serve this purpose.
        self.calls.get_path_calls.fetch_add(1, Ordering::SeqCst);
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
        for p in &requested {
            let _ = rio_nix::store_path::StorePath::parse(p)
                .map_err(|e| Status::invalid_argument(format!("mock: invalid store path: {e}")))?;
        }
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
        let ind = self.state.indeterminate.read().unwrap();
        let substitutable: Vec<String> = missing
            .iter()
            .filter(|p| subs.contains(p) && !ind.contains(p))
            .cloned()
            .collect();
        let indeterminate: Vec<String> = missing
            .iter()
            .filter(|p| ind.contains(p))
            .cloned()
            .collect();
        Ok(Response::new(types::FindMissingPathsResponse {
            missing_paths: missing,
            substitutable_paths: substitutable,
            indeterminate_paths: indeterminate,
        }))
    }

    type SubstitutePathStream =
        tokio_stream::wrappers::ReceiverStream<Result<types::SubstitutePathResponse, Status>>;

    /// Mock SubstitutePath: behaves like `query_path_info`'s on-miss
    /// substitute fallback. If the path is seeded as `substitutable`,
    /// inserts it into `paths` (mirroring the real store's ingest) and
    /// streams a terminal `Info`. Otherwise NotFound. `Progress` emits
    /// + `nar_size` per `MockStoreState::subst_progress_ticks`; absent
    /// key → no `Progress`, `nar_size = 1`.
    async fn substitute_path(
        &self,
        request: Request<types::SubstitutePathRequest>,
    ) -> Result<Response<Self::SubstitutePathStream>, Status> {
        let store_path = request.into_inner().store_path;
        let _ = rio_nix::store_path::StorePath::parse(&store_path)
            .map_err(|e| Status::invalid_argument(format!("mock: invalid store path: {e}")))?;
        // Honor the same gate + fault knobs as `query_path_info`, then
        // record under `qpi_calls` ONLY on the success path (after the
        // fault block) — tests asserting "every path reached success
        // exactly once" (e.g. `cd83a9b2_cannot_recur`) rely on
        // `qpi_calls` excluding retries. `qpi_attempts_by_path` (inside
        // `check_qpi_faults`) records every attempt.
        if self
            .faults
            .query_path_info_gate_armed
            .load(Ordering::SeqCst)
        {
            self.faults.query_path_info_gate.notified().await;
        }
        self.check_qpi_faults(&store_path)?;
        self.calls
            .qpi_calls
            .write()
            .unwrap()
            .push(store_path.clone());
        let (nar_size, ticks) = self
            .state
            .subst_progress_ticks
            .read()
            .unwrap()
            .get(&store_path)
            .cloned()
            .unwrap_or((1, Vec::new()));
        let info = {
            if !self
                .state
                .substitutable
                .read()
                .unwrap()
                .contains(&store_path)
            {
                return Err(Status::not_found(format!("path not found: {store_path}")));
            }
            // Mirror QueryPathInfo's on-miss substitute: synthesize a
            // PathInfo and insert it so subsequent BatchQueryPathInfo /
            // FindMissingPaths see it as present.
            let info = types::PathInfo {
                store_path: store_path.clone(),
                nar_hash: vec![0u8; 32],
                nar_size,
                ..Default::default()
            };
            self.state
                .paths
                .write()
                .unwrap()
                .insert(store_path.clone(), (info.clone(), Vec::new()));
            info
        };
        let (tx, rx) = tokio::sync::mpsc::channel(ticks.len() + 1);
        for (done, expected) in ticks {
            let _ = tx
                .send(Ok(types::SubstitutePathResponse {
                    msg: Some(types::substitute_path_response::Msg::Progress(
                        types::SubstitutePathProgress {
                            bytes_done: done,
                            bytes_expected: expected,
                            upstream_uri: "mock://upstream".to_string(),
                        },
                    )),
                }))
                .await;
        }
        let _ = tx
            .send(Ok(types::SubstitutePathResponse {
                msg: Some(types::substitute_path_response::Msg::Info(info)),
            }))
            .await;
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
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

    // TODO(P0552): no consumers yet — castore-FUSE `tree::build_tree`
    // (P0559) prefetches via GetDirectory, not GetNarIndex.
    async fn get_nar_index(
        &self,
        _request: Request<types::GetNarIndexRequest>,
    ) -> Result<Response<types::NarIndex>, Status> {
        Err(Status::unimplemented("MockStore: GetNarIndex (P0552)"))
    }

    type GetNarIndexBatchStream =
        tokio_stream::wrappers::ReceiverStream<Result<types::NarIndexResponse, Status>>;

    async fn get_nar_index_batch(
        &self,
        _request: Request<types::GetNarIndexBatchRequest>,
    ) -> Result<Response<Self::GetNarIndexBatchStream>, Status> {
        Err(Status::unimplemented("MockStore: GetNarIndexBatch (P0552)"))
    }
}
