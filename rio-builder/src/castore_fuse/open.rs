//! The castore-FUSE `open()` data path (ADR-022 §2.6–§2.7).
//!
//! `open(ino)` resolves `ino → file_digest` and is a **broker, not a
//! server**: its job is to ensure the file exists in the node-SSD
//! backing cache and hand the kernel a passthrough fd to it. Three
//! cases:
//!
//! - **(a) cache hit** — `/var/rio/cache/{ab}/{hex}` exists: open it
//!   `O_RDONLY`, broker a `backing_id` through rio-mountd, reply
//!   `FOPEN_PASSTHROUGH`. Zero further upcalls for this fd.
//! - **(b) miss, `size ≤ stream_threshold`** — fetch the whole file via
//!   `ReadBlob(file_digest)` into the per-build staging dir, verify its
//!   blake3, `Promote` it into the shared cache, then as (a).
//! - **(c) miss, `size > stream_threshold`** — P0575's streaming open
//!   ([`super::stream`]): spawn a background chunk-by-chunk fill and
//!   reply `FOPEN_KEEP_CACHE` once the first chunk has landed; `read()`
//!   serves from the partially-filled staging file until the fill
//!   completes and promotes, after which the next open is case (a).
//!
//! The cache lookup / fill / promote sequence lives in [`OpenPath`],
//! which is independent of fuser types (beyond `Errno`) so the mode
//! dispatch and the fill race are unit-testable without a kernel mount.
// r[impl builder.fs.digest-fuse-open]

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use fuser::Errno;
use tokio::runtime::Handle;

use rio_proto::types::ReadBlobRequest;

use super::circuit::CircuitBreaker;
use super::mountd_client::MountdClient;
use super::stream::{self, StreamFill};
use crate::IgnorePoison;
use crate::store_fetch::StoreClients;

/// Tunables for the open path, lifted from [`crate::config::Config`].
#[derive(Clone, Debug)]
pub struct OpenConfig {
    /// Whole-file JIT fetch budget (`ReadBlob` stream + blake3 verify).
    pub jit_fetch_timeout: Duration,
    /// Per-request budget for every mountd UDS round-trip.
    pub mountd_request_timeout: Duration,
    /// Files larger than this take the streaming path (P0575): `open()`
    /// returns after the first chunk while the rest fills in the
    /// background, instead of blocking for the whole transfer.
    pub stream_threshold: u64,
}

/// Which dispatch arm an `open()` took — the `case` label of
/// `rio_builder_castore_fuse_open_case_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenCase {
    /// Backing cache hit: no fetch.
    Hit,
    /// Miss, `size ≤ stream_threshold`: whole-file fetch.
    MissSmall,
    /// Miss, `size > stream_threshold`: this open started the
    /// streaming fill (P0575) and returned once its first chunk
    /// landed.
    MissStream,
    /// Another open of the same `file_digest` was already filling
    /// (whole-file or streaming); this one waited for it (completion
    /// for the whole-file path, the first chunk for the streaming
    /// path).
    WaitFetching,
}

impl OpenCase {
    pub fn label(self) -> &'static str {
        match self {
            OpenCase::Hit => "hit",
            OpenCase::MissSmall => "miss_small",
            OpenCase::MissStream => "miss_stream",
            OpenCase::WaitFetching => "wait_fetching",
        }
    }
}

/// In-process coordination for concurrent `open()`s of the same
/// `file_digest` within one build (e.g. `make -jN` both `dlopen`ing one
/// `.so`). The first opener (the *winner*) performs the fetch; every
/// other opener (a *loser*) waits on the condvar until the winner
/// publishes a result, then re-checks the cache.
struct FillState {
    /// `None` while the fill is in progress; `Some(outcome)` once the
    /// winner has finished (either way).
    done: Mutex<Option<Result<(), Errno>>>,
    cv: Condvar,
}

impl FillState {
    fn new() -> Self {
        Self {
            done: Mutex::new(None),
            cv: Condvar::new(),
        }
    }

    /// Publish the winner's outcome and wake every waiter.
    fn finish(&self, outcome: Result<(), Errno>) {
        *self.done.lock().ignore_poison() = Some(outcome);
        self.cv.notify_all();
    }

