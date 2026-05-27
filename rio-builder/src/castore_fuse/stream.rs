//! P0575 streaming `open()` — the during-fill mode for large files
//! (ADR-022 §2.8 mitigation i).
//!
//! For a cache miss on a file larger than `stream_threshold`, `open()`
//! cannot reply passthrough (no complete backing file exists yet) and
//! must not block for the whole transfer (a multi-GiB input would stall
//! `open(2)` for minutes). Instead it spawns a background **fill** and
//! replies `FOPEN_KEEP_CACHE` as soon as the first chunk has landed:
//!
//! - The fill resolves the file's chunk window via
//!   `StatBlob(file_digest, send_chunks=true)`, then walks the chunks
//!   in order: each is sourced from the mountd-owned node chunk cache
//!   (`/var/rio/chunks/{ab}/{hex}`) when present, otherwise fetched
//!   over one pipelined `GetChunks` stream, blake3-verified, and also
//!   staged + batched into `PromoteChunks` so *other* builds on this
//!   node can hit it locally.
//! - Each chunk's contribution (the first/last chunks of a window may
//!   begin/end mid-chunk) is appended to the staging `.partial` file
//!   and the shared high-water mark advances; `read()`s inside the
//!   verified prefix are served from the `.partial`, reads beyond it
//!   block (bounded) until their range arrives or the fill fails.
//! - On completion the whole-file blake3 is verified and the `.partial`
//!   is renamed to the bare digest name; readers are unblocked at that
//!   point, and the file is then `Promote`d (best-effort) into the
//!   shared backing cache so the next `open()` of this digest is a
//!   passthrough cache hit.
//!
//! The progress tracker is std::sync (`Mutex` + `Condvar`), matching
//! the rest of the FUSE callback path; only the fill thread itself
//! touches tokio (via `Handle::block_on`, the established
//! fuser-thread bridging pattern).
// r[impl builder.fs.streaming-open]

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use fuser::Errno;
use tokio::runtime::Handle;
use tokio_stream::wrappers::ReceiverStream;

use rio_proto::types::{ChunkData, GetChunksRequest, StatBlobRequest, StatBlobResponse};

use super::circuit::CircuitBreaker;
use super::fs::read_at_full;
use super::mountd_client::MountdClient;
use crate::IgnorePoison;
use crate::store_fetch::{ScopePresenter, StoreClients};

/// How many chunks ahead of the assembly cursor the fill probes the
/// node chunk cache and requests remote misses. Bounds the
/// out-of-order receive buffer at `FETCH_LOOKAHEAD × CHUNK_MAX`
/// (≈ 4 MiB) while keeping the store's per-frame fan-out busy.
const FETCH_LOOKAHEAD: usize = 16;

/// Remote-fetched chunk digests per `PromoteChunks` batch ("every 32
/// chunks or at EOF"). Half the daemon's `PROMOTE_CHUNKS_MAX` so a
/// batch never trips `BatchTooLarge`.
const PROMOTE_BATCH: usize = 32;

/// Upper bound on a single chunk's claimed size in a `StatBlob` window:
/// the store's FastCDC parameters cap chunks at 4 MiB. A window claiming
/// more (or a zero-size chunk) is malformed — rejecting it up front
/// bounds what one `GetChunks` reply can make the fill buffer or write,
/// instead of trusting the wire value.
const CHUNK_SIZE_MAX: u64 = 4 * 1024 * 1024;

/// Shared state of one in-flight streaming fill: the staging
/// `.partial` being assembled, the verified-prefix high-water mark,
/// and the fill's terminal result.
///
/// One per `file_digest`, shared by every `open()` of that digest in
/// this process (winner and attachers alike) and by the fill thread.
/// FUSE `read()` callbacks consult it via `StreamFill::read_at`;
/// they never talk to the fill thread directly.
pub struct StreamFill {
    /// The staging `.partial`: written sequentially by the fill thread
    /// (`write_at`), `pread` by every streaming `read()`. Holding the
    /// flock here preserves the orphan-reclaim contract: a held lock
    /// means a live fill owns the file, an unheld one is reclaimable.
    /// The fd stays valid for late readers even after the fill renames
    /// (success) or unlinks (failure) the path.
    partial: nix::fcntl::Flock<File>,
    /// True (final) file size — the EOF boundary for reads. The disk
    /// file is shorter than this until the fill completes.
    size: u64,
    /// Upper bound on any single blocked wait (a `read()` past the
    /// high-water mark, or `open()`'s first-chunk barrier). Sized to
    /// the fill's own size-aware budget plus one mountd round-trip of
    /// slack, so a healthy fill always finishes (or fails and wakes
    /// everyone) before a waiter gives up.
    wait_budget: Duration,
    progress: Mutex<Progress>,
    cv: Condvar,
}

/// The condvar-guarded fill progress.
struct Progress {
    /// Verified, written, contiguous prefix length in bytes.
    high_water: u64,
    /// `Some` once the fill finished. `Err` means no further bytes are
    /// coming and every waiter (and every later read on this handle)
    /// gets `EIO` — the §2.7/§13 fail-fast promise.
    result: Option<Result<(), Errno>>,
}

impl StreamFill {
    fn new(partial: nix::fcntl::Flock<File>, size: u64, wait_budget: Duration) -> Self {
        Self {
            partial,
            size,
            wait_budget,
            progress: Mutex::new(Progress {
                high_water: 0,
                result: None,
            }),
            cv: Condvar::new(),
        }
    }

    /// Publish a new contiguous prefix length and wake every waiter.
    fn set_high_water(&self, bytes: u64) {
        self.progress.lock().ignore_poison().high_water = bytes;
        self.cv.notify_all();
    }

    /// Publish the fill's terminal result and wake every waiter. Called
    /// exactly once, on every fill exit path (success, error, panic
    /// guard) — a fill that vanished without finishing would park every
    /// blocked reader until its wait budget.
    fn finish(&self, result: Result<(), Errno>) {
        self.progress.lock().ignore_poison().result = Some(result);
        self.cv.notify_all();
    }

