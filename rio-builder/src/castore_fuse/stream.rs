//! Streaming `open()` for large files (ADR-022 §2.8).
//!
//! The during-fill mode for a cache miss whose size exceeds the
//! streaming threshold: `open()` cannot reply passthrough (no complete
//! backing file exists yet), so it replies `FOPEN_KEEP_CACHE` as soon
//! as the first chunk of the file is verified and written, and a
//! background fill task assembles the rest into the build's staging
//! `.partial`. `read()` upcalls during the fill window block on the
//! fill's high-water mark and are served from the `.partial`; once the
//! fill completes (whole-file verify → rename → `Promote`) the next
//! `open()` of the same digest is a passthrough cache hit.
//!
//! Chunks are sourced from the mountd-owned node chunk cache
//! (`/var/rio/chunks/ab/<hex>`) first; misses go to the store's
//! batched `GetChunks` stream, are verified per-chunk on arrival, and
//! are staged + `PromoteChunks`d so concurrent builds on the node
//! share fetch progress at chunk granularity.

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use fuser::Errno;
use tonic::transport::Channel;

use rio_proto::store::chunk_service_client::ChunkServiceClient;
use rio_proto::store::directory_service_client::DirectoryServiceClient;
use rio_proto::types::{
    ChunkMeta, GetChunksRequest, ReadBlobRequest, StatBlobRequest, StatBlobResponse,
};

use super::circuit::CircuitBreaker;
use super::mountd_client::MountdClient;
use super::open::attach_token;
use crate::IgnorePoison;
use crate::store_fetch::jit_fetch_timeout;

/// Chunks fetched, written, and promoted per batch. Bounds the reorder
/// buffer (`WINDOW × MAX_CHUNK_BYTES` of chunk bytes held in memory
/// while a window's misses are in flight) and stays under mountd's
/// `PROMOTE_CHUNKS_MAX` (64) frame ceiling.
const WINDOW: usize = 32;

/// Defensive per-chunk size ceiling. FastCDC's max is 256 KiB; a
/// `ChunkMeta.size` beyond this is a server bug or a poisoned chunk
/// list, and honoring it would let the response size the client's
/// allocations.
const MAX_CHUNK_BYTES: u64 = 4 * 1024 * 1024;

/// Shared progress of one in-flight fill, joined by every concurrent
/// `open()` of the same `file_digest` within the build and by every
/// `read()` on the resulting file handles.
pub(super) struct FillState {
    /// The staging `.partial`, opened read-write. The fill task
    /// `pwrite`s verified bytes in file order; readers `pread` below
    /// the high-water mark — the two never touch the same offset.
    partial: File,
    /// Final file size; the high-water mark at fill completion.
    size: u64,
    progress: Mutex<Progress>,
    cv: Condvar,
}

#[derive(Default)]
struct Progress {
    /// Contiguous verified bytes available from offset 0.
    high_water: u64,
    /// Set exactly once when the fill task exits. `Err` wakes every
    /// blocked reader with that errno (the §2.7 fail-fast promise — a
    /// build never silently reads short or stale bytes).
    result: Option<Result<(), Errno>>,
}

impl FillState {
    pub(super) fn new(partial: File, size: u64) -> Self {
        Self {
            partial,
            size,
            progress: Mutex::new(Progress::default()),
            cv: Condvar::new(),
        }
    }

    /// Block until the first chunk is readable (or the fill already
    /// failed). This is the `open()` reply gate: the kernel's first
    /// `read(0, ..)` after the reply must find bytes.
    pub(super) fn wait_first_chunk(&self) -> Result<(), Errno> {
        let mut p = self.progress.lock().ignore_poison();
        loop {
            if let Some(Err(e)) = p.result {
                return Err(e);
            }
            if p.high_water > 0 || p.result.is_some() {
                return Ok(());
            }
            p = self.cv.wait(p).ignore_poison();
        }
    }

    /// Serve a `read()` upcall from the `.partial`, blocking until the
    /// fill's high-water mark covers the requested range. Reads past
    /// EOF return short (the kernel always reads in page-sized units
    /// regardless of the file size).
    // r[impl builder.fs.streaming-open]
    pub(super) fn read_at(&self, offset: u64, len: u32) -> Result<Vec<u8>, Errno> {
        if offset >= self.size {
            return Ok(Vec::new());
        }
        let want = self.size.min(offset + u64::from(len));
        {
            let mut p = self.progress.lock().ignore_poison();
            loop {
                // A failed fill poisons every subsequent read, even of
                // ranges already below the high-water mark: the bytes
                // are individually chunk-verified but the whole-file
                // digest check proved the assembly is not the file the
                // inode claims to be.
                if let Some(Err(e)) = p.result {
                    return Err(e);
                }
                if p.high_water >= want {
                    break;
                }
                // result == Some(Ok) implies high_water == size >= want,
                // so reaching here means the fill is still running.
                p = self.cv.wait(p).ignore_poison();
            }
        }
        let mut buf = vec![0u8; (want - offset) as usize];
        self.partial.read_exact_at(&mut buf, offset).map_err(|e| {
            tracing::warn!(offset, error = %e, "streaming read from .partial failed");
            Errno::EIO
        })?;
        Ok(buf)
    }

    /// Publish `high_water` bytes as readable and wake blocked readers.
    fn advance(&self, high_water: u64) {
        self.progress.lock().ignore_poison().high_water = high_water;
        self.cv.notify_all();
    }

    /// Record the fill's terminal outcome and wake every waiter. The
    /// first call wins; later calls (the abandon guard racing a normal
    /// exit) are no-ops.
    fn finish(&self, result: Result<(), Errno>) {
        let mut p = self.progress.lock().ignore_poison();
        if p.result.is_none() {
            p.result = Some(result);
            self.cv.notify_all();
        }
    }
}

/// Releases everything a fill holds on behalf of the rest of the build,
/// no matter how the task exits — normal return, error, panic unwind,
/// or runtime-shutdown cancellation at an await point:
///
///   - wakes blocked readers with `EIO` if no terminal result was set
///     (they would otherwise park forever on the high-water mark);
///   - reports the fetch outcome to the circuit breaker if the task
///     never did — the `open()` that spawned this fill may hold the
///     breaker's half-open probe claim, and an unreported exit would
///     leave it claimed forever (every later cold open EIOs while the
///     heartbeat still reports healthy);
///   - removes the staging `.partial` (a no-op after the success-path
///     rename);
///   - removes the digest from `active_fills` so a later `open()` can
///     retry instead of joining a dead fill for the rest of the build.
struct FillGuard {
    state: Arc<FillState>,
    active: Arc<Mutex<HashMap<[u8; 32], Arc<FillState>>>>,
    digest: [u8; 32],
    circuit: Arc<CircuitBreaker>,
    partial: PathBuf,
    recorded: bool,
}