    /// Block until the winner finishes, at most `deadline`. Returns the
    /// winner's outcome, or `Err(EIO)` if the wait itself timed out
    /// (the winner is wedged past its own fetch timeout).
    fn wait(&self, deadline: Duration) -> Result<(), Errno> {
        let guard = self.done.lock().ignore_poison();
        let (guard, timeout) = self
            .cv
            .wait_timeout_while(guard, deadline, |done| done.is_none())
            .ignore_poison();
        if timeout.timed_out() {
            return Err(Errno::EIO);
        }
        guard.unwrap_or(Err(Errno::EIO))
    }
}

/// What `open()` should reply with, as decided by
/// [`OpenPath::ensure_readable`].
pub enum Readable {
    /// The backing-cache entry is complete: open it and reply
    /// passthrough (or the keep-cache fallback).
    Backing(OpenCase),
    /// A P0575 streaming fill is in progress (this open started it, or
    /// attached to one already running): reply `FOPEN_KEEP_CACHE` and
    /// serve `read()` from the shared fill until it completes.
    Streaming {
        fill: Arc<StreamFill>,
        case: OpenCase,
    },
}

impl std::fmt::Debug for Readable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Readable::Backing(case) => f.debug_tuple("Backing").field(case).finish(),
            Readable::Streaming { case, .. } => f
                .debug_struct("Streaming")
                .field("case", case)
                .finish_non_exhaustive(),
        }
    }
}

/// The `open()` data path: cache lookup, JIT fetch, promote.
///
/// Shared across all fuser threads behind `&self`; interior mutability
/// is std::sync only (FUSE callbacks are not tokio tasks). The async
/// gRPC fetch is bridged with `Handle::block_on` from the calling
/// (blocking) fuser thread — the established `fuse::fetch` pattern.
pub struct OpenPath {
    /// Shared node-SSD backing cache root (`/var/rio/cache`).
    /// Mountd-owned, builder-readonly; entries appear here only via
    /// `Promote`.
    cache_dir: PathBuf,
    /// This build's staging dir (`/var/rio/staging/{build_id}`),
    /// builder-writable, created by mountd at `Mount` time.
    staging_dir: PathBuf,
    /// Shared node-SSD chunk cache root (`/var/rio/chunks`).
    /// Mountd-owned, builder-readonly; entries appear here only via
    /// `PromoteChunks`. Consulted by the P0575 streaming fill before
    /// any remote chunk fetch.
    chunks_dir: PathBuf,
    /// Per-`file_digest` in-flight whole-file fills (case (b)).
    fills: Mutex<HashMap<[u8; 32], Arc<FillState>>>,
    /// Per-`file_digest` in-flight streaming fills (case (c)). Behind
    /// an `Arc` because each fill thread deregisters itself when it
    /// finishes.
    streams: Arc<Mutex<HashMap<[u8; 32], Arc<StreamFill>>>>,
    /// Breaker around the remote fetch. Checked before every `ReadBlob`
    /// attempt; recorded after.
    pub circuit: Arc<CircuitBreaker>,
    clients: StoreClients,
    /// Tokio runtime handle for bridging the sync FUSE callback into
    /// the async gRPC fetch (`Handle::block_on` from the calling fuser
    /// thread).
    ///
    /// INVARIANT (P0560 mount wiring): no task scheduled on this
    /// runtime may perform blocking filesystem I/O against the castore
    /// mount or the overlay merged view stacked on top of it.
    /// `block_on` parks a fuser thread until a tokio worker drives the
    /// fetch future to completion; if a tokio worker is itself parked
    /// in the kernel waiting on a FUSE upcall that only a fuser thread
    /// can answer, and every fuser thread is parked in `block_on`
    /// waiting for a tokio worker, the mount deadlocks with no
    /// timeout to save it. Today the invariant holds because this
    /// runtime only runs gRPC I/O and the upload pipeline (which reads
    /// the overlay *upper*, never the lower). The old FUSE module's
    /// warm-stat `spawn_blocking` tasks consumed the mount they served
    /// and paid for it with D-state hangs at teardown (I-165) — do not
    /// reintroduce that shape here.
    runtime: Handle,
    mountd: MountdClient,
    cfg: OpenConfig,
}