    /// Block until the verified prefix covers `end` bytes, the fill
    /// finishes, or `deadline` expires.
    ///
    /// A failed fill returns its error even for ranges the prefix
    /// already covers: once the fill is dead this build is failing
    /// anyway, and serving more bytes from a half-fetched file only
    /// delays the loud failure.
    fn wait_covering(&self, end: u64, deadline: Duration) -> Result<(), Errno> {
        let give_up = Instant::now() + deadline;
        let mut progress = self.progress.lock().ignore_poison();
        loop {
            match progress.result {
                Some(Err(errno)) => return Err(errno),
                // A finished fill covers the whole file.
                Some(Ok(())) => return Ok(()),
                None if progress.high_water >= end => return Ok(()),
                None => {}
            }
            let remaining = give_up.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Errno::EIO);
            }
            let (guard, _timeout) = self.cv.wait_timeout(progress, remaining).ignore_poison();
            progress = guard;
        }
    }

    /// Wait until the first chunk has been verified and written (or the
    /// fill failed). This is `open()`'s return barrier: the kernel gets
    /// its reply as soon as byte 0 is servable, not when the file is
    /// complete.
    pub(super) fn wait_first_chunk(&self, deadline: Duration) -> Result<(), Errno> {
        self.wait_covering(1, deadline)
    }

    /// Serve one `read(offset, len)` from the partially-filled file.
    ///
    /// Ranges inside the verified prefix return immediately; ranges
    /// beyond it block until the fill catches up, bounded by the fill's
    /// own budget. Reads at or past the true file size return empty
    /// (EOF); reads spanning it are truncated to it.
    pub(super) fn read_at(&self, offset: u64, len: u32) -> Result<Vec<u8>, Errno> {
        let end = offset.saturating_add(u64::from(len)).min(self.size);
        if offset >= end {
            return Ok(Vec::new());
        }
        self.wait_covering(end, self.wait_budget)?;
        let want = usize::try_from(end - offset).map_err(|_| Errno::EIO)?;
        let mut buf = vec![0u8; want];
        match read_at_full(&self.partial, &mut buf, offset) {
            Ok(n) if n == want => Ok(buf),
            Ok(n) => {
                tracing::error!(
                    offset,
                    want,
                    got = n,
                    "castore-fuse: streaming .partial is shorter than its high-water mark"
                );
                Err(Errno::EIO)
            }
            Err(e) => {
                tracing::warn!(offset, want, error = %e, "castore-fuse: streaming read failed");
                Err(Errno::EIO)
            }
        }
    }
}

/// Everything the background fill thread needs, cloned out of
/// [`super::open::OpenPath`] so the thread owns its world and can
/// outlive both the `open()` that spawned it and any individual file
/// handle.
pub(super) struct FillContext {
    pub file_digest: [u8; 32],
    pub size: u64,
    /// `staging/{build_id}/{hex}.partial` — the assembly target.
    pub partial_path: PathBuf,
    /// `staging/{build_id}/{hex}` — the rename target `Promote` reads.
    pub staging_path: PathBuf,
    /// `staging/{build_id}/chunks/` — where remotely-fetched chunks are
    /// staged for `PromoteChunks`.
    pub staging_chunks_dir: PathBuf,
    /// `cache/{ab}/{hex}` — the shared backing-cache entry the final
    /// `Promote` publishes. Consulted on `RaceTimeout` to detect that a
    /// concurrent build's promote of the same digest already landed.
    pub cache_path: PathBuf,
    /// The mountd-owned node chunk cache root (`/var/rio/chunks`),
    /// read-only to the builder.
    pub chunks_dir: PathBuf,
    pub clients: StoreClients,
    pub runtime: Handle,
    pub mountd: MountdClient,
    pub circuit: Arc<CircuitBreaker>,
    pub mountd_timeout: Duration,
    /// Total wall-clock budget for the fill (StatBlob + every chunk +
    /// the final verify/rename), sized to the file like the whole-file
    /// path's JIT budget.
    pub budget: Duration,
    /// This build's HMAC assignment token, attached as
    /// `x-rio-assignment-token` to every store RPC the fill makes
    /// (`StatBlob`, `GetChunks`, the `ReadBlob` fallback) — the store
    /// derives the caller's tenant from it.
    pub assignment_token: String,
    /// The build's closure-scope presenter (ADR-022 P0591), shared with
    /// the mount-time prefetch and the whole-file path: the fill's
    /// scoped reads (`StatBlob`, the `ReadBlob` fallback) re-present and
    /// retry through it on `CASTORE_SCOPE_REQUIRED`. The chunk RPCs are
    /// digest-as-capability and never scoped.
    pub scope: Arc<ScopePresenter>,
    /// The in-flight fill registry this fill deregisters from when it
    /// finishes (so the next opener of a failed digest starts fresh).
    pub registry: Arc<Mutex<HashMap<[u8; 32], Arc<StreamFill>>>>,
}

/// Why a streaming fill failed. Every variant surfaces to the kernel as
/// `EIO`; the split decides which metric/log/circuit treatment applies.
/// Promote is deliberately NOT a variant: it runs after the verified
/// rename and after readers have been unblocked, so its failure never
/// fails the fill (see [`promote_streamed`]).
enum FillError {
    /// rio-store unreachable or a gRPC stream failed (counts against
    /// the fetch circuit breaker).
    Store(String),
    /// The store still demands a closure presentation
    /// (`CASTORE_SCOPE_REQUIRED`) after the bounded re-presents — an
    /// authorization-coordination failure, deliberately NOT counted
    /// against the fetch circuit breaker (ADR-022 P0591).
    ScopeRequired(String),
    /// Bytes did not match a content address (a chunk's blake3, the
    /// whole-file digest, or a malformed chunk window).
    Integrity(String),
    /// Local staging I/O failed.
    Io(String),
    /// The fill exceeded its wall-clock budget.
    Timeout,
}

/// Create the [`StreamFill`] for `ctx` (claiming the staging
/// `.partial`) and spawn its fill thread. Returns the shared handle the
/// caller registers and waits on.
///
/// The thread is detached: it keeps running after every file handle is
/// released so the fill's promote still benefits later opens. Its
/// lifetime is bounded by `ctx.budget` (every blocking step in the fill
/// body checks the deadline) plus the post-publish Promote round-trip.
pub(super) fn spawn_fill(ctx: FillContext) -> std::io::Result<Arc<StreamFill>> {
    let partial = super::open::create_partial(&ctx.partial_path)?;
    let fill = Arc::new(StreamFill::new(
        partial,
        ctx.size,
        ctx.budget + ctx.mountd_timeout,
    ));
    let thread_fill = Arc::clone(&fill);
    std::thread::Builder::new()
        .name("castore-stream-fill".into())
        .spawn(move || run_fill(&ctx, &thread_fill))?;
    Ok(fill)
}