impl FillGuard {
    /// Report the store-fetch outcome to the circuit breaker. Exactly
    /// once per fill — the `Drop` fallback fires only if the task never
    /// got here (panic or cancellation).
    fn record(&mut self, ok: bool) {
        self.circuit.record(ok);
        self.recorded = true;
    }
}

impl Drop for FillGuard {
    fn drop(&mut self) {
        if !self.recorded {
            // Panic or cancellation before the outcome was reported:
            // count it as a failure so a claimed half-open probe is
            // released rather than leaked.
            self.circuit.record(false);
        }
        // No-op if a terminal result was already set.
        self.state.finish(Err(Errno::EIO));
        // Remove the partial before the active_fills entry: a new fill
        // for this digest can only start once the entry is gone.
        let _ = std::fs::remove_file(&self.partial);
        self.active.lock().ignore_poison().remove(&self.digest);
    }
}

/// Everything the background fill task needs, cloned out of the
/// [`super::open::Opener`] so the task outlives the `open()` callback
/// that spawned it.
pub(super) struct FillCtx {
    pub digest: [u8; 32],
    pub size: u64,
    pub directory: DirectoryServiceClient<Channel>,
    pub chunk: ChunkServiceClient<Channel>,
    /// Assignment token for the StatBlob/ReadBlob/GetChunks calls the
    /// fill makes — same credential the whole-file path attaches.
    pub assignment_token: String,
    pub mountd: MountdClient,
    pub circuit: Arc<CircuitBreaker>,
    /// Node-shared chunk cache root (`/var/rio/chunks`), mountd-owned.
    pub chunks_dir: PathBuf,
    /// This build's staging dir (`/var/rio/staging/{build_id}`).
    pub staging_dir: PathBuf,
    pub fetch_timeout: Duration,
    pub mountd_request_timeout: Duration,
}

/// The background fill task: fetch → verify → rename → `Promote`.
/// Always reaches a terminal [`FillState::finish`] and always removes
/// itself from `active` — the [`FillGuard`] covers panic and
/// cancellation, so no exit path can wedge the digest for the build.
pub(super) async fn run_fill(
    ctx: FillCtx,
    state: Arc<FillState>,
    active: Arc<Mutex<HashMap<[u8; 32], Arc<FillState>>>>,
) {
    let hexd = hex::encode(ctx.digest);
    let partial = ctx.staging_dir.join(format!("{hexd}.partial"));
    let staged = ctx.staging_dir.join(&hexd);
    let mut guard = FillGuard {
        state: Arc::clone(&state),
        active,
        digest: ctx.digest,
        circuit: Arc::clone(&ctx.circuit),
        partial: partial.clone(),
        recorded: false,
    };

    let outcome = fill(&ctx, &state).await;
    // The circuit records the store-fetch outcome; Promote (a mountd
    // round-trip) is excluded, same as the whole-file path.
    guard.record(outcome.is_ok());

    let result = match outcome {
        Err(e) => Err(e),
        Ok(()) => match std::fs::rename(&partial, &staged) {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(digest = %hexd, error = %e, "staging finalize failed");
                Err(Errno::EIO)
            }
        },
    };

    // Readers are unblocked before the Promote round-trip: they pread
    // the (verified, renamed) staging file through the already-open fd,
    // and the Promote only matters for the *next* open of this digest.
    state.finish(result);

    if result.is_ok() {
        // Best-effort: a Promote failure leaves the verified bytes
        // readable from staging for every fh already open; only the
        // cross-build/cross-open cache entry is lost, and the next
        // open() re-streams and re-attempts it.
        let mountd = ctx.mountd.clone();
        let digest = ctx.digest;
        let timeout = jit_fetch_timeout(ctx.mountd_request_timeout, ctx.size);
        match tokio::task::spawn_blocking(move || mountd.promote(digest, timeout)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(digest = %hexd, error = %e, "Promote after streaming fill failed")
            }
            Err(join) => {
                tracing::warn!(digest = %hexd, error = %join, "Promote task panicked")
            }
        }
    }
    // The guard's Drop removes the leftover `.partial` (failure paths)
    // and the active_fills entry.
}

/// Fetch and assemble the file's bytes into the `.partial`, verifying
/// the whole-file blake3 against `ctx.digest`. Every source path must
/// fall through to the digest check at the bottom — an early return on
/// success would let a wrong-but-complete stream reach the cache.
// r[impl builder.fs.file-digest-integrity]
async fn fill(ctx: &FillCtx, state: &FillState) -> Result<(), Errno> {
    let mut hasher = blake3::Hasher::new();
    let mut directory = ctx.directory.clone();
    let mut req = tonic::Request::new(StatBlobRequest {
        file_digest: ctx.digest.to_vec(),
        send_chunks: true,
    });
    attach_token(&mut req, &ctx.assignment_token)?;
    match tokio::time::timeout(ctx.fetch_timeout, directory.stat_blob(req)).await {
        Err(_elapsed) => {
            tracing::warn!(digest = %hex::encode(ctx.digest), "StatBlob timed out");
            return Err(Errno::EIO);
        }
        // Inline manifests have no chunk list; the file's bytes are
        // only reachable as a whole-file ReadBlob stream. Same fill
        // contract (in-order frames, high-water advance), no chunk
        // cache participation.
        Ok(Err(status)) if status.code() == tonic::Code::FailedPrecondition => {
            fill_from_readblob(ctx, state, &mut hasher).await?;
        }
        Ok(Err(status)) => {
            tracing::warn!(digest = %hex::encode(ctx.digest), %status, "StatBlob failed");
            return Err(Errno::EIO);
        }
        Ok(Ok(resp)) => {
            let window = resp.into_inner();
            validate_window(&window, ctx.size).map_err(|why| {
                tracing::error!(
                    digest = %hex::encode(ctx.digest),
                    why,
                    "StatBlob chunk window is inconsistent with the file size"
                );
                Errno::EIO
            })?;
            fill_from_chunks(ctx, state, &window, &mut hasher).await?;
        }
    }

    if hasher.finalize().as_bytes() != &ctx.digest {
        metrics::counter!("rio_builder_castore_fuse_integrity_fail_total").increment(1);
        tracing::error!(
            want = %hex::encode(ctx.digest),
            "streamed content does not match file_digest"
        );
        return Err(Errno::EIO);
    }
    Ok(())
}