impl OpenPath {
    pub fn new(
        cache_dir: PathBuf,
        staging_dir: PathBuf,
        chunks_dir: PathBuf,
        clients: StoreClients,
        runtime: Handle,
        mountd: MountdClient,
        cfg: OpenConfig,
    ) -> Self {
        Self {
            cache_dir,
            staging_dir,
            chunks_dir,
            fills: Mutex::new(HashMap::new()),
            streams: Arc::new(Mutex::new(HashMap::new())),
            circuit: Arc::new(CircuitBreaker::default()),
            clients,
            runtime,
            mountd,
            cfg,
        }
    }

    /// The mountd connection, for `BackingOpen`/`BackingClose` from the
    /// `Filesystem` impl.
    pub fn mountd(&self) -> &MountdClient {
        &self.mountd
    }

    /// Per-request mountd timeout, for the `Filesystem` impl's calls.
    pub fn mountd_timeout(&self) -> Duration {
        self.cfg.mountd_request_timeout
    }

    /// Backing-cache path for a file digest:
    /// `{cache_dir}/{ab}/{hex}` where `ab` is the first two hex chars
    /// (the same shard layout rio-mountd's `Promote` writes).
    // r[impl builder.fs.shared-backing-cache]
    pub fn cache_path(&self, file_digest: &[u8; 32]) -> PathBuf {
        let hex = hex::encode(file_digest);
        self.cache_dir.join(&hex[..2]).join(&hex)
    }

    /// Staging path for a file digest (no shard — the staging dir is
    /// per-build and short-lived).
    fn staging_path(&self, file_digest: &[u8; 32]) -> PathBuf {
        self.staging_dir.join(hex::encode(file_digest))
    }

    /// (a) Cache hit — the common steady-state case. No locks, no
    /// RPCs: one stat against the node SSD. The objects-cache
    /// observables count exactly this case: the digest was already
    /// present in the shared backing cache, so `size` bytes did not
    /// have to be re-fetched from the store (P0571).
    // r[impl builder.fs.passthrough-on-hit]
    fn cache_hit(&self, file_digest: &[u8; 32], size: u64) -> bool {
        if self.cache_path(file_digest).exists() {
            metrics::counter!("rio_builder_objects_cache_hit_total").increment(1);
            metrics::counter!("rio_builder_objects_cache_bytes").increment(size);
            return true;
        }
        false
    }

    /// The full `open()` dispatch: decide how this `(file_digest, size)`
    /// will be served and make it so.
    ///
    /// - cache hit, or miss at `size ≤ stream_threshold` (whole-file
    ///   fetch + promote) → [`Readable::Backing`]: the backing-cache
    ///   entry is complete when this returns.
    /// - miss at `size > stream_threshold` → [`Readable::Streaming`]:
    ///   a background fill is running and at least its first chunk is
    ///   servable when this returns.
    // r[impl builder.fs.streaming-open-threshold]
    pub fn ensure_readable(&self, file_digest: &[u8; 32], size: u64) -> Result<Readable, Errno> {
        if size <= self.cfg.stream_threshold {
            return self
                .ensure_backing(file_digest, size)
                .map(Readable::Backing);
        }
        if self.cache_hit(file_digest, size) {
            return Ok(Readable::Backing(OpenCase::Hit));
        }
        let (fill, case) = self.attach_stream(file_digest, size)?;
        Ok(Readable::Streaming { fill, case })
    }