/// The fill thread body: do the work, publish the outcome exactly once,
/// clean up, deregister.
fn run_fill(ctx: &FillContext, fill: &Arc<StreamFill>) {
    let _span = tracing::info_span!(
        "castore_stream_fill",
        digest = %hex::encode(ctx.file_digest),
        size = ctx.size,
    )
    .entered();
    let started = Instant::now();

    // Publish a result and deregister even if the fill body panics (the
    // realistic panic is `Handle::block_on` racing runtime shutdown at
    // process teardown): a fill that vanished silently would park every
    // blocked reader — and, if it stayed registered, every future
    // opener of this digest — until their wait budgets expire.
    let panic_guard = scopeguard::guard(
        (Arc::clone(fill), Arc::clone(&ctx.registry), ctx.file_digest),
        |(fill, registry, digest)| {
            registry.lock().ignore_poison().remove(&digest);
            fill.finish(Err(Errno::EIO));
        },
    );

    let outcome = fill_blob(ctx, fill, started + ctx.budget);

    let errno = match &outcome {
        Ok(()) => {
            tracing::info!(
                elapsed = ?started.elapsed(),
                "castore-fuse: streaming fill complete"
            );
            None
        }
        Err(FillError::Store(msg)) => {
            tracing::warn!(error = %msg, "castore-fuse: streaming fill failed fetching from rio-store");
            Some(Errno::EIO)
        }
        Err(FillError::ScopeRequired(msg)) => {
            tracing::warn!(
                error = %msg,
                "castore-fuse: streaming fill denied pending closure presentation \
                 (CASTORE_SCOPE_REQUIRED still unresolved after re-presenting)"
            );
            Some(Errno::EIO)
        }
        Err(FillError::Integrity(msg)) => {
            metrics::counter!("rio_builder_castore_fuse_integrity_fail_total").increment(1);
            tracing::error!(error = %msg, "castore-fuse: streaming fill integrity failure");
            Some(Errno::EIO)
        }
        Err(FillError::Io(msg)) => {
            tracing::error!(error = %msg, "castore-fuse: streaming fill staging I/O failed");
            Some(Errno::EIO)
        }
        Err(FillError::Timeout) => {
            tracing::warn!(
                budget = ?ctx.budget,
                "castore-fuse: streaming fill exceeded its budget"
            );
            Some(Errno::EIO)
        }
    };
    // The breaker watches store reachability: only store-side failures
    // (and timeouts, which on this path are dominated by the remote
    // transfer) count against it. ScopeRequired is excluded by design
    // (ADR-022 P0591): it is a coordination signal, not a health signal.
    match &outcome {
        Ok(()) => ctx.circuit.record(true),
        Err(FillError::Store(_) | FillError::Timeout) => ctx.circuit.record(false),
        Err(_) => {}
    }

    let (fill, registry, digest) = scopeguard::ScopeGuard::into_inner(panic_guard);
    match errno {
        Some(errno) => {
            // A failed fill leaves nothing behind: the next open of this
            // digest starts a fresh fill (or, if mountd promoted a
            // concurrent build's copy meanwhile, takes the hit path).
            let _ = std::fs::remove_file(&ctx.partial_path);
            // Ordering matters: the `.partial` is gone and the registry
            // entry removed BEFORE waiters observe the result, so an
            // opener that saw this fill fail and immediately retries
            // gets a fresh fill instead of attaching to (or colliding
            // with the staging file of) a dead one.
            registry.lock().ignore_poison().remove(&digest);
            fill.finish(Err(errno));
        }
        None => {
            // Success is publishable the moment the verified rename
            // landed: readers pread the renamed staging file through the
            // fd this StreamFill already holds. The Promote that follows
            // only matters for the NEXT open of this digest, so it must
            // neither delay nor poison reads on handles already open.
            // (The whole-file path mirrors this since the
            // promote-degrade change: an unpublishable Promote there
            // serves the open from the verified staged copy instead of
            // EIO — only daemon-side rejections of the bytes stay
            // fatal.)
            fill.finish(Ok(()));
            promote_streamed(ctx);
            // Deregister last: an open() racing the promote attaches to
            // this finished fill (and reads from staging) instead of
            // starting a redundant one.
            registry.lock().ignore_poison().remove(&digest);
        }
    }
}

/// Best-effort `Promote` of the renamed staging file into the shared
/// backing cache, after the fill's success has been published. Failures
/// are logged and counted, never fatal: every already-open handle keeps
/// reading from the staging fd; only future opens of this digest lose
/// the cache hit (they re-stream and re-attempt the promote).
///
/// The round-trip deadline is size-scaled like the whole-file path's
/// (mountd re-hashes + copies the entire file), and `RaceTimeout` gets
/// the same cache re-check + single retry.
fn promote_streamed(ctx: &FillContext) {
    let timeout = crate::store_fetch::jit_fetch_timeout(ctx.mountd_timeout, ctx.size);
    if let Err(e) =
        super::open::promote_with_race_retry(&ctx.mountd, ctx.file_digest, &ctx.cache_path, timeout)
    {
        metrics::counter!("rio_builder_castore_fuse_promote_fail_total").increment(1);
        tracing::warn!(
            digest = %hex::encode(ctx.file_digest),
            error = %e,
            build_fatal = e.is_build_fatal(),
            "castore-fuse: promoting the streamed file failed; this build keeps reading from \
             staging, but later opens of this digest will re-fetch"
        );
    }
}

/// One chunk's place in the fill: its content address, its full size,
/// and which of its bytes land in the file (the first and last chunks
/// of a window may begin or end mid-chunk — a `file_digest` resolved
/// via a legacy whole-NAR-chunked manifest has NAR framing or adjacent
/// files' bytes around it).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedChunk {
    digest: [u8; 32],
    size: u64,
    take: std::ops::Range<u64>,
}

impl PlannedChunk {
    fn contribution(&self) -> u64 {
        self.take.end - self.take.start
    }
}