/// Check that the chunk window reassembles to exactly `size` bytes
/// before fetching anything. A malformed window would otherwise be
/// caught only by the whole-file hash at fill-complete — after the
/// build has already consumed the wrong bytes.
fn validate_window(window: &StatBlobResponse, size: u64) -> Result<(), &'static str> {
    if window.chunks.is_empty() {
        return Err("empty chunk list for a non-empty file");
    }
    // Repeated digests are legitimate (content-defined chunking of
    // repetitive data), but every occurrence must agree on the size:
    // the batch fetch dedups by digest and verifies the bytes against
    // the FIRST occurrence only, so a later occurrence claiming a
    // larger size would slice past the fetched buffer.
    let mut sizes: HashMap<&[u8], u64> = HashMap::new();
    for c in &window.chunks {
        if c.digest.len() != 32 {
            return Err("chunk digest is not 32 bytes");
        }
        if c.size == 0 || c.size > MAX_CHUNK_BYTES {
            return Err("chunk size out of range");
        }
        if *sizes.entry(c.digest.as_slice()).or_insert(c.size) != c.size {
            return Err("repeated chunk digest with inconsistent sizes");
        }
    }
    let mut total: u64 = 0;
    for (i, c) in window.chunks.iter().enumerate() {
        let (skip, take) = slice_bounds(i, window, c.size).ok_or("slice out of range")?;
        total = total.saturating_add(take - skip);
    }
    if total != size {
        return Err("sliced chunk lengths do not sum to the file size");
    }
    Ok(())
}

/// `(start, end)` byte range of chunk `i`'s contribution, per the
/// `StatBlobResponse` reassembly contract: chunk 0 starts at
/// `first_chunk_skip`, the last chunk ends at `last_chunk_take`, and a
/// single-chunk file applies both to the same chunk.
fn slice_bounds(i: usize, window: &StatBlobResponse, chunk_size: u64) -> Option<(u64, u64)> {
    let n = window.chunks.len();
    let skip = if i == 0 {
        u64::from(window.first_chunk_skip)
    } else {
        0
    };
    let take = if i == n - 1 {
        u64::from(window.last_chunk_take)
    } else {
        chunk_size
    };
    (skip <= take && take <= chunk_size).then_some((skip, take))
}

/// Assemble the file from its chunk window, [`WINDOW`] chunks at a
/// time: probe the node chunk cache for each, batch the misses into one
/// `GetChunks` request frame, write the batch in file order, then stage
/// + `PromoteChunks` the misses for other builds on the node.
// r[impl builder.fs.node-chunk-cache]
async fn fill_from_chunks(
    ctx: &FillCtx,
    state: &FillState,
    window: &StatBlobResponse,
    hasher: &mut blake3::Hasher,
) -> Result<(), Errno> {
    let mut offset: u64 = 0;
    // The GetChunks bidi stream is opened lazily on the first miss and
    // reused for every subsequent batch — one HTTP/2 stream per fill,
    // not per window. Safe for arbitrarily large files: the server
    // bounds the stream with an IDLE timeout, not an absolute lifetime
    // cap (see rio-store `ChunkServiceImpl::stream_idle_timeout`), and
    // this loop never idles longer than one batch's local work.
    let mut remote: Option<RemoteChunks> = None;

    // The first batch is a single chunk so the open() reply gate (first
    // chunk written) is one chunk RTT away, not WINDOW chunk transfers.
    let mut batches = Vec::with_capacity(1 + window.chunks.len() / WINDOW);
    let mut start = 0;
    while start < window.chunks.len() {
        let end = if start == 0 {
            1
        } else {
            (start + WINDOW).min(window.chunks.len())
        };
        batches.push(start..end);
        start = end;
    }

    for batch in batches {
        let metas = &window.chunks[batch.clone()];
        // Probe the node chunk cache; collect the misses. Both maps are
        // keyed by digest, so a chunk that occurs more than once in the
        // batch (content-defined chunking of repetitive data) is read
        // or fetched once and written at every occurrence.
        let mut local: HashMap<&[u8], Vec<u8>> = HashMap::new();
        let mut misses: Vec<&ChunkMeta> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for meta in metas {
            if !seen.insert(meta.digest.as_slice()) {
                continue;
            }
            match read_local_chunk(&ctx.chunks_dir, meta) {
                Some(data) => {
                    metrics::counter!("rio_builder_castore_fuse_fetch_bytes_total", "hit" => "node_ssd")
                        .increment(data.len() as u64);
                    local.insert(&meta.digest, data);
                }
                None => misses.push(meta),
            }
        }

        let mut fetched: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        if !misses.is_empty() {
            let stream = match &mut remote {
                Some(s) => s,
                None => remote.insert(
                    RemoteChunks::open(ctx.chunk.clone(), &ctx.assignment_token, ctx.fetch_timeout)
                        .await?,
                ),
            };
            let miss_bytes: u64 = misses.iter().map(|m| m.size).sum();
            fetched = stream
                .fetch(&misses, jit_fetch_timeout(ctx.fetch_timeout, miss_bytes))
                .await?;
            metrics::counter!("rio_builder_castore_fuse_fetch_bytes_total", "hit" => "remote")
                .increment(miss_bytes);
        }

        // Write the batch in file order, advancing the high-water mark
        // per chunk so a reader blocked mid-batch wakes as soon as its
        // range lands.
        for (i, meta) in batch.clone().zip(metas) {
            let data = local
                .get(meta.digest.as_slice())
                .or_else(|| fetched.get(&meta.digest))
                .expect("every chunk in the batch is either local or fetched");
            let (skip, take) = slice_bounds(i, window, meta.size)
                .expect("validated against the chunk size before fetching");
            // validate_window pins repeated digests to one size, so the
            // fetched buffer always covers `take` — but a malformed
            // window must degrade to a failed fill, never a panic that
            // skips the fill's cleanup.
            let Some(sliced) = data.get(skip as usize..take as usize) else {
                tracing::error!(
                    offset,
                    chunk = %hex::encode(&meta.digest),
                    "chunk bytes are shorter than the window's claimed slice"
                );
                return Err(Errno::EIO);
            };
            hasher.update(sliced);
            state.partial.write_all_at(sliced, offset).map_err(|e| {
                tracing::warn!(offset, error = %e, "staging write failed");
                Errno::EIO
            })?;
            offset += sliced.len() as u64;
            state.advance(offset);
        }

        // Stage + promote the misses so the next build on this node
        // reads them from local SSD. Awaited so at most one batch is in
        // flight at mountd, but failure is non-fatal: assembly proceeds
        // from this build's own staging and never reads the promoted
        // copies back.
        if !misses.is_empty() {
            let digests: Vec<[u8; 32]> = misses
                .iter()
                .filter_map(|m| m.digest.as_slice().try_into().ok())
                .collect();
            stage_chunks(&ctx.staging_dir, &misses, &fetched);
            let mountd = ctx.mountd.clone();
            let timeout = ctx.mountd_request_timeout;
            let sent =
                tokio::task::spawn_blocking(move || mountd.promote_chunks(digests, timeout)).await;
            match sent {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::debug!(error = %e, "PromoteChunks failed; chunks stay build-local");
                }
                Err(join) => {
                    tracing::warn!(error = %join, "PromoteChunks task panicked");
                }
            }
        }
    }
    Ok(())
}