    /// Ensure `file_digest` is present in the backing cache, fetching
    /// and promoting it (whole-file) if necessary. On success the file
    /// at [`Self::cache_path`] exists and is complete. Returns which
    /// dispatch case was taken.
    ///
    /// Safe to call concurrently for the same digest from multiple
    /// fuser threads: exactly one performs the fetch, the rest wait.
    /// Files above the streaming threshold are dispatched to
    /// [`Self::ensure_readable`]'s streaming arm instead — this method
    /// always pays the whole transfer before returning.
    pub fn ensure_backing(&self, file_digest: &[u8; 32], size: u64) -> Result<OpenCase, Errno> {
        if self.cache_hit(file_digest, size) {
            return Ok(OpenCase::Hit);
        }

        // Miss. Decide winner vs loser for this digest.
        let (state, winner) = {
            let mut fills = self.fills.lock().ignore_poison();
            match fills.get(file_digest) {
                Some(existing) => (Arc::clone(existing), false),
                None => {
                    let state = Arc::new(FillState::new());
                    fills.insert(*file_digest, Arc::clone(&state));
                    (state, true)
                }
            }
        };

        if !winner {
            // Loser: wait for the winner, then re-check the cache. The
            // winner's own work is bounded by `jit_fetch_timeout` (the
            // fetch) plus `mountd_request_timeout` (the promote); the
            // wait deadline covers both so a healthy winner always
            // publishes before a waiter gives up.
            let deadline = self.cfg.jit_fetch_timeout + self.cfg.mountd_request_timeout;
            let outcome = state.wait(deadline);
            // The cache, not the condvar outcome, is the source of
            // truth: a wait that timed out a hair before the winner's
            // promote landed must not fail an open whose backing file
            // is sitting right there.
            if self.cache_path(file_digest).exists() {
                return Ok(OpenCase::WaitFetching);
            }
            return Err(outcome.err().unwrap_or(Errno::EIO));
        }

        // Winner: fetch, verify, promote. Always publish an outcome and
        // remove the fill entry, even on the error paths — a leaked
        // in-progress FillState would park every future opener of this
        // digest until their wait deadline.
        //
        // The "always" must survive a panic, not just an Err.
        // The realistic panic in the fill is `Handle::block_on` on a
        // runtime that is shutting down (process teardown racing a
        // late open()); fuser catches callback panics, so without this
        // guard the leaked FillState would silently wedge every future
        // open of this digest. Disarmed on the normal return path,
        // which publishes the real outcome instead of the guard's EIO.
        let unwind_guard = scopeguard::guard((), |()| {
            state.finish(Err(Errno::EIO));
            self.fills.lock().ignore_poison().remove(file_digest);
        });
        let outcome = self.fill_and_promote(file_digest);
        scopeguard::ScopeGuard::into_inner(unwind_guard);
        state.finish(outcome);
        self.fills.lock().ignore_poison().remove(file_digest);
        outcome.map(|()| OpenCase::MissSmall)
    }

    /// Attach to (or start) the P0575 streaming fill for `file_digest`,
    /// then wait until its first chunk is servable so the caller can
    /// reply `FOPEN_KEEP_CACHE` immediately.
    ///
    /// The streams lock is held across the fill creation (a `.partial`
    /// create + a thread spawn) so two racing first-opens of one digest
    /// serialize into start-then-attach instead of both starting — the
    /// same justification as `BackingTable`'s lock-across-mint.
    // r[impl builder.fs.streaming-open]
    fn attach_stream(
        &self,
        file_digest: &[u8; 32],
        size: u64,
    ) -> Result<(Arc<StreamFill>, OpenCase), Errno> {
        let (fill, case) = {
            let mut streams = self.streams.lock().ignore_poison();
            if let Some(existing) = streams.get(file_digest) {
                (Arc::clone(existing), OpenCase::WaitFetching)
            } else {
                // Starting a new fill: same fail-fast breaker gate as
                // the whole-file path — don't queue another doomed
                // fetch behind the ones already timing out.
                self.circuit.check()?;
                let budget =
                    crate::store_fetch::jit_fetch_timeout(self.cfg.jit_fetch_timeout, size);
                let ctx = stream::FillContext {
                    file_digest: *file_digest,
                    size,
                    partial_path: self.staging_path(file_digest).with_extension("partial"),
                    staging_path: self.staging_path(file_digest),
                    staging_chunks_dir: self.staging_dir.join("chunks"),
                    chunks_dir: self.chunks_dir.clone(),
                    clients: self.clients.clone(),
                    runtime: self.runtime.clone(),
                    mountd: self.mountd.clone(),
                    circuit: Arc::clone(&self.circuit),
                    mountd_timeout: self.cfg.mountd_request_timeout,
                    budget,
                    registry: Arc::clone(&self.streams),
                };
                let partial_path = ctx.partial_path.clone();
                let fill = match stream::spawn_fill(ctx) {
                    Ok(fill) => fill,
                    Err(e) => {
                        tracing::error!(
                            path = %partial_path.display(),
                            error = %e,
                            "castore-fuse: cannot start the streaming fill"
                        );
                        return Err(Errno::EIO);
                    }
                };
                streams.insert(*file_digest, Arc::clone(&fill));
                (fill, OpenCase::MissStream)
            }
        };
        // Outside the lock: wait for the first chunk (or the fill's
        // failure). Bounded by the flat JIT budget — the first chunk is
        // one StatBlob plus one ≤256 KiB chunk, not the whole file.
        fill.wait_first_chunk(self.cfg.jit_fetch_timeout)?;
        Ok((fill, case))
    }