/// Validate a `StatBlobResponse` into the ordered chunk plan. Rejects
/// malformed windows (slice offsets outside their chunk, or a window
/// whose contributions don't sum to the file size) before any byte is
/// fetched or served.
fn plan_chunks(resp: &StatBlobResponse, file_size: u64) -> Result<Vec<PlannedChunk>, String> {
    let n = resp.chunks.len();
    if n == 0 {
        return Err("StatBlob returned no chunks for a non-empty file".into());
    }
    let mut plan = Vec::with_capacity(n);
    for (i, meta) in resp.chunks.iter().enumerate() {
        let digest: [u8; 32] = meta
            .digest
            .as_slice()
            .try_into()
            .map_err(|_| format!("chunk {i} digest is {} bytes, want 32", meta.digest.len()))?;
        // Per-chunk size sanity: the store's FastCDC never produces an
        // empty chunk or one above 4 MiB, so a window claiming either is
        // malformed. Rejecting here bounds what a single GetChunks reply
        // can make the fill buffer/write before its own length check.
        if meta.size == 0 || meta.size > CHUNK_SIZE_MAX {
            return Err(format!(
                "chunk {i} claims {} bytes (want 1..={CHUNK_SIZE_MAX})",
                meta.size
            ));
        }
        let start = if i == 0 {
            u64::from(resp.first_chunk_skip)
        } else {
            0
        };
        let end = if i == n - 1 {
            u64::from(resp.last_chunk_take)
        } else {
            meta.size
        };
        if start > end || end > meta.size {
            return Err(format!(
                "chunk {i} slice {start}..{end} is outside its {} bytes",
                meta.size
            ));
        }
        plan.push(PlannedChunk {
            digest,
            size: meta.size,
            take: start..end,
        });
    }
    let total: u64 = plan.iter().map(PlannedChunk::contribution).sum();
    if total != file_size {
        return Err(format!(
            "chunk window contributes {total} bytes but the file is {file_size}"
        ));
    }
    Ok(plan)
}

/// `{root}/{ab}/{hex}` — the shard layout rio-mountd's promotes write
/// (shared by the backing cache and the chunk cache).
fn sharded(root: &Path, digest: &[u8; 32]) -> PathBuf {
    let hex = hex::encode(digest);
    root.join(&hex[..2]).join(&hex)
}

/// Time left before `deadline`, or [`FillError::Timeout`] if it has
/// passed. Every blocking step in the fill goes through this so the
/// whole fill respects one budget.
fn remaining(deadline: Instant) -> Result<Duration, FillError> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(FillError::Timeout);
    }
    Ok(left)
}

/// The fill body: resolve the chunk window, assemble the `.partial`
/// chunk by chunk, verify, rename, promote.
fn fill_blob(ctx: &FillContext, fill: &StreamFill, deadline: Instant) -> Result<(), FillError> {
    let resp = match stat_blob(ctx, deadline)? {
        StatOutcome::Chunked(resp) => resp,
        // An inline manifest has no chunk list (`FailedPrecondition`
        // per the StatBlob contract in store.proto). Reachable only
        // when stream_threshold is configured below the store's inline
        // ceiling — fall back to the whole-file stream rather than
        // failing the open.
        StatOutcome::Inline => {
            tracing::debug!(
                "castore-fuse: StatBlob says inline manifest; streaming via ReadBlob instead"
            );
            return fill_from_read_blob(ctx, fill, deadline);
        }
    };
    let plan = plan_chunks(&resp, ctx.size).map_err(FillError::Integrity)?;

    let mut run = FillRun {
        ctx,
        fill,
        deadline,
        is_local: vec![false; plan.len()],
        needed: {
            let mut needed: HashMap<[u8; 32], usize> = HashMap::new();
            for c in &plan {
                *needed.entry(c.digest).or_default() += 1;
            }
            needed
        },
        plan,
        probed: 0,
        requested: HashSet::new(),
        pending: HashMap::new(),
        remote: RemoteChunks::new(
            ctx.clients.clone(),
            ctx.runtime.clone(),
            ctx.assignment_token.clone(),
        ),
        promote_batch: Vec::new(),
        hasher: blake3::Hasher::new(),
        offset: 0,
    };
    run.assemble()?;
    run.flush_promotes();

    // Whole-file verification gates both the rename (what later reads
    // of the cache see) and the Promote (what other builds see).
    // r[impl builder.fs.file-digest-integrity]
    let got = run.hasher.finalize();
    if got.as_bytes() != &ctx.file_digest {
        return Err(FillError::Integrity(format!(
            "assembled file hashes to {got}, want {}",
            hex::encode(ctx.file_digest)
        )));
    }
    finish_rename(ctx)
}

/// Rename the verified `.partial` to the bare digest name (the path
/// mountd's `Promote` reads, and the file readers keep preading through
/// the already-open fd). The Promote itself happens after the fill's
/// success is published — see [`promote_streamed`].
fn finish_rename(ctx: &FillContext) -> Result<(), FillError> {
    std::fs::rename(&ctx.partial_path, &ctx.staging_path)
        .map_err(|e| FillError::Io(format!("staging rename failed: {e}")))
}

/// What `StatBlob` said about the blob's layout.
enum StatOutcome {
    /// A chunk window the fill can assemble from.
    Chunked(StatBlobResponse),
    /// The manifest is inline (no chunk list): the store answers
    /// `FailedPrecondition` and the caller must use `ReadBlob`.
    Inline,
}