/// Read one chunk from the node-shared chunk cache. `None` on any
/// failure — a miss, a torn read against a concurrent LRU unlink, or a
/// length mismatch — sends the chunk to the remote path instead.
/// Content verification is mountd's job (`PromoteChunks` re-hashes
/// before the entry becomes visible, and the directory is read-only to
/// builders); the whole-file digest at fill-complete is the final gate.
/// `O_NOFOLLOW` is defense-in-depth: the chunk-cache parents are
/// root-owned 0755, so only root could plant a symlink here — a leaf
/// entry is never legitimately one, and refusing it falls back to the
/// remote path like any other miss.
fn read_local_chunk(chunks_dir: &Path, meta: &ChunkMeta) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let hex = hex::encode(&meta.digest);
    let path = chunks_dir.join(&hex[..2]).join(&hex);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(&path)
        .ok()?;
    let mut data = Vec::new();
    file.read_to_end(&mut data).ok()?;
    (data.len() as u64 == meta.size).then_some(data)
}

/// Write each fetched chunk (whole, not sliced — chunks are
/// content-addressed) into `staging/chunks/<hex>` where mountd's
/// `PromoteChunks` expects to find it. Mode 0600 like the `.partial`:
/// the staging dir holds one tenant's build inputs and only mountd
/// (root) needs to read them back. Best-effort: a failed staging write
/// only costs the cross-build dedup for that chunk.
///
/// `create_new` like the `.partial` staging path: the staging dir is
/// per-build 0700 and only this process writes it, so an existing
/// entry means the same chunk was staged twice — a bug worth surfacing
/// loudly, not silently truncating over.
fn stage_chunks(staging_dir: &Path, misses: &[&ChunkMeta], fetched: &HashMap<Vec<u8>, Vec<u8>>) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = staging_dir.join("chunks");
    for meta in misses {
        let Some(data) = fetched.get(&meta.digest) else {
            continue;
        };
        let path = dir.join(hex::encode(&meta.digest));
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut f| f.write_all(data));
        match written {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                tracing::warn!(path = %path.display(), "chunk staged twice; keeping existing entry");
            }
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "chunk staging write failed");
            }
        }
    }
}

/// One `GetChunks` bidi stream, request-sender half + response half.
struct RemoteChunks {
    tx: tokio::sync::mpsc::Sender<GetChunksRequest>,
    rx: tonic::Streaming<rio_proto::types::ChunkData>,
}

impl RemoteChunks {
    async fn open(
        mut client: ChunkServiceClient<Channel>,
        assignment_token: &str,
        timeout: Duration,
    ) -> Result<Self, Errno> {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let mut req = tonic::Request::new(stream);
        attach_token(&mut req, assignment_token)?;
        match tokio::time::timeout(timeout, client.get_chunks(req)).await {
            Err(_elapsed) => {
                tracing::warn!(?timeout, "GetChunks stream open timed out");
                Err(Errno::EIO)
            }
            Ok(Err(status)) => {
                tracing::warn!(%status, "GetChunks stream open failed");
                Err(Errno::EIO)
            }
            Ok(Ok(resp)) => Ok(Self {
                tx,
                rx: resp.into_inner(),
            }),
        }
    }

    /// Request `misses` in one frame and collect every reply. The
    /// server fans out and replies in completion order; each chunk is
    /// verified against its content address on arrival, before any
    /// byte of it can reach the `.partial` or the chunk staging dir.
    async fn fetch(
        &mut self,
        misses: &[&ChunkMeta],
        timeout: Duration,
    ) -> Result<HashMap<Vec<u8>, Vec<u8>>, Errno> {
        let want: HashMap<&[u8], u64> = misses
            .iter()
            .map(|m| (m.digest.as_slice(), m.size))
            .collect();
        let req = GetChunksRequest {
            digests: misses.iter().map(|m| m.digest.clone()).collect(),
        };
        if self.tx.send(req).await.is_err() {
            tracing::warn!("GetChunks request stream closed");
            return Err(Errno::EIO);
        }
        let mut out = HashMap::with_capacity(misses.len());
        let deadline = tokio::time::Instant::now() + timeout;
        while out.len() < misses.len() {
            let next = tokio::time::timeout_at(deadline, self.rx.message()).await;
            let chunk = match next {
                Err(_elapsed) => {
                    tracing::warn!(?timeout, "GetChunks batch timed out");
                    return Err(Errno::EIO);
                }
                Ok(Err(status)) => {
                    tracing::warn!(%status, "GetChunks stream failed");
                    return Err(Errno::EIO);
                }
                Ok(Ok(None)) => {
                    tracing::warn!("GetChunks stream ended before the batch completed");
                    return Err(Errno::EIO);
                }
                Ok(Ok(Some(chunk))) => chunk,
            };
            let Some(&size) = want.get(chunk.digest.as_slice()) else {
                tracing::warn!("GetChunks returned a chunk that was not requested");
                return Err(Errno::EIO);
            };
            // r[impl builder.fs.file-digest-integrity]
            if chunk.data.len() as u64 != size
                || blake3::hash(&chunk.data).as_bytes() != chunk.digest.as_slice()
            {
                metrics::counter!("rio_builder_castore_fuse_integrity_fail_total").increment(1);
                tracing::error!(
                    digest = %hex::encode(&chunk.digest),
                    "GetChunks content does not match its digest"
                );
                return Err(Errno::EIO);
            }
            out.insert(chunk.digest, chunk.data.into());
        }
        Ok(out)
    }
}