    /// The winner's fill: `ReadBlob` → staging, blake3 verify, rename,
    /// `Promote`. Returns `Err(Errno)` ready to hand to the kernel.
    fn fill_and_promote(&self, file_digest: &[u8; 32]) -> Result<(), Errno> {
        // Fail fast if the store has been unreachable long enough to
        // trip the breaker — don't queue another doomed fetch behind
        // the ones already timing out.
        self.circuit.check()?;

        let partial = self.staging_path(file_digest).with_extension("partial");
        let final_staging = self.staging_path(file_digest);

        let mut file = match create_partial(&partial) {
            Ok(f) => f,
            Err(e) => {
                tracing::error!(
                    path = %partial.display(),
                    error = %e,
                    "castore-fuse: cannot create staging .partial"
                );
                // Not a fetch failure — don't count it against the
                // store's circuit.
                return Err(Errno::EIO);
            }
        };

        // Stream the blob into the .partial, hashing as we go. The
        // whole attempt (connect, stream, disk writes) is bounded by
        // jit_fetch_timeout.
        // r[impl builder.fs.file-digest-integrity]
        let fetch = self.runtime.block_on(tokio::time::timeout(
            self.cfg.jit_fetch_timeout,
            read_blob_into(self.clients.clone(), *file_digest, &mut file),
        ));
        let fetched = match fetch {
            Err(_elapsed) => {
                tracing::warn!(
                    digest = %hex::encode(file_digest),
                    timeout = ?self.cfg.jit_fetch_timeout,
                    "castore-fuse: ReadBlob timed out"
                );
                Err(Errno::EIO)
            }
            Ok(Err(FetchError::Integrity { got, want, bytes })) => {
                metrics::counter!("rio_builder_castore_fuse_integrity_fail_total").increment(1);
                tracing::error!(
                    want = %want,
                    got = %got,
                    bytes,
                    "castore-fuse: ReadBlob content does not match its file_digest"
                );
                Err(Errno::EIO)
            }
            Ok(Err(FetchError::Rpc(status))) => {
                tracing::warn!(
                    digest = %hex::encode(file_digest),
                    status = %status,
                    "castore-fuse: ReadBlob failed"
                );
                Err(Errno::EIO)
            }
            Ok(Err(FetchError::Io(e))) => {
                tracing::error!(
                    digest = %hex::encode(file_digest),
                    error = %e,
                    "castore-fuse: writing the fetched blob to staging failed"
                );
                Err(Errno::EIO)
            }
            Ok(Ok(bytes)) => {
                metrics::counter!("rio_builder_castore_fuse_fetch_bytes_total", "hit" => "remote")
                    .increment(bytes);
                // P0575 splits this by chunk provenance (node chunk
                // cache vs remote); the whole-file path is all-remote.
                metrics::counter!("rio_builder_castore_fuse_chunk_source_total", "src" => "remote")
                    .increment(1);
                Ok(())
            }
        };
        // One record per fetch attempt, success or failure — the
        // breaker's consecutive-failure count is the "is the store
        // healthy" signal.
        self.circuit.record(fetched.is_ok());
        if let Err(errno) = fetched {
            drop(file);
            let _ = std::fs::remove_file(&partial);
            return Err(errno);
        }
        if let Err(e) = file.flush() {
            tracing::error!(error = %e, "castore-fuse: flushing staging .partial failed");
            drop(file);
            let _ = std::fs::remove_file(&partial);
            return Err(Errno::EIO);
        }
        drop(file);

        // Verified. Publish into this build's staging under the bare
        // digest name (what mountd's Promote opens), then ask mountd to
        // verify-copy it into the shared cache.
        if let Err(e) = std::fs::rename(&partial, &final_staging) {
            tracing::error!(
                from = %partial.display(),
                to = %final_staging.display(),
                error = %e,
                "castore-fuse: staging rename failed"
            );
            let _ = std::fs::remove_file(&partial);
            return Err(Errno::EIO);
        }
        match self
            .mountd
            .promote(*file_digest, self.cfg.mountd_request_timeout)
        {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::error!(
                    digest = %hex::encode(file_digest),
                    error = %e,
                    build_fatal = e.is_build_fatal(),
                    "castore-fuse: Promote failed"
                );
                // Leave the staging file for post-mortem; mountd reaps
                // the whole staging dir on connection close.
                Err(Errno::EIO)
            }
        }
    }
}