/// `StatBlob(file_digest, send_chunks=true)` bounded by the fill
/// deadline. Transport-level failures get the short in-budget transient
/// retry (`store_fetch::retry_transient`); a `CASTORE_SCOPE_REQUIRED`
/// answer gets the closure re-present-and-retry
/// (`r[builder.castore.scope-present]`) — both loops run inside `left`,
/// so the fill's overall deadline and its single breaker record are
/// unchanged.
// r[impl builder.castore.scope-present]
fn stat_blob(ctx: &FillContext, deadline: Instant) -> Result<StatOutcome, FillError> {
    let left = remaining(deadline)?;
    let resp = ctx.runtime.block_on(async {
        tokio::time::timeout(
            left,
            ctx.scope.run_scoped("stat_blob", async || {
                crate::store_fetch::retry_transient("stat_blob", async || {
                    let req = crate::store_fetch::authed_request(
                        StatBlobRequest {
                            file_digest: ctx.file_digest.to_vec(),
                            send_chunks: true,
                        },
                        &ctx.assignment_token,
                    )?;
                    let mut directory = ctx.clients.directory.clone();
                    directory.stat_blob(req).await
                })
                .await
            }),
        )
        .await
    });
    match resp {
        Err(_elapsed) => Err(FillError::Timeout),
        // A scope-required answer that survived the re-present loop is
        // NOT the inline-manifest signal — falling through to ReadBlob
        // would just hit the same denial.
        Ok(Err(status)) if ScopePresenter::is_scope_required(&status) => {
            Err(FillError::ScopeRequired(format!("StatBlob: {status}")))
        }
        // The documented "no chunk list — use ReadBlob" signal
        // (store.proto StatBlob): an inline manifest, not a failure.
        Ok(Err(status)) if status.code() == tonic::Code::FailedPrecondition => {
            Ok(StatOutcome::Inline)
        }
        Ok(Err(status)) => Err(FillError::Store(format!("StatBlob: {status}"))),
        Ok(Ok(resp)) => Ok(StatOutcome::Chunked(resp.into_inner())),
    }
}

/// Whole-file fallback for inline manifests: stream `ReadBlob` into the
/// `.partial`, advancing the high-water mark per frame so readers
/// unblock progressively, then verify/rename as usual.
///
/// Only the stream OPEN gets the in-budget transient retry: once frames
/// have been written the high-water mark has been published to readers,
/// so a mid-stream failure stays a fill failure (the next open of this
/// digest starts a fresh fill) rather than risking a partially-rewound
/// file under live readers.
fn fill_from_read_blob(
    ctx: &FillContext,
    fill: &StreamFill,
    deadline: Instant,
) -> Result<(), FillError> {
    use std::os::unix::fs::FileExt;

    let left = remaining(deadline)?;
    let mut stream = match ctx.runtime.block_on(async {
        tokio::time::timeout(
            left,
            ctx.scope.run_scoped("read_blob_connect", async || {
                crate::store_fetch::retry_transient("read_blob_connect", async || {
                    let req = crate::store_fetch::authed_request(
                        rio_proto::types::ReadBlobRequest {
                            file_digest: ctx.file_digest.to_vec(),
                        },
                        &ctx.assignment_token,
                    )?;
                    let mut directory = ctx.clients.directory.clone();
                    directory.read_blob(req).await
                })
                .await
            }),
        )
        .await
    }) {
        Err(_elapsed) => return Err(FillError::Timeout),
        Ok(Err(status)) if ScopePresenter::is_scope_required(&status) => {
            return Err(FillError::ScopeRequired(format!("ReadBlob: {status}")));
        }
        Ok(Err(status)) => return Err(FillError::Store(format!("ReadBlob: {status}"))),
        Ok(Ok(resp)) => resp.into_inner(),
    };

    let mut hasher = blake3::Hasher::new();
    let mut offset = 0u64;
    loop {
        let left = remaining(deadline)?;
        let frame = ctx
            .runtime
            .block_on(async { tokio::time::timeout(left, stream.message()).await });
        let frame = match frame {
            Err(_elapsed) => return Err(FillError::Timeout),
            Ok(Err(status)) => return Err(FillError::Store(format!("ReadBlob: {status}"))),
            Ok(Ok(None)) => break,
            Ok(Ok(Some(frame))) => frame,
        };
        // Per-frame overrun guard: fail at the first frame that would
        // exceed the inode's declared size instead of only noticing the
        // mismatch at stream end — bounds how much junk a misbehaving
        // ReadBlob can write into staging (and keeps the high-water mark
        // inside the EOF readers are clamped to).
        if offset + frame.data.len() as u64 > ctx.size {
            return Err(FillError::Integrity(format!(
                "ReadBlob streamed past the declared size ({} + {} > {})",
                offset,
                frame.data.len(),
                ctx.size
            )));
        }
        hasher.update(&frame.data);
        fill.partial
            .write_all_at(&frame.data, offset)
            .map_err(|e| FillError::Io(format!("writing .partial: {e}")))?;
        offset += frame.data.len() as u64;
        fill.set_high_water(offset);
        metrics::counter!("rio_builder_castore_fuse_fetch_bytes_total", "hit" => "remote")
            .increment(frame.data.len() as u64);
    }
    metrics::counter!("rio_builder_castore_fuse_chunk_source_total", "src" => "remote")
        .increment(1);
    if offset != ctx.size {
        return Err(FillError::Integrity(format!(
            "ReadBlob streamed {offset} bytes, want {}",
            ctx.size
        )));
    }
    let got = hasher.finalize();
    if got.as_bytes() != &ctx.file_digest {
        return Err(FillError::Integrity(format!(
            "ReadBlob content hashes to {got}, want {}",
            hex::encode(ctx.file_digest)
        )));
    }
    finish_rename(ctx)
}

/// Mutable state of one in-progress chunk assembly.
struct FillRun<'a> {
    ctx: &'a FillContext,
    fill: &'a StreamFill,
    deadline: Instant,
    plan: Vec<PlannedChunk>,
    /// Whether the chunk at each plan index was present in the node
    /// chunk cache when its window slot was probed.
    is_local: Vec<bool>,
    /// Remaining occurrences per digest (a CDC plan can repeat a chunk,
    /// e.g. identical zero-page runs); the received copy is kept in
    /// `pending` until its last occurrence is consumed.
    needed: HashMap<[u8; 32], usize>,
    /// Plan indexes `< probed` have been probed locally and (on miss)
    /// requested remotely.
    probed: usize,
    /// Digests already sent to `GetChunks` (request dedup).
    requested: HashSet<[u8; 32]>,
    /// Remote chunks received but not yet consumed by the assembly
    /// cursor (out-of-order arrivals). Bounded by the lookahead window.
    pending: HashMap<[u8; 32], Vec<u8>>,
    remote: RemoteChunks,
    /// Remotely-fetched, staged chunk digests awaiting `PromoteChunks`.
    promote_batch: Vec<[u8; 32]>,
    hasher: blake3::Hasher,
    /// Bytes of the file written so far == the next contribution's
    /// offset == the published high-water mark.
    offset: u64,
}

