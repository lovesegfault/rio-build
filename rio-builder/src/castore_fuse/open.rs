//! The castore-FUSE `open()` data path (ADR-022 §2.6–2.7).
//!
//! `open(ino → file_digest)` is a broker, not a server: its job is to
//! make sure the file exists in the node-SSD backing cache and hand
//! the kernel a passthrough fd to it. Warm reads then go kernel →
//! ext4 with zero FUSE involvement. The handler fetches on miss
//! (`ReadBlob` into the build's staging dir, verify, `Promote` to
//! mountd) and is upcalled for `read()` only when an open degrades to
//! a userspace `FOPEN_KEEP_CACHE` read — the escape hatch, the
//! backing-id ceiling, an io-mode conflict with live caching handles
//! of the inode, a cache entry evicted right after its promote — or,
//! for files above the streaming threshold, during the one-shot
//! background-fill window of the first cold open on the node
//! (`stream.rs`).
//!
//! Everything here is **synchronous from the caller's point of view**
//! — FUSE callbacks run on fuser's thread pool. The gRPC fetch is
//! bridged with `Handle::block_on`; the mountd UDS client blocks the
//! calling thread directly.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Seek, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fuser::Errno;
use tokio::runtime::Handle;
use tonic::transport::Channel;

use rio_proto::store::chunk_service_client::ChunkServiceClient;
use rio_proto::store::directory_service_client::DirectoryServiceClient;
use rio_proto::types::ReadBlobRequest;

use super::circuit::CircuitBreaker;
use super::mountd_client::{MountdClient, MountdError};
use super::mountd_proto::ErrKind;
use super::stream::{FillCtx, FillState};
use crate::IgnorePoison;
use crate::store_fetch::jit_fetch_timeout;

/// Tunables threaded from `Config` at mount time. Plain data so the
/// FUSE side never touches the config crate.
#[derive(Clone, Debug)]
pub struct OpenerConfig {
    /// Cache misses larger than this classify as `miss_stream` (§2.8's
    /// streaming-open path).
    pub stream_threshold: u64,
    /// Ceiling on every mountd UDS round-trip except `Promote` (which
    /// gets a size-proportional budget on top).
    pub mountd_request_timeout: Duration,
    /// Base fetch timeout; scaled up by file size via
    /// [`jit_fetch_timeout`].
    pub fetch_timeout: Duration,
    /// Concurrent-passthrough-open ceiling. The kernel never reclaims
    /// a backing slot until `BACKING_CLOSE`, so a build leaking opens
    /// would otherwise grow mountd's IDR without bound. At the ceiling
    /// new digests degrade to userspace `FOPEN_KEEP_CACHE` reads of the
    /// cache entry instead of registering more backings — slower, never
    /// build-fatal.
    pub max_backing_ids: usize,
    /// Escape hatch (`RIO_DISABLE_PASSTHROUGH`): reply plain
    /// `FOPEN_KEEP_CACHE` and serve `read()` from userspace instead of
    /// registering passthrough backings.
    pub disable_passthrough: bool,
}

/// What an `open()` decided to reply, before it is marshalled into the
/// fuser reply object. Split from the decision logic so the open path
/// can be exercised by unit tests without a kernel FUSE channel.
#[derive(Debug)]
pub(super) enum OpenOutcome {
    /// `opened_passthrough(fh, …, backing_id)`: warm reads go kernel →
    /// backing file with no upcall.
    Passthrough { fh: u64, backing_id: u32 },
    /// `opened(fh, FOPEN_KEEP_CACHE)`: userspace `read()` upcalls served
    /// from [`Opener::open_files`] or [`Opener::streams`].
    KeepCache { fh: u64 },
}

/// Per-mount `open()`/`read()`/`release()` state. One per castore-FUSE
/// session, shared across fuser's callback threads.
pub struct Opener {
    mountd: MountdClient,
    directory: DirectoryServiceClient<Channel>,
    chunk: ChunkServiceClient<Channel>,
    /// Assignment token for store RPCs; empty = no header — see
    /// `mount_and_serve`'s `assignment_token` parameter.
    assignment_token: String,
    runtime: Handle,
    circuit: Arc<CircuitBreaker>,
    /// `/var/rio/cache` — mountd-owned, read-only to the builder.
    cache_dir: PathBuf,
    /// `/var/rio/chunks` — mountd-owned, read-only to the builder.
    chunks_dir: PathBuf,
    /// `/var/rio/staging/{build_id}` — builder-writable, quota'd.
    staging_dir: PathBuf,
    cfg: OpenerConfig,
    /// Per-digest io-mode bookkeeping: which open mode the digest's
    /// inode is currently in and how many handles hold it. The kernel
    /// refuses to mix caching and passthrough opens on one inode
    /// (fs/fuse/iomode.c, `-ETXTBSY` → user-visible `EIO`), so every
    /// mode decision happens under this single lock — see
    /// [`Opener::backed_outcome`].
    // r[impl builder.fs.open-iomode-compatible]
    modes: Mutex<IoModes>,
    /// Per-digest fetch singleflight. Concurrent opens of the same
    /// digest within one build (e.g. `make -jN` both `dlopen`ing one
    /// `.so`) block on the winner's lock instead of double-fetching.
    /// Never pruned — bounded by the distinct digests a build opens,
    /// and the mount is ephemeral.
    fills: Mutex<HashMap<[u8; 32], Arc<Mutex<()>>>>,
    /// Per-digest in-flight streaming fills. Unlike [`Opener::fills`],
    /// a joiner does not wait for the winner to finish — it shares the
    /// winner's [`FillState`] and unblocks at the first chunk. Entries
    /// remove themselves when the fill task exits. `Arc` so the
    /// spawned fill task can do that removal after the `Opener`'s
    /// callback returns.
    active_fills: Arc<Mutex<HashMap<[u8; 32], Arc<FillState>>>>,
    /// Fallback read path: fh → an open file with the digest's verified
    /// bytes (normally the cache entry; after a post-promote eviction,
    /// the staging copy). Populated when passthrough is disabled,
    /// unavailable, or io-mode-incompatible for the open.
    open_files: Mutex<HashMap<u64, File>>,
    /// Streaming read path: fh → the in-flight (or completed) fill the
    /// handle reads from.
    streams: Mutex<HashMap<u64, Arc<FillState>>>,
    next_fh: AtomicU64,
}

/// See [`Opener::modes`].
#[derive(Default)]
struct IoModes {
    map: HashMap<[u8; 32], DigestMode>,
    /// How many `map` entries are [`DigestMode::Passthrough`] — the
    /// live registrations the `max_backing_ids` ceiling bounds.
    backings: usize,
}

/// The live open mode of one digest's content inode.
enum DigestMode {
    /// Live caching (`FOPEN_KEEP_CACHE`) handles — every fh in
    /// [`Opener::open_files`] or [`Opener::streams`] counts one. While
    /// the count is non-zero, opens of the digest must keep replying a
    /// caching mode and must not register a passthrough backing.
    Caching(u32),
    /// One passthrough backing per `file_digest`, refcounted across
    /// opens. The kernel rejects a second concurrent passthrough open
    /// whose `fuse_backing` differs from the inode's recorded one
    /// (`-EBUSY` → user-visible `EIO`), and overlay copy-up opens the
    /// lower several times in one syscall — so the id MUST be reused,
    /// not re-registered. While it is live, opens of the digest must
    /// not reply a caching mode.
    Passthrough(BackingRef),
}

struct BackingRef {
    id: u32,
    refcount: u32,
}

/// `fetch_into` retry budget for Unavailable/Unknown transport errors.
/// A rio-store rolling restart drops in-flight `ReadBlob` streams for a
/// few seconds; without a bounded retry every cold `open()` in that
/// window EIOs and fails a long build for a blip the next attempt would
/// absorb. Kept small so a genuinely down store still trips the fetch
/// circuit breaker quickly instead of parking fuser callback threads.
const TRANSIENT_FETCH_ATTEMPTS: u32 = 3;

/// Base pause between transient fetch attempts; jittered in
/// [`transient_fetch_backoff`] so a `make -jN` burst of cold opens does
/// not re-converge on the restarting store in lockstep.
const TRANSIENT_FETCH_BACKOFF: Duration = Duration::from_millis(250);

/// Pause between `Promote` re-issues after a `RaceTimeout`. The retry
/// loop is normally paced by mountd's own placeholder wait (~2 s per
/// rejection); this floor only matters if a daemon answers instantly
/// and keeps the loop from hammering the UDS.
const PROMOTE_RACE_RETRY_PAUSE: Duration = Duration::from_millis(100);