/// `O_EXCL`-create the staging `.partial` and take an exclusive flock
/// on it, held until the returned guard drops (fill complete or
/// abandoned). The in-process [`FillState`] (or the streaming fill
/// registry) already guarantees one filler per digest; the `O_EXCL` +
/// flock combination exists to detect and reclaim a `.partial`
/// orphaned by an earlier fill whose thread died without cleaning up
/// (panic swallowed by fuser): an orphan has no flock holder, so
/// `EEXIST` → `flock(LOCK_NB)` succeeds → unlink → retry the create
/// exactly once.
///
/// Opened read+write: the whole-file path only writes through it, but
/// the P0575 streaming fill also serves `read()` upcalls from the same
/// fd while it fills.
pub(super) fn create_partial(path: &Path) -> std::io::Result<nix::fcntl::Flock<File>> {
    use nix::fcntl::{Flock, FlockArg};
    for attempt in 0..2 {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(f) => {
                // This thread just created the file with O_EXCL, so no
                // live fill can hold the lock — a non-blocking acquire
                // can only fail on a pathological filesystem.
                return Flock::lock(f, FlockArg::LockExclusiveNonblock)
                    .map_err(|(_, e)| std::io::Error::from(e));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt == 0 => {
                // A `.partial` already exists. If its flock is free, it
                // is an orphan from a fill that died without cleanup;
                // if it is held, a live fill owns it — which cannot
                // happen here (the FillState map had no entry for this
                // digest), so a held lock means something is seriously
                // wrong and the second create_new attempt will surface
                // it as EEXIST → EIO.
                match std::fs::OpenOptions::new().write(true).open(path) {
                    Ok(orphan) => match Flock::lock(orphan, FlockArg::LockExclusiveNonblock) {
                        Ok(_held) => {
                            tracing::warn!(
                                path = %path.display(),
                                "castore-fuse: reclaiming orphaned staging .partial"
                            );
                            std::fs::remove_file(path)?;
                            // `_held` drops here, after the unlink — the
                            // lock dies with the unlinked inode.
                        }
                        Err((_, nix::errno::Errno::EWOULDBLOCK)) => {
                            return Err(std::io::Error::other(
                                "staging .partial is flock-held by a fill this process does not \
                                 know about",
                            ));
                        }
                        Err((_, e)) => return Err(std::io::Error::from(e)),
                    },
                    // Deleted between the EEXIST and the open — retry
                    // the create.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("create_partial loops at most twice and the second pass always returns")
}

/// Why a `ReadBlob` fill failed.
enum FetchError {
    /// gRPC error (connect, stream reset, NotFound, ...).
    Rpc(tonic::Status),
    /// Local disk write failed.
    Io(std::io::Error),
    /// The streamed bytes do not hash to the requested `file_digest`.
    Integrity {
        want: String,
        got: String,
        bytes: u64,
    },
}

/// Stream `ReadBlob(file_digest)` into `dst`, hashing as we go, and
/// verify the whole-file blake3 before returning. Returns the byte
/// count on success.
///
/// `dst` writes are synchronous `std::io` calls from inside an async
/// fn — this future is only ever polled via `Handle::block_on` from a
/// fuser (blocking) thread, so a blocking write blocks a thread that is
/// already allowed to block (the `fuse::fetch::SyncSpool` precedent).
async fn read_blob_into(
    mut clients: StoreClients,
    file_digest: [u8; 32],
    dst: &mut File,
) -> Result<u64, FetchError> {
    let mut stream = clients
        .directory
        .read_blob(ReadBlobRequest {
            file_digest: file_digest.to_vec(),
        })
        .await
        .map_err(FetchError::Rpc)?
        .into_inner();
    let mut hasher = blake3::Hasher::new();
    let mut total = 0u64;
    while let Some(chunk) = stream.message().await.map_err(FetchError::Rpc)? {
        hasher.update(&chunk.data);
        dst.write_all(&chunk.data).map_err(FetchError::Io)?;
        total += chunk.data.len() as u64;
    }
    let got = hasher.finalize();
    if got.as_bytes() != &file_digest {
        return Err(FetchError::Integrity {
            want: hex::encode(file_digest),
            got: got.to_hex().to_string(),
            bytes: total,
        });
    }
    Ok(total)
}