impl FillRun<'_> {
    /// Walk the plan in order, sourcing each chunk locally or remotely,
    /// and append its contribution to the `.partial`.
    fn assemble(&mut self) -> Result<(), FillError> {
        for i in 0..self.plan.len() {
            self.extend_window(i)?;
            let bytes = self.obtain(i)?;
            self.append_contribution(i, &bytes)?;
        }
        Ok(())
    }

    /// Probe the node chunk cache (and request remote misses) for every
    /// chunk up to `cursor + FETCH_LOOKAHEAD`, batching the misses
    /// discovered in this pass into one `GetChunks` request frame.
    // r[impl builder.fs.node-chunk-cache]
    fn extend_window(&mut self, cursor: usize) -> Result<(), FillError> {
        let mut misses: Vec<Vec<u8>> = Vec::new();
        while self.probed < self.plan.len() && self.probed <= cursor + FETCH_LOOKAHEAD {
            let chunk = &self.plan[self.probed];
            let cached = matches!(
                std::fs::metadata(sharded(&self.ctx.chunks_dir, &chunk.digest)),
                Ok(meta) if meta.len() == chunk.size
            );
            self.is_local[self.probed] = cached;
            if !cached && self.requested.insert(chunk.digest) {
                misses.push(chunk.digest.to_vec());
            }
            self.probed += 1;
        }
        if !misses.is_empty() {
            let left = remaining(self.deadline)?;
            self.remote.request(misses, left)?;
        }
        Ok(())
    }

    /// Produce the full bytes of plan index `i`, from the node chunk
    /// cache or the `GetChunks` stream.
    fn obtain(&mut self, i: usize) -> Result<Vec<u8>, FillError> {
        let chunk = self.plan[i].clone();
        if self.is_local[i] {
            match std::fs::read(sharded(&self.ctx.chunks_dir, &chunk.digest)) {
                Ok(bytes) if bytes.len() as u64 == chunk.size => {
                    metrics::counter!("rio_builder_castore_fuse_chunk_source_total", "src" => "node_ssd")
                        .increment(1);
                    metrics::counter!("rio_builder_castore_fuse_fetch_bytes_total", "hit" => "node_ssd")
                        .increment(bytes.len() as u64);
                    return Ok(bytes);
                }
                // Evicted (or truncated) between the probe and the
                // read: fall through to a one-off remote fetch.
                _ => {
                    if self.requested.insert(chunk.digest) {
                        let left = remaining(self.deadline)?;
                        self.remote.request(vec![chunk.digest.to_vec()], left)?;
                    }
                }
            }
        }
        if let Some(bytes) = self.take_pending(&chunk) {
            return Ok(bytes);
        }
        loop {
            let left = remaining(self.deadline)?;
            let data = self.remote.recv(left)?;
            self.accept_remote(&data)?;
            if let Some(bytes) = self.take_pending(&chunk) {
                return Ok(bytes);
            }
        }
    }

    /// Take chunk `chunk` from the receive buffer if it has arrived,
    /// keeping a copy only while later plan indexes still need it.
    fn take_pending(&mut self, chunk: &PlannedChunk) -> Option<Vec<u8>> {
        let outstanding = self.needed.get_mut(&chunk.digest)?;
        let bytes = if *outstanding > 1 {
            self.pending.get(&chunk.digest).cloned()
        } else {
            self.pending.remove(&chunk.digest)
        }?;
        *outstanding -= 1;
        Some(bytes)
    }

    /// Verify and stage one `ChunkData` arrival, then make it available
    /// to the assembly cursor.
    fn accept_remote(&mut self, data: &ChunkData) -> Result<(), FillError> {
        let digest: [u8; 32] = data
            .digest
            .as_slice()
            .try_into()
            .map_err(|_| FillError::Store("GetChunks sent a non-32-byte digest".into()))?;
        if !self.requested.contains(&digest) {
            return Err(FillError::Store(format!(
                "GetChunks sent un-requested chunk {}",
                hex::encode(digest)
            )));
        }
        // Per-chunk verification on arrival (§2.7): never serve — and
        // never stage for other builds — a byte that doesn't hash to
        // its content address.
        let got = blake3::hash(&data.data);
        if got.as_bytes() != &digest {
            return Err(FillError::Integrity(format!(
                "chunk {} arrived hashing to {got}",
                hex::encode(digest)
            )));
        }
        metrics::counter!("rio_builder_castore_fuse_chunk_source_total", "src" => "remote")
            .increment(1);
        metrics::counter!("rio_builder_castore_fuse_fetch_bytes_total", "hit" => "remote")
            .increment(data.data.len() as u64);
        self.stage_for_promote(&digest, &data.data);
        self.pending.insert(digest, data.data.to_vec());
        Ok(())
    }

    /// Write a verified remote chunk into this build's staging chunk
    /// dir and queue it for `PromoteChunks`, so the next build on this
    /// node finds it in `/var/rio/chunks/`. Best-effort: assembly never
    /// depends on it (the contribution goes into the `.partial`
    /// directly), so a failure here only costs cross-build dedup.
    fn stage_for_promote(&mut self, digest: &[u8; 32], bytes: &[u8]) {
        let _ = std::fs::create_dir_all(&self.ctx.staging_chunks_dir);
        let path = self.ctx.staging_chunks_dir.join(hex::encode(digest));
        if let Err(e) = std::fs::write(&path, bytes) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "castore-fuse: staging a chunk for PromoteChunks failed; other builds will refetch it"
            );
            return;
        }
        self.promote_batch.push(*digest);
        if self.promote_batch.len() >= PROMOTE_BATCH {
            self.flush_promotes();
        }
    }

    /// Send the pending `PromoteChunks` batch (if any) and drop the
    /// staged copies. Failures are logged, never fatal: the batch is
    /// purely for *other* builds' benefit — this build assembles from
    /// its own `.partial`.
    fn flush_promotes(&mut self) {
        if self.promote_batch.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.promote_batch);
        if let Err(e) = self
            .ctx
            .mountd
            .promote_chunks(batch.clone(), self.ctx.mountd_timeout)
        {
            tracing::warn!(
                chunks = batch.len(),
                error = %e,
                "castore-fuse: PromoteChunks failed; other builds will refetch these chunks"
            );
        }
        // Promoted (or given up on) — either way the staged copies are
        // dead weight; dropping them keeps staging usage bounded to one
        // batch instead of the whole file.
        for digest in &batch {
            let _ = std::fs::remove_file(self.ctx.staging_chunks_dir.join(hex::encode(digest)));
        }
    }

    /// Slice plan index `i`'s contribution out of its full chunk bytes,
    /// append it to the `.partial`, and publish the new high-water
    /// mark.
    fn append_contribution(&mut self, i: usize, bytes: &[u8]) -> Result<(), FillError> {
        use std::os::unix::fs::FileExt;

        let chunk = &self.plan[i];
        if bytes.len() as u64 != chunk.size {
            return Err(FillError::Integrity(format!(
                "chunk {} is {} bytes, the plan says {}",
                hex::encode(chunk.digest),
                bytes.len(),
                chunk.size
            )));
        }
        let slice_overflow =
            |_| FillError::Integrity("chunk slice offset does not fit in usize".into());
        let take = usize::try_from(chunk.take.start).map_err(slice_overflow)?
            ..usize::try_from(chunk.take.end).map_err(slice_overflow)?;
        let contribution = &bytes[take];
        self.hasher.update(contribution);
        self.fill
            .partial
            .write_all_at(contribution, self.offset)
            .map_err(|e| FillError::Io(format!("writing .partial: {e}")))?;
        self.offset += contribution.len() as u64;
        self.fill.set_high_water(self.offset);
        Ok(())
    }
}