/// Per-attempt linear backoff plus 0–249 ms of clock-noise jitter (no
/// rand dependency needed; sub-ms wall-clock noise is independent
/// across callback threads, which is all the de-correlation required).
fn transient_fetch_backoff(attempt: u32) -> Duration {
    let jitter_ms = u64::from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_micros())
            .unwrap_or(0),
    ) % 250;
    TRANSIENT_FETCH_BACKOFF * attempt + Duration::from_millis(jitter_ms)
}

/// Why one `ReadBlob` attempt failed: `Transient` (Unavailable/Unknown
/// transport class) is retried by [`Opener::fetch_into`]; `Fatal` maps
/// straight to the FUSE errno.
enum StreamBlobError {
    Transient(tonic::Status),
    Fatal(Errno),
}

impl Opener {
    /// Wire the opener's clients, runtime handle, circuit breaker, and
    /// cache/staging directories. Called once per build by
    /// [`crate::castore_fuse::session::CastoreSession`] after the
    /// mountd handshake.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mountd: MountdClient,
        directory: DirectoryServiceClient<Channel>,
        chunk: ChunkServiceClient<Channel>,
        assignment_token: String,
        runtime: Handle,
        circuit: Arc<CircuitBreaker>,
        cache_dir: PathBuf,
        chunks_dir: PathBuf,
        staging_dir: PathBuf,
        cfg: OpenerConfig,
    ) -> Self {
        Self {
            mountd,
            directory,
            chunk,
            assignment_token,
            runtime,
            circuit,
            cache_dir,
            chunks_dir,
            staging_dir,
            cfg,
            modes: Mutex::new(IoModes::default()),
            fills: Mutex::new(HashMap::new()),
            active_fills: Arc::new(Mutex::new(HashMap::new())),
            open_files: Mutex::new(HashMap::new()),
            streams: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
        }
    }

    /// Shared backing-cache path for `digest`:
    /// `cache/{2-hex-prefix}/{64-hex}`.
    fn cache_path(&self, digest: &[u8; 32]) -> PathBuf {
        let hex = hex::encode(digest);
        self.cache_dir.join(&hex[..2]).join(&hex)
    }

    /// The `open(ino → file_digest)` decision per ADR §2.6, structured
    /// as a degrade ladder: passthrough off the node cache → userspace
    /// `KEEP_CACHE` read → fetch (whole-file or streaming). Expected
    /// failures at one tier (an evicted cache entry, an exhausted
    /// backing ceiling, an io-mode conflict) fall through to the next;
    /// only genuinely unexpected errors become `EIO`. Side effects
    /// (backing registration, fh table inserts, metrics) happen here;
    /// the ring dispatcher marshals the outcome into the wire reply.
    // r[impl builder.fs.digest-fuse-open]
    pub(super) fn open_inner(
        &self,
        file_digest: [u8; 32],
        size: u64,
    ) -> Result<OpenOutcome, Errno> {
        // Warm rungs first (shared with the ring fast tier), then the
        // network ladder.
        if let Some(outcome) = self.open_warm(file_digest)? {
            return Ok(outcome);
        }
        let started = std::time::Instant::now();
        let cache_path = self.cache_path(&file_digest);

        // Cache miss above the streaming threshold: reply after the
        // first chunk and fill in the background (§2.8).
        // r[impl builder.fs.streaming-open-threshold]
        if size > self.cfg.stream_threshold {
            return self.open_streaming(file_digest, size, started);
        }

        // Cache miss: fetch under the per-digest singleflight lock.
        let lock = {
            let mut fills = self.fills.lock().ignore_poison();
            Arc::clone(fills.entry(file_digest).or_default())
        };
        let contended = lock.try_lock().is_err();
        let _guard = lock.lock().ignore_poison();
        if let Some(file) = self.open_cache_entry(&cache_path)? {
            // The winner promoted while we waited on its lock (or on a
            // concurrent build's Promote).
            metrics::counter!("rio_builder_castore_fuse_open_case_total", "case" => "wait_fetching")
                .increment(1);
            let outcome = self.backed_outcome(&file_digest, file);
            record_open(started, "wait_fetching");
            return Ok(outcome);
        }
        // The histogram below shares this case so latency attribution
        // stays in lockstep with the open_case_total taxonomy.
        let case = if contended {
            // We waited but the winner failed — fall through and try
            // the fetch ourselves rather than propagating its error.
            "wait_fetching"
        } else {
            "miss_small"
        };
        metrics::counter!("rio_builder_castore_fuse_open_case_total", "case" => case).increment(1);

        let (fetched, staged_file) = self.fetch_and_promote(&file_digest, size)?;
        metrics::counter!("rio_builder_castore_fuse_fetch_bytes_total", "hit" => "remote")
            .increment(fetched);
        // The promoted entry is normally there for the passthrough
        // reply. If the LRU sweep already evicted it (disk pressure),
        // serve the verified bytes we just fetched through the
        // still-open staging fd instead of failing a healthy open.
        let outcome = match self.open_cache_entry(&cache_path)? {
            Some(file) => self.backed_outcome(&file_digest, file),
            None => {
                tracing::debug!(
                    digest = %hex::encode(file_digest),
                    "promoted cache entry already evicted; serving the staging copy"
                );
                self.keep_cache_outcome(&file_digest, staged_file)
            }
        };
        record_open(started, case);
        Ok(outcome)
    }

    /// The warm rungs of the open ladder — everything that can be
    /// answered from local state, with no network round-trip:
    /// reuse of a live passthrough backing, or a published node-cache
    /// entry. `Ok(None)` means a genuine cache miss, i.e. serving this
    /// open requires a fetch.
    ///
    /// This is the fuse-over-io_uring fast/slow boundary: the queue
    /// thread answers `Some` inline and punts `None` to the slow pool
    /// (which runs the full [`Opener::open_inner`] ladder — the warm
    /// rungs are re-checked there, so a promotion that races the punt
    /// is still served warm).
    pub(super) fn open_warm(&self, file_digest: [u8; 32]) -> Result<Option<OpenOutcome>, Errno> {
        let started = std::time::Instant::now();

        // A passthrough backing registered by an earlier open of this
        // digest is still live: reuse it. The kernel pins the backing
        // file for as long as the registration exists, so this serves
        // the open even after the LRU sweep unlinked the cache entry —
        // and it is the only io-mode-compatible reply while passthrough
        // opens of this inode are outstanding.
        // r[impl builder.fs.open-iomode-compatible]
        if let Some(outcome) = self.reuse_live_backing(&file_digest) {
            metrics::counter!("rio_builder_castore_fuse_open_case_total", "case" => "hit")
                .increment(1);
            record_open(started, "hit");
            return Ok(Some(outcome));
        }

        // Case (a): already in the shared node cache — another build
        // (or an earlier open in this one) fetched and promoted it.
        match self.open_cache_entry(&self.cache_path(&file_digest))? {
            Some(file) => {
                metrics::counter!("rio_builder_castore_fuse_open_case_total", "case" => "hit")
                    .increment(1);
                let outcome = self.backed_outcome(&file_digest, file);
                record_open(started, "hit");
                Ok(Some(outcome))
            }
            None => Ok(None),
        }
    }

    /// Open the digest's shared-cache entry. `Ok(None)` when the entry
    /// does not exist — including the case where mountd's LRU sweep
    /// unlinked it concurrently — which callers treat as an ordinary
    /// cache miss. Any other failure is a real error: the entry is
    /// there but unopenable, and neither a fetch nor a retry fixes a
    /// corrupted cache layout. `O_NOFOLLOW` is defense-in-depth: the
    /// cache parents are root-owned 0755, so only root could plant a
    /// symlink here — a leaf entry is never legitimately one.
    fn open_cache_entry(&self, cache_path: &Path) -> Result<Option<File>, Errno> {
        use std::os::unix::fs::OpenOptionsExt;
        match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(cache_path)
        {
            Ok(f) => Ok(Some(f)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => {
                tracing::warn!(path = %cache_path.display(), error = %e, "cache open failed");
                Err(Errno::EIO)
            }
        }
    }

    /// Reuse the digest's already-registered passthrough backing, if
    /// any: bump the refcount and reply passthrough with the same id
    /// (the kernel rejects a second registration for the inode anyway).
    fn reuse_live_backing(&self, digest: &[u8; 32]) -> Option<OpenOutcome> {
        if self.cfg.disable_passthrough {
            return None;
        }
        let mut modes = self.modes.lock().ignore_poison();
        let Some(DigestMode::Passthrough(b)) = modes.map.get_mut(digest) else {
            return None;
        };
        b.refcount += 1;
        let backing_id = b.id;
        drop(modes);
        Some(self.passthrough_outcome(backing_id))
    }

    /// §2.6 case (c): cache miss above the streaming threshold. Join
    /// (or start) the per-digest background fill and reply
    /// `FOPEN_KEEP_CACHE` as soon as the first chunk is readable;
    /// `read()` upcalls during the fill window are served from the
    /// shared `.partial` via [`FillState::read_at`].
    // r[impl builder.fs.streaming-open]
    fn open_streaming(
        &self,
        file_digest: [u8; 32],
        size: u64,
        started: std::time::Instant,
    ) -> Result<OpenOutcome, Errno> {
        let mut active = self.active_fills.lock().ignore_poison();
        if let Some(state) = active.get(&file_digest) {
            // A concurrent open of the same digest already owns the
            // fill; share its progress instead of double-fetching. No
            // circuit check: joining touches no remote — the owning
            // fill already reports its own outcome.
            let state = Arc::clone(state);
            drop(active);
            metrics::counter!("rio_builder_castore_fuse_open_case_total", "case" => "wait_fetching")
                .increment(1);
            return self.streaming_outcome(&file_digest, state, started, "wait_fetching");
        }

        let hexd = hex::encode(file_digest);
        let partial_path = self.staging_dir.join(format!("{hexd}.partial"));
        let partial = match create_partial(&partial_path) {
            Ok(f) => f,
            Err(errno) => {
                drop(active);
                return Err(errno);
            }
        };
        // The circuit gate sits immediately before the fill is
        // committed: a check() that claims the half-open probe must
        // always be followed by a record(), and the spawned fill's
        // guard guarantees exactly that. Checking earlier would let a
        // local failure (e.g. the staging create above) leak the probe
        // claim and wedge the breaker for the life of the process.
        // r[impl builder.fs.fetch-circuit]
        if let Err(errno) = self.circuit.check() {
            drop(active);
            let _ = std::fs::remove_file(&partial_path);
            return Err(errno);
        }
        let state = Arc::new(FillState::new(partial, size));
        active.insert(file_digest, Arc::clone(&state));
        drop(active);
        metrics::counter!("rio_builder_castore_fuse_open_case_total", "case" => "miss_stream")
            .increment(1);

        let ctx = FillCtx {
            digest: file_digest,
            size,
            directory: self.directory.clone(),
            chunk: self.chunk.clone(),
            assignment_token: self.assignment_token.clone(),
            mountd: self.mountd.clone(),
            circuit: Arc::clone(&self.circuit),
            chunks_dir: self.chunks_dir.clone(),
            staging_dir: self.staging_dir.clone(),
            fetch_timeout: self.cfg.fetch_timeout,
            mountd_request_timeout: self.cfg.mountd_request_timeout,
        };
        // TODO: the fill's cleanup guard is constructed inside the spawned
        // task, so a task dropped before its first poll (runtime shutdown
        // racing this open) skips the cleanup and this open parks until the
        // process exits; constructing the guard here and moving it into the
        // future would close that, at the cost of threading it through
        // run_fill and every test call site.
        self.runtime.spawn(super::stream::run_fill(
            ctx,
            Arc::clone(&state),
            Arc::clone(&self.active_fills),
        ));
        self.streaming_outcome(&file_digest, state, started, "miss_stream")
    }

    /// Block until `state`'s first chunk is readable, then hand the
    /// kernel a userspace-read handle over the shared `.partial` — or,
    /// if a passthrough backing for the digest went live while we
    /// waited, reuse it (see [`Opener::note_caching_handle`]).
    fn streaming_outcome(
        &self,
        digest: &[u8; 32],
        state: Arc<FillState>,
        started: std::time::Instant,
        case: &'static str,
    ) -> Result<OpenOutcome, Errno> {
        state.wait_first_chunk()?;
        let outcome = match self.note_caching_handle(digest) {
            Some(backing_id) => self.passthrough_outcome(backing_id),
            None => {
                let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
                self.streams.lock().ignore_poison().insert(fh, state);
                // KEEP_CACHE does not suppress cold-page upcalls (each
                // still reaches read_at during the fill window); it only
                // preserves already-cached pages across opens, so the
                // post-fill steady state is zero-upcall once the opens go
                // passthrough.
                metrics::counter!("rio_builder_castore_fuse_open_mode_total", "mode" => "keep_cache")
                    .increment(1);
                OpenOutcome::KeepCache { fh }
            }
        };
        record_open(started, case);
        Ok(outcome)
    }

    /// Serve an open from `file` (the digest's verified bytes):
    /// passthrough when possible, degrading to a userspace
    /// `FOPEN_KEEP_CACHE` read when passthrough is unavailable for this
    /// open — the escape hatch, live caching handles of the same inode
    /// (kernel io-mode exclusivity), the backing-id ceiling, or a
    /// failed registration. Never fails: the fallback only needs the
    /// already-open file.
    // r[impl builder.fs.passthrough-on-hit]
    // r[impl builder.fs.shared-backing-cache]
    fn backed_outcome(&self, digest: &[u8; 32], file: File) -> OpenOutcome {
        if self.cfg.disable_passthrough {
            return self.keep_cache_outcome(digest, file);
        }

        // The whole mode decision happens under the one io-mode lock —
        // the live-handle check, the ceiling check, the BackingOpen
        // round-trip (a synchronous UDS call, no await), and the
        // registration. Splitting any of these onto a separate lock
        // reopens the window where a concurrent open of the same digest
        // picks the other mode mid-decision and the kernel rejects the
        // mix on a healthy file.
        // r[impl builder.fs.open-iomode-compatible]
        let mut modes = self.modes.lock().ignore_poison();
        match modes.map.get_mut(digest) {
            Some(DigestMode::Caching(n)) => {
                *n += 1;
                drop(modes);
                tracing::debug!(
                    digest = %hex::encode(digest),
                    "live KEEP_CACHE handles; staying in caching mode until they drain"
                );
                return self.caching_fh(file);
            }
            Some(DigestMode::Passthrough(b)) => {
                b.refcount += 1;
                let backing_id = b.id;
                drop(modes);
                return self.passthrough_outcome(backing_id);
            }
            None => {}
        }
        if modes.backings >= self.cfg.max_backing_ids {
            // Degrade, don't fail: the ceiling exists to bound mountd's
            // IDR growth, and the open is perfectly servable from the
            // file we already hold.
            modes.map.insert(*digest, DigestMode::Caching(1));
            drop(modes);
            tracing::warn!(
                max = self.cfg.max_backing_ids,
                "concurrent passthrough-open ceiling reached; falling back to userspace read"
            );
            return self.caching_fh(file);
        }
        match self
            .mountd
            .backing_open(file.as_raw_fd(), self.cfg.mountd_request_timeout)
        {
            Ok(id) => {
                modes.map.insert(
                    *digest,
                    DigestMode::Passthrough(BackingRef { id, refcount: 1 }),
                );
                modes.backings += 1;
                drop(modes);
                self.passthrough_outcome(id)
            }
            Err(e) => {
                modes.map.insert(*digest, DigestMode::Caching(1));
                drop(modes);
                tracing::warn!(error = %e, "BackingOpen failed; falling back to userspace read");
                self.caching_fh(file)
            }
        }
    }

    /// The userspace-read tier of the ladder: hand the kernel a
    /// `FOPEN_KEEP_CACHE` handle whose `read()` upcalls are served from
    /// `file` — unless a passthrough backing went live since the
    /// caller's cache check (see [`Opener::note_caching_handle`]).
    fn keep_cache_outcome(&self, digest: &[u8; 32], file: File) -> OpenOutcome {
        match self.note_caching_handle(digest) {
            Some(backing_id) => self.passthrough_outcome(backing_id),
            None => self.caching_fh(file),
        }
    }

    /// Count one new caching-mode (`FOPEN_KEEP_CACHE`) handle of
    /// `digest` under the io-mode lock — or, if a passthrough backing
    /// is already live for the digest, bump its refcount and return the
    /// backing id so the caller replies passthrough instead: a caching
    /// reply at that point is exactly the mixed-io-mode open the kernel
    /// rejects.
    // r[impl builder.fs.open-iomode-compatible]
    fn note_caching_handle(&self, digest: &[u8; 32]) -> Option<u32> {
        let mut modes = self.modes.lock().ignore_poison();
        match modes.map.entry(*digest).or_insert(DigestMode::Caching(0)) {
            DigestMode::Caching(n) => {
                *n += 1;
                None
            }
            DigestMode::Passthrough(b) => {
                b.refcount += 1;
                Some(b.id)
            }
        }
    }

    /// Register `file` as the userspace-read source behind a fresh fh
    /// and reply `FOPEN_KEEP_CACHE`. The caller has already counted the
    /// caching handle in [`Opener::modes`]. Correct but pays one upcall
    /// per 128 KiB read.
    fn caching_fh(&self, file: File) -> OpenOutcome {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.open_files.lock().ignore_poison().insert(fh, file);
        metrics::counter!("rio_builder_castore_fuse_open_mode_total", "mode" => "keep_cache")
            .increment(1);
        OpenOutcome::KeepCache { fh }
    }

    /// Hand out one more passthrough handle over an already-registered
    /// backing id. The caller has already counted the reference in
    /// [`Opener::modes`].
    fn passthrough_outcome(&self, backing_id: u32) -> OpenOutcome {
        metrics::counter!("rio_builder_castore_fuse_open_mode_total", "mode" => "passthrough")
            .increment(1);
        OpenOutcome::Passthrough {
            fh: self.next_fh.fetch_add(1, Ordering::Relaxed),
            backing_id,
        }
    }

    /// Fetch `digest` into staging, verify, and `Promote` it into the
    /// shared cache. On return the cache path normally exists (or an
    /// `Errno` says why not). Returns the byte count fetched from the
    /// store and the still-open handle to the verified staging copy, so
    /// the caller can serve the open even if the cache entry is evicted
    /// before it gets to open it.
    // r[impl builder.fs.file-digest-integrity]
    fn fetch_and_promote(&self, digest: &[u8; 32], size: u64) -> Result<(u64, File), Errno> {
        // r[impl builder.fs.fetch-circuit]
        self.circuit.check()?;

        let hexd = hex::encode(digest);
        let partial = self.staging_dir.join(format!("{hexd}.partial"));
        let staged = self.staging_dir.join(&hexd);

        let result = self.fetch_into(digest, size, &partial, &staged);
        self.circuit.record(result.is_ok());
        let (written, staged_file) = result?;

        // Promote: mountd re-hashes during the copy into the cache it
        // owns. A concurrent build promoting the same digest can win
        // the `.promoting` placeholder; mountd then answers RaceTimeout
        // when the winner is still copying after its bounded wait. The
        // protocol classifies that as retryable (`ErrKind::is_build_fatal`
        // is false): keep waiting for the winner — re-checking the cache
        // and re-issuing the Promote, which short-circuits once the
        // entry is published — and give up only when the promote budget
        // is exhausted. Each re-issue blocks in mountd's own placeholder
        // wait, so the loop is server-paced, not a spin.
        let timeout = jit_fetch_timeout(self.cfg.mountd_request_timeout, size);
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.mountd.promote(*digest, timeout) {
                Ok(()) => break Ok((written, staged_file)),
                Err(MountdError::Rejected(ErrKind::RaceTimeout)) => {
                    if self.cache_path(digest).exists() {
                        // The winner published while we waited.
                        break Ok((written, staged_file));
                    }
                    if std::time::Instant::now() >= deadline {
                        tracing::warn!(
                            digest = %hexd,
                            ?timeout,
                            "Promote race never resolved within the promote budget"
                        );
                        break Err(Errno::EIO);
                    }
                    std::thread::sleep(PROMOTE_RACE_RETRY_PAUSE);
                }
                Err(e) => {
                    tracing::warn!(digest = %hexd, error = %e, "Promote failed");
                    break Err(Errno::EIO);
                }
            }
        }
    }

    /// Stream `ReadBlob(digest)` into `partial`, verify the whole-file
    /// blake3, and rename to `staged`. Returns the byte count actually
    /// transferred and the (still-open) staged file. Split from
    /// [`Opener::fetch_and_promote`] so the circuit breaker records
    /// exactly the store-fetch outcome, not the Promote outcome.
    ///
    /// Sits below the circuit-breaker check in `fetch_and_promote` and
    /// retries ONLY Unavailable/Unknown-class transport errors, a
    /// bounded number of times: a rio-store rolling restart mid-build
    /// must not EIO a long build's cold `open()`s for the few seconds
    /// the LB needs to converge on the replacement pods. Everything
    /// else (NotFound, integrity mismatch, idle-stream timeout) fails
    /// immediately — retrying those repeats the same answer and only
    /// delays the breaker.
    fn fetch_into(
        &self,
        digest: &[u8; 32],
        size: u64,
        partial: &Path,
        staged: &Path,
    ) -> Result<(u64, File), Errno> {
        let mut file = create_partial(partial)?;
        let timeout = jit_fetch_timeout(self.cfg.fetch_timeout, size);

        let mut attempt = 1;
        let verified = loop {
            match self.stream_blob(digest, size, timeout, &mut file) {
                Err(StreamBlobError::Transient(status)) if attempt < TRANSIENT_FETCH_ATTEMPTS => {
                    tracing::warn!(
                        digest = %hex::encode(digest),
                        %status,
                        attempt,
                        "ReadBlob transient transport error; retrying"
                    );
                    // Reset the partial so the next attempt re-streams
                    // from a clean slate.
                    if let Err(e) = file.set_len(0).and_then(|()| file.rewind()) {
                        tracing::warn!(error = %e, "staging reset for retry failed");
                        break Err(Errno::EIO);
                    }
                    std::thread::sleep(transient_fetch_backoff(attempt));
                    attempt += 1;
                }
                Err(StreamBlobError::Transient(status)) => {
                    tracing::warn!(digest = %hex::encode(digest), %status, "ReadBlob failed");
                    break Err(Errno::EIO);
                }
                Err(StreamBlobError::Fatal(errno)) => break Err(errno),
                Ok((got, _)) if got.as_bytes() != digest => {
                    metrics::counter!("rio_builder_castore_fuse_integrity_fail_total").increment(1);
                    tracing::error!(
                        want = %hex::encode(digest),
                        got = %got.to_hex(),
                        "ReadBlob content does not match file_digest"
                    );
                    break Err(Errno::EIO);
                }
                Ok((_, written)) => {
                    break file
                        .flush()
                        .and_then(|()| std::fs::rename(partial, staged))
                        .map(|()| written)
                        .map_err(|e| {
                            tracing::warn!(error = %e, "staging finalize failed");
                            Errno::EIO
                        });
                }
            }
        };
        match verified {
            Ok(written) => Ok((written, file)),
            Err(errno) => {
                let _ = std::fs::remove_file(partial);
                Err(errno)
            }
        }
    }

    /// One streamed `ReadBlob` attempt: hash and append every chunk to
    /// `file`, refusing to read past the expected `size` (a stream that
    /// overruns the DAG-recorded size cannot hash to `digest`, so
    /// consuming it would only burn staging quota and fuser-thread time
    /// before the same rejection). Classifies failures so
    /// [`Opener::fetch_into`] retries only the transient transport
    /// class.
    fn stream_blob(
        &self,
        digest: &[u8; 32],
        size: u64,
        timeout: Duration,
        file: &mut File,
    ) -> Result<(blake3::Hash, u64), StreamBlobError> {
        let mut client = self.directory.clone();
        let mut req = tonic::Request::new(ReadBlobRequest {
            file_digest: digest.to_vec(),
        });
        attach_token(&mut req, &self.assignment_token).map_err(StreamBlobError::Fatal)?;
        let fetched = self.runtime.block_on(async {
            tokio::time::timeout(timeout, async {
                // streaming-open-ban: bound the open itself; the outer
                // tokio::time::timeout still bounds open+consume at the
                // same `timeout`, so the inner TimedOut arm is dominated
                // and serves as the structural witness.
                let open = rio_common::transport::bounded_open(
                    std::future::pending(),
                    timeout,
                    client.read_blob(req),
                )
                .await;
                let mut stream = match open {
                    rio_common::transport::OpenOutcome::Opened(r) => r?.into_inner(),
                    rio_common::transport::OpenOutcome::TimedOut { .. }
                    | rio_common::transport::OpenOutcome::Aborted => {
                        return Err(tonic::Status::deadline_exceeded("ReadBlob open timed out"));
                    }
                };
                let mut hasher = blake3::Hasher::new();
                let mut written: u64 = 0;
                while let Some(chunk) = stream.message().await? {
                    if written + chunk.data.len() as u64 > size {
                        return Err(tonic::Status::out_of_range(
                            "ReadBlob overran the expected file size",
                        ));
                    }
                    hasher.update(&chunk.data);
                    // Sync write from inside block_on: this thread is a
                    // fuser callback thread, already a blocking context
                    // — same contract as the legacy fetch path.
                    file.write_all(&chunk.data)
                        .map_err(|e| tonic::Status::internal(format!("staging write: {e}")))?;
                    written += chunk.data.len() as u64;
                }
                Ok::<_, tonic::Status>((hasher.finalize(), written))
            })
            .await
        });

        match fetched {
            Err(_elapsed) => {
                tracing::warn!(digest = %hex::encode(digest), ?timeout, "ReadBlob timed out");
                Err(StreamBlobError::Fatal(Errno::EIO))
            }
            Ok(Err(status))
                if matches!(
                    status.code(),
                    tonic::Code::Unavailable | tonic::Code::Unknown
                ) =>
            {
                Err(StreamBlobError::Transient(status))
            }
            Ok(Err(status)) => {
                tracing::warn!(digest = %hex::encode(digest), %status, "ReadBlob failed");
                Err(StreamBlobError::Fatal(Errno::EIO))
            }
            Ok(Ok(pair)) => Ok(pair),
        }
    }

    /// Userspace `read()`. Reachable when an open degraded to a
    /// userspace read (any tier of `Opener::backed_outcome`'s ladder)
    /// or while a streaming open is inside its fill window.
    pub fn read(&self, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        if let Some(result) = self.read_fast(fh, offset, size) {
            return result;
        }
        // Clone out of the map before blocking on the fill's high-water
        // mark — holding the lock across the wait would stall every
        // other read and open. A release racing in between is an
        // ordinary EBADF.
        let stream = self.streams.lock().ignore_poison().get(&fh).cloned();
        match stream {
            Some(state) => state.read_at(offset, size),
            None => Err(Errno::EBADF),
        }
    }

    /// The non-blocking half of [`Opener::read`]: serve `fh` from its
    /// already-open fallback file (a local `pread`), or report `None`
    /// when the handle is a streaming fill — whose `read_at` blocks on
    /// the fill's high-water mark and therefore belongs on the slow
    /// pool, never on a fuse-over-io_uring queue thread.
    pub(super) fn read_fast(
        &self,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> Option<Result<Vec<u8>, Errno>> {
        use std::os::unix::fs::FileExt;
        {
            let files = self.open_files.lock().ignore_poison();
            if let Some(file) = files.get(&fh) {
                let mut buf = vec![0u8; size as usize];
                return Some(match file.read_at(&mut buf, offset) {
                    Ok(n) => {
                        buf.truncate(n);
                        Ok(buf)
                    }
                    Err(e) => {
                        tracing::warn!(fh, offset, error = %e, "fallback read failed");
                        Err(Errno::EIO)
                    }
                });
            }
        }
        if self.streams.lock().ignore_poison().contains_key(&fh) {
            return None;
        }
        Some(Err(Errno::EBADF))
    }

    /// `release(fh)`: drop the userspace-read handle (fallback file or
    /// in-flight stream), decrementing the digest's live caching-handle
    /// count, or decrement the digest's backing refcount, sending
    /// `BackingClose` at zero. The kernel holds its own reference on
    /// the backing file for the open's lifetime, so the close only
    /// frees mountd's IDR slot — in-flight reads are unaffected.
    pub fn release(&self, file_digest: &[u8; 32], fh: u64) {
        if self.open_files.lock().ignore_poison().remove(&fh).is_some() {
            self.drop_caching_handle(file_digest);
            return;
        }
        // Dropping a streaming handle does not cancel the fill — the
        // task runs to completion so the file still lands in the node
        // cache for the next open.
        if self.streams.lock().ignore_poison().remove(&fh).is_some() {
            self.drop_caching_handle(file_digest);
            return;
        }
        let close = {
            let mut modes = self.modes.lock().ignore_poison();
            // No live passthrough registration = no matching open (a
            // passthrough open whose BackingOpen failed fell back to
            // open_files and was handled above). Nothing to close.
            let mut close = None;
            match modes.map.get_mut(file_digest) {
                Some(DigestMode::Passthrough(b)) if b.refcount > 1 => b.refcount -= 1,
                Some(DigestMode::Passthrough(b)) => close = Some(b.id),
                _ => {}
            }
            if close.is_some() {
                modes.map.remove(file_digest);
                modes.backings -= 1;
            }
            close
        };
        if let Some(id) = close
            && let Err(e) = self
                .mountd
                .backing_close(id, self.cfg.mountd_request_timeout)
        {
            // Best-effort: the IDR slot leaks until unmount. Bounded by
            // max_backing_ids.
            tracing::debug!(backing_id = id, error = %e, "BackingClose failed");
        }
    }

    /// One caching-mode handle of `digest` was released.
    fn drop_caching_handle(&self, digest: &[u8; 32]) {
        let mut modes = self.modes.lock().ignore_poison();
        if let Some(DigestMode::Caching(n)) = modes.map.get_mut(digest) {
            *n -= 1;
            if *n == 0 {
                modes.map.remove(digest);
            }
        }
    }
}