/// Whole-file `ReadBlob` fill for files whose manifest is inline (no
/// chunk list to stream from). Frames arrive in file order, so the
/// high-water mark advances per frame and the first frame opens the
/// `open()` reply gate exactly like the first chunk does.
async fn fill_from_readblob(
    ctx: &FillCtx,
    state: &FillState,
    hasher: &mut blake3::Hasher,
) -> Result<(), Errno> {
    let mut client = ctx.directory.clone();
    let mut req = tonic::Request::new(ReadBlobRequest {
        file_digest: ctx.digest.to_vec(),
    });
    attach_token(&mut req, &ctx.assignment_token)?;
    let timeout = jit_fetch_timeout(ctx.fetch_timeout, ctx.size);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut stream = match tokio::time::timeout_at(deadline, client.read_blob(req)).await {
        Err(_elapsed) => {
            tracing::warn!(digest = %hex::encode(ctx.digest), "ReadBlob timed out");
            return Err(Errno::EIO);
        }
        Ok(Err(status)) => {
            tracing::warn!(digest = %hex::encode(ctx.digest), %status, "ReadBlob failed");
            return Err(Errno::EIO);
        }
        Ok(Ok(resp)) => resp.into_inner(),
    };
    let mut offset: u64 = 0;
    loop {
        let frame = match tokio::time::timeout_at(deadline, stream.message()).await {
            Err(_elapsed) => {
                tracing::warn!(digest = %hex::encode(ctx.digest), "ReadBlob timed out");
                return Err(Errno::EIO);
            }
            Ok(Err(status)) => {
                tracing::warn!(digest = %hex::encode(ctx.digest), %status, "ReadBlob failed");
                return Err(Errno::EIO);
            }
            Ok(Ok(None)) => break,
            Ok(Ok(Some(frame))) => frame,
        };
        if offset + frame.data.len() as u64 > ctx.size {
            tracing::warn!(digest = %hex::encode(ctx.digest), "ReadBlob overran the file size");
            return Err(Errno::EIO);
        }
        hasher.update(&frame.data);
        state
            .partial
            .write_all_at(&frame.data, offset)
            .map_err(|e| {
                tracing::warn!(offset, error = %e, "staging write failed");
                Errno::EIO
            })?;
        offset += frame.data.len() as u64;
        state.advance(offset);
    }
    if offset != ctx.size {
        tracing::warn!(
            digest = %hex::encode(ctx.digest),
            got = offset,
            want = ctx.size,
            "ReadBlob stream ended short"
        );
        return Err(Errno::EIO);
    }
    metrics::counter!("rio_builder_castore_fuse_fetch_bytes_total", "hit" => "remote")
        .increment(offset);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::castore_fuse::open::create_partial;
    use crate::castore_fuse::testing::{FakeDirectory, RecordingMountd};
    use rio_proto::DirectoryServiceServer;
    use rio_proto::types::StatBlobResponse;
    use rio_test_support::grpc::{MockStore, spawn_grpc_server};

    /// Deterministic non-uniform chunk content so a swapped or
    /// misplaced chunk changes the assembled bytes.
    fn chunk_bytes(seed: u8, len: usize) -> Vec<u8> {
        (0..len).map(|i| seed ^ (i as u8)).collect()
    }

    /// Build a chunk window over `pieces`, sliced by `skip`/`take`, and
    /// return it with the file bytes it reassembles to.
    fn window_for(pieces: &[Vec<u8>], skip: u32, take: u32) -> (StatBlobResponse, Vec<u8>) {
        let n = pieces.len();
        let chunks = pieces
            .iter()
            .map(|p| ChunkMeta {
                digest: blake3::hash(p).as_bytes().to_vec(),
                size: p.len() as u64,
            })
            .collect();
        let mut file = Vec::new();
        for (i, p) in pieces.iter().enumerate() {
            let start = if i == 0 { skip as usize } else { 0 };
            let end = if i == n - 1 { take as usize } else { p.len() };
            file.extend_from_slice(&p[start..end]);
        }
        (
            StatBlobResponse {
                chunks,
                first_chunk_skip: skip,
                last_chunk_take: take,
            },
            file,
        )
    }

    /// One assembled test fixture: tempdir layout + clients + the
    /// FillCtx pointing at them.
    struct Harness {
        tmp: tempfile::TempDir,
        mountd: RecordingMountd,
        store: MockStore,
        ctx: FillCtx,
    }

    impl Harness {
        async fn new(
            digest: [u8; 32],
            size: u64,
            stat: std::result::Result<StatBlobResponse, tonic::Code>,
            blob: Vec<u8>,
        ) -> Self {
            Self::with_directory(digest, size, FakeDirectory::new(stat, blob)).await
        }

        async fn with_directory(digest: [u8; 32], size: u64, directory: FakeDirectory) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            for d in ["chunks", "staging", "staging/chunks"] {
                std::fs::create_dir_all(tmp.path().join(d)).unwrap();
            }
            let (mountd, mountd_client) = RecordingMountd::spawn(&tmp.path().join("mountd.sock"));
            let store = MockStore::new();
            let router = tonic::transport::Server::builder()
                .add_service(rio_proto::ChunkServiceServer::new(store.clone()))
                .add_service(DirectoryServiceServer::new(directory));
            let (addr, _handle) = spawn_grpc_server(router).await;
            let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
                .unwrap()
                .connect_lazy();
            let ctx = FillCtx {
                digest,
                size,
                directory: DirectoryServiceClient::new(channel.clone()),
                chunk: ChunkServiceClient::new(channel),
                assignment_token: "test-token".to_owned(),
                mountd: mountd_client,
                circuit: Arc::new(CircuitBreaker::default()),
                chunks_dir: tmp.path().join("chunks"),
                staging_dir: tmp.path().join("staging"),
                fetch_timeout: Duration::from_secs(5),
                mountd_request_timeout: Duration::from_secs(5),
            };
            Self {
                tmp,
                mountd,
                store,
                ctx,
            }
        }

        fn new_fill_state(&self) -> Arc<FillState> {
            let partial = self
                .ctx
                .staging_dir
                .join(format!("{}.partial", hex::encode(self.ctx.digest)));
            Arc::new(FillState::new(
                create_partial(&partial).unwrap(),
                self.ctx.size,
            ))
        }

        /// Seed a chunk into the node-shared chunk cache (as mountd's
        /// `PromoteChunks` would have left it).
        fn seed_local_chunk(&self, data: &[u8]) {
            let hex = hex::encode(blake3::hash(data).as_bytes());
            let dir = self.tmp.path().join("chunks").join(&hex[..2]);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(&hex), data).unwrap();
        }

        /// Seed a chunk into the remote store only.
        fn seed_remote_chunk(&self, data: &[u8]) {
            self.store
                .state
                .chunks
                .write()
                .unwrap()
                .insert(blake3::hash(data).as_bytes().to_vec(), data.to_vec());
        }
    }

    // ── FillState: the open()-reply and read() blocking contract ──────

    /// `wait_first_chunk` returns as soon as the first chunk lands —
    /// not when the fill completes — and `read_at` past the high-water
    /// mark blocks until the fill catches up. This is the §2.8 promise
    /// that lets `open()` of a multi-GiB file reply in one chunk RTT.
    // r[verify builder.fs.streaming-open]
    #[test]
    fn first_chunk_unblocks_open_before_fill_completes() {
        let tmp = tempfile::tempdir().unwrap();
        let partial = create_partial(&tmp.path().join("x.partial")).unwrap();
        let state = Arc::new(FillState::new(partial, 1024));

        let opener = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || state.wait_first_chunk())
        };
        let tail_reader = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || state.read_at(1000, 24))
        };

        state.partial.write_all_at(&[0xAA; 256], 0).unwrap();
        state.advance(256);
        assert!(
            opener.join().unwrap().is_ok(),
            "open() unblocks at the first chunk"
        );
        assert!(
            !tail_reader.is_finished(),
            "a read past the high-water mark stays blocked"
        );

        state.partial.write_all_at(&[0xBB; 768], 256).unwrap();
        state.advance(1024);
        state.finish(Ok(()));
        assert_eq!(
            tail_reader.join().unwrap().unwrap(),
            vec![0xBB; 24],
            "the blocked read is served once the fill covers its range"
        );
    }

    /// A failed fill must wake every blocked reader with an error
    /// instead of leaving them parked — the §2.7 fail-fast promise.
    #[test]
    fn fill_failure_wakes_blocked_readers_with_eio() {
        let tmp = tempfile::tempdir().unwrap();
        let partial = create_partial(&tmp.path().join("x.partial")).unwrap();
        let state = Arc::new(FillState::new(partial, 1024));

        let reader = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || state.read_at(512, 16))
        };
        let opener = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || state.wait_first_chunk())
        };
        state.finish(Err(Errno::EIO));
        assert_eq!(
            reader.join().unwrap().unwrap_err().code(),
            Errno::EIO.code()
        );
        assert_eq!(
            opener.join().unwrap().unwrap_err().code(),
            Errno::EIO.code()
        );
    }

    /// Reads at and past EOF return short/empty without blocking; the
    /// kernel always reads in page-sized units regardless of file size.
    #[test]
    fn read_at_clamps_to_eof() {
        let tmp = tempfile::tempdir().unwrap();
        let partial = create_partial(&tmp.path().join("x.partial")).unwrap();
        let state = FillState::new(partial, 100);
        state.partial.write_all_at(&[7u8; 100], 0).unwrap();
        state.advance(100);
        assert_eq!(state.read_at(96, 4096).unwrap(), vec![7u8; 4]);
        assert!(state.read_at(100, 4096).unwrap().is_empty());
        assert!(state.read_at(4096, 4096).unwrap().is_empty());
    }

    /// A stale `.partial` from a crashed predecessor is replaced, not
    /// reused — reusing the inode would leave stale bytes past the new
    /// fill's write cursor.
    #[test]
    fn create_partial_replaces_a_stale_orphan() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("orphan.partial");
        std::fs::write(&path, b"stale bytes from a crashed fill").unwrap();
        let f = create_partial(&path).unwrap();
        assert_eq!(
            f.metadata().unwrap().len(),
            0,
            "the orphan is unlinked and recreated empty"
        );
    }

    // ── Window validation ─────────────────────────────────────────────

    #[test]
    fn validate_window_rejects_malformed_responses() {
        let piece = chunk_bytes(1, 100);
        let (good, file) = window_for(std::slice::from_ref(&piece), 10, 90);
        assert_eq!(file.len(), 80);
        assert!(validate_window(&good, 80).is_ok());

        // Sliced lengths don't sum to the file size.
        assert!(validate_window(&good, 81).is_err());
        // skip > take on a single-chunk window.
        let mut bad = good.clone();
        bad.first_chunk_skip = 95;
        assert!(validate_window(&bad, 0).is_err());
        // take past the end of the last chunk.
        let mut bad = good.clone();
        bad.last_chunk_take = 101;
        assert!(validate_window(&bad, 91).is_err());
        // Empty chunk list for a non-empty file.
        assert!(validate_window(&StatBlobResponse::default(), 1).is_err());
        // Truncated digest.
        let mut bad = good.clone();
        bad.chunks[0].digest.truncate(8);
        assert!(validate_window(&bad, 80).is_err());
        // Oversized chunk.
        let mut bad = good;
        bad.chunks[0].size = MAX_CHUNK_BYTES + 1;
        assert!(validate_window(&bad, 80).is_err());
    }

    /// Content-defined chunking legitimately repeats a digest within one
    /// window — but every occurrence must agree on the chunk's size. The
    /// batch fetch dedups by digest and length-checks the bytes against
    /// the FIRST occurrence only, so a later occurrence claiming a larger
    /// size would slice past the fetched buffer.
    #[test]
    fn validate_window_rejects_inconsistent_duplicate_chunk_sizes() {
        let a = chunk_bytes(3, 1000);
        let digest = blake3::hash(&a).as_bytes().to_vec();
        let window = StatBlobResponse {
            chunks: vec![
                ChunkMeta {
                    digest: digest.clone(),
                    size: 1000,
                },
                ChunkMeta { digest, size: 1500 },
            ],
            first_chunk_skip: 0,
            last_chunk_take: 1500,
        };
        // Sliced sums match the file size, so only the digest→size
        // consistency check can reject this window.
        assert!(
            validate_window(&window, 2500).is_err(),
            "a repeated chunk digest with two different sizes must be rejected up front"
        );
    }

    // ── End-to-end fill ───────────────────────────────────────────────

    /// The full streaming fill: a window whose first/last chunks are
    /// sliced (the legacy whole-NAR-chunked layout), some chunks
    /// already in the node chunk cache and the rest only in the store.
    /// Local chunks MUST be sourced locally (they are absent from the
    /// store, so a remote fetch would fail the fill); remote chunks
    /// MUST be staged and `PromoteChunks`d for the next build.
    // r[verify builder.fs.node-chunk-cache]
    // r[verify builder.fs.streaming-open]
    #[tokio::test(flavor = "multi_thread")]
    async fn streaming_fill_assembles_verifies_and_promotes() {
        let pieces: Vec<Vec<u8>> = (0..5u8)
            .map(|i| chunk_bytes(i, 1000 + i as usize))
            .collect();
        let (window, file) = window_for(&pieces, 100, 900);
        let digest = *blake3::hash(&file).as_bytes();
        let h = Harness::new(digest, file.len() as u64, Ok(window), Vec::new()).await;
        // Chunks 0 and 3 are already on the node; 1, 2, 4 are remote.
        h.seed_local_chunk(&pieces[0]);
        h.seed_local_chunk(&pieces[3]);
        for p in [&pieces[1], &pieces[2], &pieces[4]] {
            h.seed_remote_chunk(p);
        }

        let state = h.new_fill_state();
        let active = Arc::new(Mutex::new(HashMap::from([(digest, Arc::clone(&state))])));
        run_fill(h.ctx, state.clone(), Arc::clone(&active)).await;

        assert_eq!(
            state.read_at(0, file.len() as u32).unwrap(),
            file,
            "the assembled bytes are the sliced concatenation of the window"
        );
        let staged = h.tmp.path().join("staging").join(hex::encode(digest));
        assert_eq!(
            std::fs::read(&staged).unwrap(),
            file,
            ".partial is renamed to the bare digest for Promote"
        );
        assert!(
            active.lock().unwrap().is_empty(),
            "the fill removes itself from the active map"
        );

        // Cross-build dedup: exactly the remote chunks are staged and
        // promoted, whole (not sliced) because they are content-addressed.
        let mut promoted = h.mountd.promoted_chunks();
        promoted.sort_unstable();
        let mut want: Vec<[u8; 32]> = [&pieces[1], &pieces[2], &pieces[4]]
            .iter()
            .map(|p| *blake3::hash(p).as_bytes())
            .collect();
        want.sort_unstable();
        assert_eq!(promoted, want);
        for p in [&pieces[1], &pieces[2], &pieces[4]] {
            let staged_chunk = h
                .tmp
                .path()
                .join("staging/chunks")
                .join(hex::encode(blake3::hash(p).as_bytes()));
            assert_eq!(
                std::fs::read(&staged_chunk).unwrap().as_slice(),
                p.as_slice()
            );
        }
        assert!(
            h.mountd.saw_promote(&digest),
            "the completed file is promoted into the node cache"
        );
    }

    /// Content-defined chunking of repetitive data legitimately yields
    /// the same chunk digest more than once in one file's window. The
    /// batch fetch must request each distinct digest once and reuse the
    /// bytes for every occurrence — counting replies against the
    /// duplicated occurrence list would wait for a reply that never
    /// comes.
    #[tokio::test(flavor = "multi_thread")]
    async fn duplicate_chunks_within_a_batch_assemble_correctly() {
        let a = chunk_bytes(1, 700);
        let pieces = vec![chunk_bytes(0, 600), a.clone(), a.clone()];
        let (window, file) = window_for(&pieces, 0, 700);
        let digest = *blake3::hash(&file).as_bytes();
        let h = Harness::new(digest, file.len() as u64, Ok(window), Vec::new()).await;
        for p in &pieces {
            h.seed_remote_chunk(p);
        }
        let state = h.new_fill_state();
        let active = Arc::new(Mutex::new(HashMap::from([(digest, Arc::clone(&state))])));
        run_fill(h.ctx, state.clone(), active).await;
        assert_eq!(state.read_at(0, file.len() as u32).unwrap(), file);
        assert_eq!(
            h.mountd
                .promoted_chunks()
                .iter()
                .filter(|d| *d == blake3::hash(&a).as_bytes())
                .count(),
            1,
            "the repeated chunk is promoted once, not once per occurrence"
        );
    }

    /// A remote chunk whose bytes do not hash to the digest the window
    /// claims must fail the fill before any of its bytes reach the
    /// `.partial` — and must not be staged for promotion.
    // r[verify builder.fs.file-digest-integrity]
    #[tokio::test(flavor = "multi_thread")]
    async fn corrupt_remote_chunk_fails_the_fill() {
        let pieces: Vec<Vec<u8>> = (0..2u8).map(|i| chunk_bytes(i, 500)).collect();
        let (window, file) = window_for(&pieces, 0, 500);
        let digest = *blake3::hash(&file).as_bytes();
        let h = Harness::new(digest, file.len() as u64, Ok(window), Vec::new()).await;
        h.seed_remote_chunk(&pieces[0]);
        // Chunk 1 is served under its correct digest key but with
        // corrupted bytes.
        h.store.state.chunks.write().unwrap().insert(
            blake3::hash(&pieces[1]).as_bytes().to_vec(),
            b"corrupted".to_vec(),
        );

        let state = h.new_fill_state();
        let active = Arc::new(Mutex::new(HashMap::from([(digest, Arc::clone(&state))])));
        run_fill(h.ctx, state.clone(), active).await;

        assert_eq!(
            state.read_at(900, 100).unwrap_err().code(),
            Errno::EIO.code()
        );
        // Chunk 0 arrived verified in an earlier batch and is
        // legitimately promoted; the corrupt chunk must be neither
        // staged nor promoted.
        let corrupt = *blake3::hash(&pieces[1]).as_bytes();
        assert!(
            !h.mountd.promoted_chunks().contains(&corrupt),
            "a corrupt chunk is not promoted"
        );
        assert!(
            !h.tmp
                .path()
                .join("staging/chunks")
                .join(hex::encode(corrupt))
                .exists(),
            "a corrupt chunk is not staged"
        );
    }

    /// `StatBlob` answering FailedPrecondition (inline manifest, no
    /// chunk list) falls back to a whole-file `ReadBlob` fill with the
    /// same progressive high-water semantics.
    #[tokio::test(flavor = "multi_thread")]
    async fn inline_manifest_falls_back_to_readblob() {
        let file = chunk_bytes(9, 4096);
        let digest = *blake3::hash(&file).as_bytes();
        let h = Harness::new(
            digest,
            file.len() as u64,
            Err(tonic::Code::FailedPrecondition),
            file.clone(),
        )
        .await;
        let state = h.new_fill_state();
        let active = Arc::new(Mutex::new(HashMap::from([(digest, Arc::clone(&state))])));
        run_fill(h.ctx, state.clone(), active).await;
        assert_eq!(state.read_at(0, 4096).unwrap(), file);
    }

    /// The ReadBlob fallback is subject to the same whole-file digest
    /// gate as the chunked path: a stream that delivers the wrong bytes
    /// (truncated upstream object, wrong row, bitrot) must not be
    /// renamed, promoted, or served.
    // r[verify builder.fs.file-digest-integrity]
    #[tokio::test(flavor = "multi_thread")]
    async fn corrupt_readblob_stream_fails_the_fill() {
        let file = chunk_bytes(9, 4096);
        let digest = *blake3::hash(b"not what the stream delivers").as_bytes();
        let h = Harness::new(
            digest,
            file.len() as u64,
            Err(tonic::Code::FailedPrecondition),
            file,
        )
        .await;
        let state = h.new_fill_state();
        let active = Arc::new(Mutex::new(HashMap::from([(digest, Arc::clone(&state))])));
        run_fill(h.ctx, state.clone(), active).await;
        assert_eq!(state.read_at(0, 16).unwrap_err().code(), Errno::EIO.code());
        assert!(
            !h.mountd.saw_promote(&digest),
            "a digest-mismatched fill is not promoted"
        );
    }

    /// A whole-file digest mismatch (every chunk individually valid but
    /// the window assembles to something else) is caught at
    /// fill-complete: the `.partial` is discarded, nothing is renamed
    /// or promoted, and readers get EIO.
    // r[verify builder.fs.file-digest-integrity]
    #[tokio::test(flavor = "multi_thread")]
    async fn whole_file_digest_mismatch_discards_the_fill() {
        let pieces: Vec<Vec<u8>> = (0..2u8).map(|i| chunk_bytes(i, 500)).collect();
        let (window, file) = window_for(&pieces, 0, 500);
        // The window is internally consistent but the claimed digest is
        // for different content.
        let digest = *blake3::hash(b"something else").as_bytes();
        let h = Harness::new(digest, file.len() as u64, Ok(window), Vec::new()).await;
        for p in &pieces {
            h.seed_remote_chunk(p);
        }
        let state = h.new_fill_state();
        let active = Arc::new(Mutex::new(HashMap::from([(digest, Arc::clone(&state))])));
        run_fill(h.ctx, state.clone(), active).await;

        assert_eq!(state.read_at(0, 16).unwrap_err().code(), Errno::EIO.code());
        let staging = h.tmp.path().join("staging");
        assert!(
            !staging.join(hex::encode(digest)).exists()
                && !staging
                    .join(format!("{}.partial", hex::encode(digest)))
                    .exists(),
            "neither the .partial nor a renamed copy survives a digest mismatch"
        );
    }

    /// A window that repeats a chunk digest with a LARGER claimed size
    /// for the later occurrence must fail the fill cleanly: the batch
    /// fetch verifies the bytes against the first occurrence only, so
    /// honoring the second occurrence's size would slice past the
    /// fetched buffer (a panic inside the fill task, which also skipped
    /// the active_fills cleanup and wedged the digest to EIO).
    #[tokio::test(flavor = "multi_thread")]
    async fn inconsistent_duplicate_chunk_sizes_fail_the_fill_cleanly() {
        let c0 = chunk_bytes(0, 600);
        let a = chunk_bytes(1, 1000);
        let window = StatBlobResponse {
            chunks: vec![
                ChunkMeta {
                    digest: blake3::hash(&c0).as_bytes().to_vec(),
                    size: 600,
                },
                ChunkMeta {
                    digest: blake3::hash(&a).as_bytes().to_vec(),
                    size: 1000,
                },
                // Same digest as the previous chunk, inflated size.
                ChunkMeta {
                    digest: blake3::hash(&a).as_bytes().to_vec(),
                    size: 1500,
                },
            ],
            first_chunk_skip: 0,
            last_chunk_take: 1500,
        };
        let digest = *blake3::hash(b"never assembled - the window is rejected").as_bytes();
        let h = Harness::new(digest, 3100, Ok(window), Vec::new()).await;
        h.seed_remote_chunk(&c0);
        h.seed_remote_chunk(&a);

        let state = h.new_fill_state();
        let active = Arc::new(Mutex::new(HashMap::from([(digest, Arc::clone(&state))])));
        run_fill(h.ctx, state.clone(), Arc::clone(&active)).await;

        assert_eq!(
            state.read_at(0, 16).unwrap_err().code(),
            Errno::EIO.code(),
            "readers get EIO instead of a panic-wedged wait"
        );
        assert!(
            active.lock().unwrap().is_empty(),
            "the failed fill must not stay in active_fills (later opens would join a dead fill)"
        );
        assert!(
            !h.mountd.saw_promote(&digest),
            "nothing from a rejected window is promoted"
        );
    }

    /// A fill that never reaches its own cleanup — a panic, or (as
    /// simulated here) runtime cancellation at an await point — must
    /// still release everything it holds on behalf of the build: wake
    /// blocked readers with EIO, report the outcome to the circuit
    /// breaker (a claimed half-open probe would otherwise stay claimed
    /// forever), and remove itself from `active_fills` so a later
    /// open() retries instead of joining a dead fill.
    #[tokio::test(flavor = "multi_thread")]
    async fn abandoned_fill_releases_active_entry_circuit_and_readers() {
        use crate::castore_fuse::circuit::SystemClock;

        let file = chunk_bytes(7, 4096);
        let digest = *blake3::hash(&file).as_bytes();
        // ReadBlob yields its frames and then hangs, parking the fill at
        // an await point until it is cancelled.
        let mut directory = FakeDirectory::new(Err(tonic::Code::FailedPrecondition), file.clone());
        directory.hang_at_end = true;
        let mut h = Harness::with_directory(digest, file.len() as u64, directory).await;
        // A long fetch budget so the fill cannot time out on its own and
        // reach the normal cleanup path — only the guard can clean up.
        h.ctx.fetch_timeout = Duration::from_secs(120);
        // Threshold-1 breaker: a single recorded failure is observable.
        let circuit = Arc::new(CircuitBreaker::with_clock(
            1,
            Duration::from_secs(30),
            Duration::from_secs(720),
            SystemClock,
        ));
        h.ctx.circuit = Arc::clone(&circuit);

        let state = h.new_fill_state();
        let active = Arc::new(Mutex::new(HashMap::from([(digest, Arc::clone(&state))])));
        let task = tokio::spawn(run_fill(h.ctx, Arc::clone(&state), Arc::clone(&active)));

        // Wait for the fill to demonstrably start, then cancel it.
        let started = Arc::clone(&state);
        tokio::task::spawn_blocking(move || started.wait_first_chunk())
            .await
            .unwrap()
            .unwrap();
        task.abort();
        assert!(task.await.is_err(), "the parked fill was cancelled");

        assert!(
            active.lock().unwrap().is_empty(),
            "a cancelled fill must remove itself from active_fills"
        );
        assert_eq!(
            state.read_at(0, 16).unwrap_err().code(),
            Errno::EIO.code(),
            "readers of a cancelled fill are woken with EIO"
        );
        assert!(
            circuit.is_open(),
            "the abandoned fill must report a failure so a claimed half-open probe is released"
        );
    }

    /// `stage_chunks` exclusively creates each entry: a colliding name
    /// keeps the existing bytes instead of truncating over them, and
    /// the chunks that don't collide are still staged.
    #[test]
    fn stage_chunks_does_not_overwrite_existing_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("chunks");
        std::fs::create_dir_all(&dir).unwrap();

        let existing = chunk_bytes(1, 64);
        let fresh = chunk_bytes(2, 64);
        let metas: Vec<ChunkMeta> = [&existing, &fresh]
            .iter()
            .map(|d| ChunkMeta {
                digest: blake3::hash(d).as_bytes().to_vec(),
                size: d.len() as u64,
            })
            .collect();
        let fetched: HashMap<Vec<u8>, Vec<u8>> = [
            (metas[0].digest.clone(), existing.clone()),
            (metas[1].digest.clone(), fresh.clone()),
        ]
        .into();
        // Pre-stage the first chunk with different bytes so an
        // overwrite (or truncate) is detectable.
        let pre_staged = vec![0xEE; 16];
        std::fs::write(dir.join(hex::encode(&metas[0].digest)), &pre_staged).unwrap();

        stage_chunks(tmp.path(), &metas.iter().collect::<Vec<_>>(), &fetched);

        assert_eq!(
            std::fs::read(dir.join(hex::encode(&metas[0].digest))).unwrap(),
            pre_staged,
            "an existing staged entry must not be overwritten or truncated"
        );
        assert_eq!(
            std::fs::read(dir.join(hex::encode(&metas[1].digest))).unwrap(),
            fresh,
            "a non-colliding chunk is still staged"
        );
    }
}