/// Lazily-opened `GetChunks` bidi stream: request frames are pushed as
/// local-cache misses are discovered, responses are pulled on demand by
/// the assembly cursor. Never opened at all when every chunk is local.
struct RemoteChunks {
    clients: StoreClients,
    runtime: Handle,
    /// Attached to the `GetChunks` stream-open request (the per-frame
    /// digests inherit the stream's auth).
    assignment_token: String,
    live: Option<(
        tokio::sync::mpsc::Sender<GetChunksRequest>,
        tonic::Streaming<ChunkData>,
    )>,
}

impl RemoteChunks {
    fn new(clients: StoreClients, runtime: Handle, assignment_token: String) -> Self {
        Self {
            clients,
            runtime,
            assignment_token,
            live: None,
        }
    }

    /// Send one request frame carrying `digests`, opening the stream on
    /// first use.
    fn request(&mut self, digests: Vec<Vec<u8>>, timeout: Duration) -> Result<(), FillError> {
        if self.live.is_none() {
            // Stream open gets the in-budget transient retry. Each
            // attempt mints its own request channel (a failed attempt
            // consumed the previous receiver); the winning attempt's
            // sender is what gets kept. Channel capacity 8 covers the
            // frames a full lookahead window can produce before the
            // server starts draining.
            let clients = self.clients.clone();
            let token = self.assignment_token.clone();
            let connect = self.runtime.block_on(async {
                tokio::time::timeout(
                    timeout,
                    crate::store_fetch::retry_transient("get_chunks_connect", async || {
                        let (tx, rx) = tokio::sync::mpsc::channel::<GetChunksRequest>(8);
                        let req =
                            crate::store_fetch::authed_request(ReceiverStream::new(rx), &token)?;
                        let mut chunk_client = clients.chunk.clone();
                        let streaming = chunk_client.get_chunks(req).await?.into_inner();
                        Ok((tx, streaming))
                    }),
                )
                .await
            });
            let (tx, streaming) = match connect {
                Err(_elapsed) => return Err(FillError::Timeout),
                Ok(Err(status)) => return Err(FillError::Store(format!("GetChunks: {status}"))),
                Ok(Ok(pair)) => pair,
            };
            self.live = Some((tx, streaming));
        }
        let (tx, _) = self.live.as_ref().expect("just installed");
        let sent = self.runtime.block_on(async {
            tokio::time::timeout(timeout, tx.send(GetChunksRequest { digests })).await
        });
        match sent {
            Err(_elapsed) => Err(FillError::Timeout),
            Ok(Err(_closed)) => Err(FillError::Store(
                "GetChunks request stream closed early".into(),
            )),
            Ok(Ok(())) => Ok(()),
        }
    }