/// Attach the build's assignment token to a castore data-path request.
/// Same header and helper as the upload path; the only difference is
/// the error vocabulary — a FUSE callback can only answer `Errno`, so
/// the (practically unreachable) non-ASCII-token failure becomes `EIO`.
pub(super) fn attach_token<T>(req: &mut tonic::Request<T>, token: &str) -> Result<(), Errno> {
    crate::upload::common::attach_assignment_token(req, token).map_err(|status| {
        tracing::error!(%status, "assignment token rejected by metadata encoding");
        Errno::EIO
    })
}

/// `O_RDWR|O_CREAT|O_EXCL`, mode 0600. Read access is for the
/// streaming path's `read()` upcalls against the in-progress fill;
/// mountd opens the staged file by path as root for `Promote`.
fn exclusive_create(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Exclusively create a staging `.partial`. A leftover entry means a
/// previous attempt in this process died mid-fetch (fuser catches
/// callback panics); the per-digest singleflight guarantees no live
/// writer, so unlink and retry once.
pub(super) fn create_partial(partial: &Path) -> Result<File, Errno> {
    match exclusive_create(partial) {
        Ok(f) => Ok(f),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(partial);
            exclusive_create(partial).map_err(|e| {
                tracing::warn!(path = %partial.display(), error = %e, "staging create failed");
                Errno::EIO
            })
        }
        Err(e) => {
            tracing::warn!(path = %partial.display(), error = %e, "staging create failed");
            Err(Errno::EIO)
        }
    }
}

/// `case` mirrors the `open_case_total` increment on the same path, so
/// the latency histogram and the decision counter cannot disagree about
/// what kind of open this was. The old `{hit, streamed}` labels are
/// derivable: hit/wait_fetching served node-local, miss_small fetched
/// whole-file, miss_stream replied at the first chunk of a fill.
fn record_open(started: std::time::Instant, case: &'static str) {
    metrics::histogram!(
        "rio_builder_castore_fuse_open_seconds",
        "case" => case,
    )
    .record(started.elapsed().as_secs_f64());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::castore_fuse::circuit::SystemClock;
    use crate::castore_fuse::mountd_proto::{ErrKind, Req, Resp};
    use crate::castore_fuse::testing::{FakeDirectory, RecordingMountd};
    use rio_proto::DirectoryServiceServer;
    use rio_test_support::grpc::spawn_grpc_server;
    use std::sync::atomic::{AtomicU32, AtomicUsize};

    /// Mountd reply policy: how the scripted daemon answers each request.
    type ReplyPolicy = Box<dyn Fn(&Req) -> Resp + Send + 'static>;

    /// Grants sequential backing ids and accepts everything else, but
    /// never copies anything into the cache — tests that need the cache
    /// entry published write it themselves, playing the mountd role at
    /// the moment they choose.
    fn granting(_root: &Path) -> ReplyPolicy {
        let next_id = AtomicU32::new(1);
        Box::new(move |req| match req {
            Req::BackingOpen => Resp::BackingId(next_id.fetch_add(1, Ordering::Relaxed)),
            _ => Resp::Ok,
        })
    }

    /// Like [`granting`], but emulates the real daemon's verify-copy: a
    /// `Promote` copies `staging/<hex>` into `cache/<ab>/<hex>` before
    /// replying Ok.
    fn granting_and_promoting(root: &Path) -> ReplyPolicy {
        let staging = root.join("staging");
        let cache = root.join("cache");
        let next_id = AtomicU32::new(1);
        Box::new(move |req| match req {
            Req::BackingOpen => Resp::BackingId(next_id.fetch_add(1, Ordering::Relaxed)),
            Req::Promote { digest } => {
                let hex = hex::encode(digest);
                let dst_dir = cache.join(&hex[..2]);
                std::fs::create_dir_all(&dst_dir).unwrap();
                std::fs::copy(staging.join(&hex), dst_dir.join(&hex)).unwrap();
                Resp::Ok
            }
            _ => Resp::Ok,
        })
    }

    /// Everything an `open_inner` test needs: tempdir cache/staging
    /// layout, a scripted mountd, a canned DirectoryService, and an
    /// `Opener` wired to them. Tests run on a plain (non-tokio) thread —
    /// the same situation as a fuser callback thread — and `open_inner`
    /// bridges into the harness-owned runtime via `Handle::block_on`.
    struct OpenHarness {
        tmp: tempfile::TempDir,
        mountd: RecordingMountd,
        opener: Opener,
        circuit: Arc<CircuitBreaker>,
        _rt: tokio::runtime::Runtime,
    }

    impl OpenHarness {
        fn new(
            dir: FakeDirectory,
            circuit: Arc<CircuitBreaker>,
            tweak: impl FnOnce(&mut OpenerConfig),
            policy_for: impl FnOnce(&Path) -> ReplyPolicy,
        ) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            for d in ["cache", "chunks", "staging", "staging/chunks"] {
                std::fs::create_dir_all(tmp.path().join(d)).unwrap();
            }
            let policy = policy_for(tmp.path());
            let (mountd, mountd_client) =
                RecordingMountd::spawn_with(&tmp.path().join("mountd.sock"), policy);
            let rt = tokio::runtime::Runtime::new().unwrap();
            let (addr, _server) = rt.block_on(async {
                let router = tonic::transport::Server::builder()
                    .add_service(DirectoryServiceServer::new(dir));
                spawn_grpc_server(router).await
            });
            // connect_lazy spawns the channel's background task — it
            // needs the harness runtime entered, like everything else
            // tonic does here.
            let channel = {
                let _rt_context = rt.enter();
                tonic::transport::Channel::from_shared(format!("http://{addr}"))
                    .unwrap()
                    .connect_lazy()
            };
            let mut cfg = OpenerConfig {
                // Small enough that streaming-path tests don't need
                // multi-MiB fixtures.
                stream_threshold: 8 * 1024,
                mountd_request_timeout: Duration::from_millis(500),
                fetch_timeout: Duration::from_secs(5),
                max_backing_ids: 16,
                disable_passthrough: false,
            };
            tweak(&mut cfg);
            let opener = Opener::new(
                mountd_client,
                DirectoryServiceClient::new(channel.clone()),
                ChunkServiceClient::new(channel),
                "test-token".to_owned(),
                rt.handle().clone(),
                Arc::clone(&circuit),
                tmp.path().join("cache"),
                tmp.path().join("chunks"),
                tmp.path().join("staging"),
                cfg,
            );
            Self {
                tmp,
                mountd,
                opener,
                circuit,
                _rt: rt,
            }
        }

        /// Store serves `blob` whole-file (no chunk window), mountd
        /// grants backing ids and accepts (but does not act on)
        /// Promotes.
        fn with_blob(blob: Vec<u8>) -> Self {
            Self::new(
                FakeDirectory::new(Err(tonic::Code::FailedPrecondition), blob),
                Arc::new(CircuitBreaker::default()),
                |_| {},
                granting,
            )
        }

        fn cache_file(&self, digest: &[u8; 32]) -> PathBuf {
            let hex = hex::encode(digest);
            self.tmp.path().join("cache").join(&hex[..2]).join(&hex)
        }

        /// Publish `content` into the node cache the way mountd's
        /// verify-copy would, returning its digest.
        fn seed_cache(&self, content: &[u8]) -> [u8; 32] {
            let digest = *blake3::hash(content).as_bytes();
            let path = self.cache_file(&digest);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
            digest
        }

        fn backing_open_count(&self) -> usize {
            self.mountd
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| matches!(r, Req::BackingOpen))
                .count()
        }

        fn backing_close_count(&self) -> usize {
            self.mountd
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| matches!(r, Req::BackingClose { .. }))
                .count()
        }
    }

    fn fh_of(outcome: &OpenOutcome) -> u64 {
        match outcome {
            OpenOutcome::Passthrough { fh, .. } | OpenOutcome::KeepCache { fh } => *fh,
        }
    }

    // ── The degrade ladder: passthrough → userspace read → fetch ──────

    /// The normal cold small-file path end-to-end: miss → ReadBlob →
    /// verify → Promote (the scripted daemon publishes the entry) →
    /// passthrough off the published cache file.
    // r[verify builder.fs.digest-fuse-open]
    // r[verify builder.fs.passthrough-on-hit]
    #[test]
    fn cold_small_file_fetches_promotes_and_goes_passthrough() {
        let content = vec![0x42u8; 4096];
        let digest = *blake3::hash(&content).as_bytes();
        let h = OpenHarness::new(
            FakeDirectory::new(Err(tonic::Code::FailedPrecondition), content.clone()),
            Arc::new(CircuitBreaker::default()),
            |_| {},
            granting_and_promoting,
        );

        let outcome = h.opener.open_inner(digest, content.len() as u64).unwrap();
        assert!(
            matches!(outcome, OpenOutcome::Passthrough { .. }),
            "published cache entry must be served passthrough, got {outcome:?}"
        );
        assert!(
            h.mountd.saw_promote(&digest),
            "the fetched file is promoted"
        );
        assert_eq!(
            std::fs::read(h.cache_file(&digest)).unwrap(),
            content,
            "the daemon's copy is what the backing fd points at"
        );
    }

    /// A cache entry that vanishes between the Promote and the cache
    /// open (mountd's LRU sweep evicting under disk pressure) is NOT a
    /// build-fatal error: the opener already holds the verified bytes
    /// it just fetched and must serve them, degrading to a userspace
    /// read instead of replying EIO.
    #[test]
    fn missing_cache_entry_after_promote_serves_the_verified_bytes() {
        let content = vec![0x17u8; 4096];
        let digest = *blake3::hash(&content).as_bytes();
        // `granting` accepts the Promote but never publishes the cache
        // entry — the same thing a post-promote eviction looks like to
        // the opener.
        let h = OpenHarness::with_blob(content.clone());

        let outcome = h
            .opener
            .open_inner(digest, content.len() as u64)
            .expect("an evicted cache entry must not fail an open that holds verified bytes");
        assert!(
            matches!(outcome, OpenOutcome::KeepCache { .. }),
            "without a cache entry the open degrades to a userspace read, got {outcome:?}"
        );
        assert_eq!(
            h.opener
                .read(fh_of(&outcome), 0, content.len() as u32)
                .unwrap(),
            content,
            "the degraded handle serves the verified fetched bytes"
        );
        assert!(h.mountd.saw_promote(&digest));
    }

    /// Only ENOENT is a miss. Any other failure to open the cache entry
    /// (here: the mountd-owned prefix directory replaced by a file, so
    /// the open fails ENOTDIR) stays a hard error — re-fetching cannot
    /// fix a corrupted cache layout and silently serving from staging
    /// would mask it.
    #[test]
    fn unreadable_cache_entry_is_still_a_hard_error() {
        let content = vec![0x33u8; 1024];
        let digest = *blake3::hash(&content).as_bytes();
        let h = OpenHarness::with_blob(content.clone());
        // Replace the {ab} prefix directory with a regular file.
        let prefix = h.cache_file(&digest);
        let prefix = prefix.parent().unwrap();
        std::fs::write(prefix, b"not a directory").unwrap();

        let err = h
            .opener
            .open_inner(digest, content.len() as u64)
            .expect_err("a corrupted cache layout must fail loudly");
        assert_eq!(err.code(), Errno::EIO.code());
    }

    /// At the passthrough-backing ceiling the open must degrade to the
    /// userspace-read tier, not fail the build with EMFILE: the ceiling
    /// exists to bound mountd's IDR growth, and the file is perfectly
    /// servable from the cache entry we already opened.
    #[test]
    fn backing_ceiling_degrades_to_userspace_read() {
        let h = OpenHarness::new(
            FakeDirectory::new(Err(tonic::Code::FailedPrecondition), Vec::new()),
            Arc::new(CircuitBreaker::default()),
            |cfg| cfg.max_backing_ids = 1,
            granting,
        );
        let first = vec![0xAAu8; 512];
        let second = vec![0xBBu8; 512];
        let d1 = h.seed_cache(&first);
        let d2 = h.seed_cache(&second);

        let o1 = h.opener.open_inner(d1, first.len() as u64).unwrap();
        assert!(matches!(o1, OpenOutcome::Passthrough { .. }));

        let o2 = h
            .opener
            .open_inner(d2, second.len() as u64)
            .expect("the ceiling must degrade the open, not fail it");
        assert!(
            matches!(o2, OpenOutcome::KeepCache { .. }),
            "above the ceiling the open serves userspace reads, got {o2:?}"
        );
        assert_eq!(
            h.opener.read(fh_of(&o2), 0, second.len() as u32).unwrap(),
            second
        );
        assert_eq!(
            h.backing_open_count(),
            1,
            "no BackingOpen is attempted once the ceiling is reached"
        );
    }

    // ── Promote RaceTimeout is retryable, not build-fatal ─────────────

    /// `Rejected(RaceTimeout)` means a concurrent build's promote of the
    /// same digest is still copying. The opener must keep waiting for
    /// the winner (re-issuing the Promote) instead of failing the open
    /// after a single instantaneous cache check.
    #[test]
    fn promote_race_timeout_retries_until_the_winner_publishes() {
        let content = vec![0x55u8; 4096];
        let digest = *blake3::hash(&content).as_bytes();
        let h = OpenHarness::new(
            FakeDirectory::new(Err(tonic::Code::FailedPrecondition), content.clone()),
            Arc::new(CircuitBreaker::default()),
            |_| {},
            |_root| {
                let promote_calls = AtomicU32::new(0);
                Box::new(move |req| match req {
                    Req::Promote { .. } => {
                        if promote_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                            Resp::Err(ErrKind::RaceTimeout)
                        } else {
                            Resp::Ok
                        }
                    }
                    Req::BackingOpen => Resp::BackingId(1),
                    _ => Resp::Ok,
                })
            },
        );

        let outcome = h
            .opener
            .open_inner(digest, content.len() as u64)
            .expect("a transient promote race must not fail the open");
        assert_eq!(
            h.opener
                .read(fh_of(&outcome), 0, content.len() as u32)
                .unwrap(),
            content
        );
        assert!(
            h.mountd.promote_count(&digest) >= 2,
            "the Promote is re-issued after RaceTimeout, saw {}",
            h.mountd.promote_count(&digest)
        );
    }

    /// The winner publishing the entry while we wait resolves the race
    /// without another Promote round-trip.
    #[test]
    fn promote_race_timeout_resolves_when_the_winner_publishes() {
        let content = vec![0x66u8; 4096];
        let digest = *blake3::hash(&content).as_bytes();
        let content_for_daemon = content.clone();
        let h = OpenHarness::new(
            FakeDirectory::new(Err(tonic::Code::FailedPrecondition), content.clone()),
            Arc::new(CircuitBreaker::default()),
            |_| {},
            move |root| {
                let cache = root.join("cache");
                Box::new(move |req| match req {
                    // The "winner" (a concurrent build) publishes the
                    // entry just as our promote times out on the race.
                    Req::Promote { digest } => {
                        let hex = hex::encode(digest);
                        let dir = cache.join(&hex[..2]);
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join(&hex), &content_for_daemon).unwrap();
                        Resp::Err(ErrKind::RaceTimeout)
                    }
                    Req::BackingOpen => Resp::BackingId(1),
                    _ => Resp::Ok,
                })
            },
        );

        let outcome = h.opener.open_inner(digest, content.len() as u64).unwrap();
        assert!(
            matches!(outcome, OpenOutcome::Passthrough { .. }),
            "the winner's published entry serves the open, got {outcome:?}"
        );
    }

    /// A RaceTimeout whose winner never publishes (it crashed, or the
    /// daemon is stuck) must still give up within the promote budget —
    /// the retry is bounded, not forever.
    #[test]
    fn promote_race_timeout_eventually_gives_up() {
        let content = vec![0x77u8; 2048];
        let digest = *blake3::hash(&content).as_bytes();
        let h = OpenHarness::new(
            FakeDirectory::new(Err(tonic::Code::FailedPrecondition), content.clone()),
            Arc::new(CircuitBreaker::default()),
            // The promote budget equals mountd_request_timeout for a file
            // this small, and the `>= 2` assertion below needs at least two
            // Promote round-trips with 100 ms pauses to fit inside it. The
            // harness default (500 ms) leaves little slack under heavy
            // nextest parallelism; 3 s keeps the give-up bound observable
            // while giving the first round-trip generous headroom.
            |cfg| cfg.mountd_request_timeout = Duration::from_secs(3),
            |_root| {
                Box::new(|req| match req {
                    Req::Promote { .. } => Resp::Err(ErrKind::RaceTimeout),
                    Req::BackingOpen => Resp::BackingId(1),
                    _ => Resp::Ok,
                })
            },
        );

        let err = h
            .opener
            .open_inner(digest, content.len() as u64)
            .expect_err("a never-resolving race must eventually fail the open");
        assert_eq!(err.code(), Errno::EIO.code());
        assert!(
            h.mountd.promote_count(&digest) >= 2,
            "the race is retried before giving up, saw {}",
            h.mountd.promote_count(&digest)
        );
    }

    // ── Circuit breaker probe hygiene ──────────────────────────────────

    /// A streaming open that claims the breaker's half-open probe and
    /// then fails before any fill exists must not leak the claim: the
    /// breaker would otherwise reject every later cold open for the
    /// life of the process while reporting healthy to the heartbeat.
    // r[verify builder.fs.fetch-circuit]
    #[test]
    fn failed_streaming_open_does_not_leak_the_half_open_probe() {
        let circuit = Arc::new(CircuitBreaker::with_clock(
            1,
            Duration::ZERO,
            Duration::from_secs(720),
            SystemClock,
        ));
        let content = vec![0x99u8; 64 * 1024];
        let digest = *blake3::hash(&content).as_bytes();
        let h = OpenHarness::new(
            FakeDirectory::new(Err(tonic::Code::FailedPrecondition), content.clone()),
            Arc::clone(&circuit),
            |_| {},
            granting,
        );
        // Trip the breaker (threshold 1); auto_close ZERO makes the very
        // next check half-open.
        circuit.record(false);
        // Make the local staging setup fail: the staging dir is gone.
        std::fs::remove_dir_all(h.tmp.path().join("staging")).unwrap();

        let err = h
            .opener
            .open_inner(digest, content.len() as u64)
            .expect_err("no staging dir → the open fails");
        assert_eq!(err.code(), Errno::EIO.code());

        assert!(
            h.circuit.check().is_ok(),
            "the failed open must not leave the half-open probe claimed"
        );
    }

    // ── Kernel io-mode compatibility (caching vs passthrough) ─────────

    /// While streaming (FOPEN_KEEP_CACHE) handles of a digest are still
    /// open, a later open of the same content inode must NOT reply
    /// passthrough: the kernel refuses to mix caching and passthrough
    /// opens on one inode and would fail the open of a healthy file
    /// with EIO. Once the caching handles drain, passthrough resumes.
    // r[verify builder.fs.open-iomode-compatible]
    #[test]
    fn live_streaming_handles_block_passthrough_until_released() {
        let content: Vec<u8> = (0..32 * 1024u32).map(|i| (i % 251) as u8).collect();
        let digest = *blake3::hash(&content).as_bytes();
        let h = OpenHarness::with_blob(content.clone());

        // Cold open above the threshold: streaming KEEP_CACHE handle.
        let o1 = h.opener.open_inner(digest, content.len() as u64).unwrap();
        assert!(matches!(o1, OpenOutcome::KeepCache { .. }));
        let fh1 = fh_of(&o1);
        assert_eq!(
            h.opener.read(fh1, 0, content.len() as u32).unwrap(),
            content,
            "the streaming handle serves the filled bytes"
        );

        // The fill promoted the file; the daemon (played by the test)
        // publishes it into the node cache.
        h.seed_cache(&content);

        // Second open while the first handle is still open: must stay in
        // a caching mode.
        let o2 = h.opener.open_inner(digest, content.len() as u64).unwrap();
        assert!(
            matches!(o2, OpenOutcome::KeepCache { .. }),
            "a passthrough reply here would EIO at the kernel (mixed io-modes), got {o2:?}"
        );
        let fh2 = fh_of(&o2);
        assert_eq!(
            h.opener.read(fh2, 0, content.len() as u32).unwrap(),
            content
        );
        assert_eq!(
            h.backing_open_count(),
            0,
            "no passthrough backing is registered while caching handles are live"
        );

        // Drain the caching handles; passthrough resumes.
        h.opener.release(&digest, fh1);
        h.opener.release(&digest, fh2);
        let o3 = h.opener.open_inner(digest, content.len() as u64).unwrap();
        assert!(
            matches!(o3, OpenOutcome::Passthrough { .. }),
            "once the caching handles drain the next open goes passthrough, got {o3:?}"
        );
    }

    /// The reverse direction: the LRU sweep evicts the cache entry while
    /// passthrough opens are still live. The next open must reuse the
    /// live backing (the kernel still pins the backing file) instead of
    /// re-fetching into a caching open the kernel would reject.
    // r[verify builder.fs.open-iomode-compatible]
    #[test]
    fn swept_cache_entry_with_live_backing_reuses_the_backing() {
        let content: Vec<u8> = (0..32 * 1024u32).map(|i| (i % 13) as u8).collect();
        let frames_served;
        let h = {
            let dir = FakeDirectory::new(Err(tonic::Code::FailedPrecondition), content.clone());
            frames_served = Arc::clone(&dir.frames_served);
            OpenHarness::new(dir, Arc::new(CircuitBreaker::default()), |_| {}, granting)
        };
        let digest = h.seed_cache(&content);

        let o1 = h.opener.open_inner(digest, content.len() as u64).unwrap();
        let id1 = match &o1 {
            OpenOutcome::Passthrough { backing_id, .. } => *backing_id,
            other => panic!("warm open must be passthrough, got {other:?}"),
        };

        // Disk pressure: the sweep unlinks the entry while the first
        // open still has it mapped.
        std::fs::remove_file(h.cache_file(&digest)).unwrap();

        let o2 = h.opener.open_inner(digest, content.len() as u64).unwrap();
        match &o2 {
            OpenOutcome::Passthrough { backing_id, .. } => assert_eq!(
                *backing_id, id1,
                "the live backing is reused; a fresh registration would EBUSY"
            ),
            other => panic!(
                "an open with a live backing must stay passthrough (a caching reply would EIO at the kernel), got {other:?}"
            ),
        }
        assert_eq!(h.backing_open_count(), 1, "exactly one registration");
        assert_eq!(
            frames_served.load(Ordering::SeqCst),
            0,
            "the kernel-pinned backing serves the open; nothing is re-fetched"
        );

        h.opener.release(&digest, fh_of(&o1));
        h.opener.release(&digest, fh_of(&o2));
        assert_eq!(
            h.backing_close_count(),
            1,
            "the backing closes once, after the last passthrough handle drains"
        );
    }

    /// The mode decision and the caching-handle count must be one
    /// atomic step: a streaming open that decided "caching" before a
    /// concurrent open of the same digest registered a passthrough
    /// backing must not finish with a `FOPEN_KEEP_CACHE` reply — that
    /// is the mixed-io-mode pair the kernel rejects with EIO. The
    /// schedule is forced deterministically: the streaming fill is
    /// gated before its first chunk, the concurrent open registers the
    /// backing while it is parked, and only then is the fill released.
    // r[verify builder.fs.open-iomode-compatible]
    #[test]
    fn caching_reply_racing_a_backing_registration_reuses_the_backing() {
        let content: Vec<u8> = (0..32 * 1024u32).map(|i| (i % 7) as u8).collect();
        let digest = *blake3::hash(&content).as_bytes();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let h = {
            let mut dir = FakeDirectory::new(Err(tonic::Code::FailedPrecondition), content.clone());
            dir.read_blob_gate = Some(Arc::clone(&gate));
            OpenHarness::new(dir, Arc::new(CircuitBreaker::default()), |_| {}, granting)
        };
        let partial_path = h
            .tmp
            .path()
            .join("staging")
            .join(format!("{}.partial", hex::encode(digest)));

        let (warm, streamed) = std::thread::scope(|s| {
            // Cold streaming open: parks in wait_first_chunk because the
            // gated store has not released the first frame yet — i.e.
            // after its (miss) cache check, before its caching reply.
            let streaming = s.spawn(|| h.opener.open_inner(digest, content.len() as u64));

            // Wait until the streaming open has committed to the fill
            // (its staging .partial exists), so the seeded entry below
            // cannot turn it into a plain cache hit.
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !partial_path.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "streaming open never created its .partial"
                );
                std::thread::sleep(Duration::from_millis(5));
            }

            // A concurrent build publishes the entry; a second open of
            // the same digest finds it and registers a passthrough
            // backing while the streaming open is still parked.
            h.seed_cache(&content);
            let warm = h.opener.open_inner(digest, content.len() as u64).unwrap();

            // Release the fill; the streaming open now completes its
            // reply with the backing already live.
            gate.add_permits(1);
            (warm, streaming.join().unwrap().unwrap())
        });

        let warm_id = match &warm {
            OpenOutcome::Passthrough { backing_id, .. } => *backing_id,
            other => panic!("the warm open registers a passthrough backing, got {other:?}"),
        };
        match &streamed {
            OpenOutcome::Passthrough { backing_id, .. } => assert_eq!(
                *backing_id, warm_id,
                "the streaming open reuses the concurrently registered backing"
            ),
            other => panic!(
                "a caching reply while a passthrough backing is live is the mixed-io-mode open \
                 the kernel rejects with EIO, got {other:?}"
            ),
        }
        assert_eq!(h.backing_open_count(), 1, "exactly one registration");

        // Both handles release against the one shared backing.
        h.opener.release(&digest, fh_of(&warm));
        h.opener.release(&digest, fh_of(&streamed));
        assert_eq!(h.backing_close_count(), 1);
    }

    // ── Whole-file fetch is bounded by the expected size ───────────────

    /// A store that streams more bytes than the DAG-recorded size must
    /// be cut off at the overrun, not consumed until the stream ends:
    /// the extra bytes burn staging quota/disk and park a fuser thread
    /// for up to the full fetch timeout before the digest check would
    /// reject them anyway. Exercises `stream_blob` directly so the
    /// bound is observable on the staging file (gRPC flow-control
    /// buffering makes server-side frame counts nondeterministic).
    #[test]
    fn whole_file_fetch_stops_at_the_expected_size() {
        let frame = vec![0x5Au8; 128 * 1024];
        let size = 2 * frame.len() as u64;
        let digest = *blake3::hash(&[frame.clone(), frame.clone()].concat()).as_bytes();

        // The store misbehaves: after the real two frames it keeps
        // streaming garbage.
        let dir = FakeDirectory {
            stat: Err(tonic::Code::FailedPrecondition),
            blob_frames: vec![frame; 18],
            hang_at_end: false,
            read_blob_gate: None,
            frames_served: Arc::new(AtomicUsize::new(0)),
        };
        let h = OpenHarness::new(dir, Arc::new(CircuitBreaker::default()), |_| {}, granting);

        let partial_path = h.tmp.path().join("staging").join("overrun.partial");
        let mut partial = create_partial(&partial_path).unwrap();
        let result = h
            .opener
            .stream_blob(&digest, size, Duration::from_secs(5), &mut partial);
        assert!(
            result.is_err(),
            "an overrunning stream must fail the fetch attempt"
        );
        assert!(
            partial.metadata().unwrap().len() <= size,
            "no byte past the expected size reaches staging (got {})",
            partial.metadata().unwrap().len()
        );
    }

    // ── The fuse-over-io_uring fast/slow tier boundary ────────────────

    /// `open_warm` is what a ring queue thread may call inline: it must
    /// serve a published cache entry (local state + the mountd UDS
    /// broker) and report a miss as `None` — the punt signal — without
    /// touching the store. A `Some` for a cold digest would block a
    /// queue thread on a gRPC fetch; a `None` for a warm one would
    /// route every open through the slow pool and serialize warm-open
    /// storms.
    #[test]
    fn open_warm_serves_published_entries_and_reports_misses() {
        let h = OpenHarness::with_blob(vec![1u8; 64]);
        let digest = h.seed_cache(b"warm bytes");
        let outcome = h
            .opener
            .open_warm(digest)
            .unwrap()
            .expect("published cache entry must be servable warm");
        assert!(
            matches!(outcome, OpenOutcome::Passthrough { .. }),
            "warm open of a published entry goes passthrough, got {outcome:?}"
        );

        let cold = *blake3::hash(b"never promoted").as_bytes();
        assert!(
            h.opener.open_warm(cold).unwrap().is_none(),
            "a cache miss must punt (None), not fetch"
        );
    }

    /// `read_fast` is the ring queue thread's read tier: an
    /// already-open fallback handle is a local pread (`Some`), a
    /// streaming fill handle can block on the fill's high-water mark
    /// (`None` → slow pool), and an unknown fh is an immediate EBADF
    /// (nothing to wait for). The full `read` must keep serving the
    /// punted stream — that is what the slow worker calls.
    #[test]
    fn read_fast_serves_local_handles_and_punts_streams() {
        // disable_passthrough lands the open in the userspace-read
        // tier, i.e. `open_files`.
        let content = b"local handle bytes".to_vec();
        let h = OpenHarness::new(
            FakeDirectory::new(Err(tonic::Code::FailedPrecondition), content.clone()),
            Arc::new(CircuitBreaker::default()),
            |cfg| cfg.disable_passthrough = true,
            granting,
        );
        let digest = h.seed_cache(&content);
        let outcome = h.opener.open_inner(digest, content.len() as u64).unwrap();
        let fh = fh_of(&outcome);
        let got = h
            .opener
            .read_fast(fh, 0, 64)
            .expect("fallback handle reads are fast")
            .unwrap();
        assert_eq!(got, content);

        assert!(
            matches!(h.opener.read_fast(9999, 0, 16), Some(Err(e)) if e.code() == Errno::EBADF.code()),
            "unknown fh is an inline EBADF, not a punt"
        );

        // Above the streaming threshold with nothing published: the
        // open joins a fill and the handle lives in `streams`.
        let big = vec![7u8; 16 * 1024];
        let big_digest = *blake3::hash(&big).as_bytes();
        let h2 = OpenHarness::with_blob(big.clone());
        let outcome = h2.opener.open_inner(big_digest, big.len() as u64).unwrap();
        assert!(
            matches!(outcome, OpenOutcome::KeepCache { .. }),
            "an in-flight fill serves KEEP_CACHE, got {outcome:?}"
        );
        let fh = fh_of(&outcome);
        assert!(
            h2.opener.read_fast(fh, 0, 1024).is_none(),
            "streaming-fill reads must punt to the slow pool"
        );
        assert_eq!(
            h2.opener.read(fh, 0, 1024).unwrap(),
            vec![7u8; 1024],
            "the slow tier's read still serves the punted stream"
        );
    }
}