    /// Receive the next chunk from the stream.
    fn recv(&mut self, timeout: Duration) -> Result<ChunkData, FillError> {
        let Some((_, streaming)) = self.live.as_mut() else {
            // Reaching here means the assembly cursor wants a chunk it
            // never requested — a planner bug, not a store failure.
            return Err(FillError::Store(
                "waiting for a chunk that was never requested".into(),
            ));
        };
        let next = self
            .runtime
            .block_on(async { tokio::time::timeout(timeout, streaming.message()).await });
        match next {
            Err(_elapsed) => Err(FillError::Timeout),
            Ok(Err(status)) => Err(FillError::Store(format!("GetChunks: {status}"))),
            Ok(Ok(None)) => Err(FillError::Store(
                "GetChunks stream ended before every requested chunk arrived".into(),
            )),
            Ok(Ok(Some(chunk))) => Ok(chunk),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_proto::types::ChunkMeta;

    fn meta(byte: u8, size: u64) -> ChunkMeta {
        ChunkMeta {
            digest: vec![byte; 32],
            size,
        }
    }

    /// The reassembly contract from types.proto: chunk 0 is sliced from
    /// `first_chunk_skip`, the last chunk up to `last_chunk_take`,
    /// middle chunks contribute whole, and a single-chunk window slices
    /// both ends.
    #[test]
    fn plan_chunks_slices_first_and_last() {
        let resp = StatBlobResponse {
            chunks: vec![meta(1, 100), meta(2, 50), meta(3, 80)],
            first_chunk_skip: 30,
            last_chunk_take: 20,
        };
        let plan = plan_chunks(&resp, 70 + 50 + 20).expect("valid window");
        assert_eq!(plan[0].take, 30..100);
        assert_eq!(plan[1].take, 0..50);
        assert_eq!(plan[2].take, 0..20);
        assert_eq!(
            plan.iter().map(PlannedChunk::contribution).sum::<u64>(),
            140
        );

        let single = StatBlobResponse {
            chunks: vec![meta(7, 100)],
            first_chunk_skip: 10,
            last_chunk_take: 90,
        };
        let plan = plan_chunks(&single, 80).expect("single-chunk window");
        assert_eq!(plan[0].take, 10..90);
    }

    /// Malformed windows are rejected before any fetch: a zero-chunk
    /// response for a non-empty file, slice offsets outside the chunk,
    /// and a window whose contributions don't sum to the file size.
    #[test]
    fn plan_chunks_rejects_malformed_windows() {
        let empty = StatBlobResponse::default();
        assert!(plan_chunks(&empty, 1).is_err());

        let oversliced = StatBlobResponse {
            chunks: vec![meta(1, 10)],
            first_chunk_skip: 0,
            last_chunk_take: 11,
        };
        assert!(plan_chunks(&oversliced, 11).is_err());

        let inverted = StatBlobResponse {
            chunks: vec![meta(1, 10)],
            first_chunk_skip: 9,
            last_chunk_take: 3,
        };
        assert!(plan_chunks(&inverted, 10).is_err());

        let wrong_total = StatBlobResponse {
            chunks: vec![meta(1, 10), meta(2, 10)],
            first_chunk_skip: 0,
            last_chunk_take: 10,
        };
        assert!(plan_chunks(&wrong_total, 5).is_err());

        let bad_digest = StatBlobResponse {
            chunks: vec![ChunkMeta {
                digest: vec![1; 7],
                size: 10,
            }],
            first_chunk_skip: 0,
            last_chunk_take: 10,
        };
        assert!(plan_chunks(&bad_digest, 10).is_err());
    }

    /// Per-chunk size sanity: a window claiming a zero-size chunk or one
    /// above the FastCDC 4 MiB ceiling is rejected before any byte is
    /// fetched; a chunk exactly at the ceiling is fine.
    #[test]
    fn plan_chunks_rejects_out_of_bounds_chunk_sizes() {
        let zero = StatBlobResponse {
            chunks: vec![meta(1, 0), meta(2, 10)],
            first_chunk_skip: 0,
            last_chunk_take: 10,
        };
        assert!(
            plan_chunks(&zero, 10)
                .expect_err("zero-size chunk")
                .contains("chunk 0"),
            "the error names the offending chunk"
        );

        let oversize = StatBlobResponse {
            chunks: vec![meta(1, CHUNK_SIZE_MAX + 1)],
            first_chunk_skip: 0,
            last_chunk_take: 10,
        };
        assert!(plan_chunks(&oversize, 10).is_err());

        let at_ceiling = StatBlobResponse {
            chunks: vec![meta(1, CHUNK_SIZE_MAX)],
            first_chunk_skip: 0,
            last_chunk_take: u32::try_from(CHUNK_SIZE_MAX).unwrap(),
        };
        assert!(plan_chunks(&at_ceiling, CHUNK_SIZE_MAX).is_ok());
    }

    fn test_fill(size: u64) -> (tempfile::TempDir, Arc<StreamFill>) {
        let dir = tempfile::tempdir().unwrap();
        let partial = super::super::open::create_partial(&dir.path().join("x.partial")).unwrap();
        (
            dir,
            Arc::new(StreamFill::new(partial, size, Duration::from_secs(5))),
        )
    }

    /// Reads inside the verified prefix are served immediately; reads
    /// beyond it block until the high-water mark covers them; reads at
    /// or past the true size are EOF (empty), and reads spanning it are
    /// truncated — even while the on-disk file is still short.
    // r[verify builder.fs.streaming-open]
    #[test]
    fn read_at_serves_the_prefix_and_blocks_for_the_rest() {
        use std::os::unix::fs::FileExt;
        let (_dir, fill) = test_fill(10);
        fill.partial.write_all_at(b"01234", 0).unwrap();
        fill.set_high_water(5);

        assert_eq!(fill.read_at(0, 5).unwrap(), b"01234");
        assert_eq!(fill.read_at(2, 2).unwrap(), b"23");
        // EOF semantics use the true size, not the current disk size.
        assert_eq!(fill.read_at(10, 4).unwrap(), b"");
        assert_eq!(fill.read_at(12, 4).unwrap(), b"");

        // A read past the high-water mark blocks until the fill
        // catches up, then sees the late bytes.
        let reader = {
            let fill = Arc::clone(&fill);
            std::thread::spawn(move || fill.read_at(5, 10))
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(!reader.is_finished(), "the read must wait for its range");
        fill.partial.write_all_at(b"56789", 5).unwrap();
        fill.set_high_water(10);
        // Spans EOF → truncated to the true size.
        assert_eq!(reader.join().unwrap().unwrap(), b"56789");
    }

    /// A failed fill wakes blocked readers with EIO and fails every
    /// later read on the same handle — no reader hangs out its full
    /// budget waiting on a fill that already died.
    // r[verify builder.fs.streaming-open]
    #[test]
    fn a_failed_fill_unblocks_readers_with_eio() {
        use std::os::unix::fs::FileExt;
        let (_dir, fill) = test_fill(100);
        fill.partial.write_all_at(b"prefix", 0).unwrap();
        fill.set_high_water(6);

        let blocked = {
            let fill = Arc::clone(&fill);
            std::thread::spawn(move || fill.read_at(50, 10))
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(!blocked.is_finished());
        fill.finish(Err(Errno::EIO));
        assert_eq!(
            blocked.join().unwrap().expect_err("EIO").code(),
            Errno::EIO.code()
        );
        // The already-verified prefix also fails: the fill is dead and
        // the build is failing — fail it loudly now.
        assert_eq!(
            fill.read_at(0, 4).expect_err("EIO").code(),
            Errno::EIO.code()
        );
    }

    /// A blocked read gives up with EIO once the wait budget expires
    /// (the backstop for a fill thread wedged in a way its own
    /// timeouts did not catch).
    #[test]
    fn a_blocked_read_times_out_at_the_wait_budget() {
        let dir = tempfile::tempdir().unwrap();
        let partial = super::super::open::create_partial(&dir.path().join("x.partial")).unwrap();
        let fill = StreamFill::new(partial, 100, Duration::from_millis(50));
        let started = Instant::now();
        assert_eq!(
            fill.read_at(50, 10).expect_err("EIO").code(),
            Errno::EIO.code()
        );
        assert!(started.elapsed() >= Duration::from_millis(50));
    }
}
