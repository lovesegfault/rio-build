//! Fetch and materialize store paths into the local cache.
//!
//! Two entry points:
//! - `NixStoreFs::ensure_cached`: called from FUSE callbacks
//!   (lookup, getattr). Handles singleflight WAIT semantics — if
//!   another thread is fetching, block on condvar until it finishes.
//! - [`prefetch_path_blocking`]: called from the PrefetchHint
//!   handler via spawn_blocking. Same singleflight but with
//!   RETURN-EARLY on WaitFor — prefetch is a hint, not a dependency;
//!   if FUSE already has it in flight, we're done.
//!
//! Both delegate to `fetch_extract_insert` for the actual work.

mod client;

#[cfg(test)]
mod tests;

pub use client::{FetchTransport, StoreClients};

use std::io;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fuser::Errno;
use futures_util::stream::{self, StreamExt};
use tokio::io::AsyncWriteExt;
use tokio::runtime::Handle;
use tonic::transport::Channel;
use tracing::instrument;

use rio_proto::client::NarCollectError;
use rio_proto::store::chunk_service_client::ChunkServiceClient;
use rio_proto::types::{ChunkRef, GetChunkRequest};

use super::NixStoreFs;
use super::cache::{Cache, FetchClaim, InflightEntry};

/// In-flight `GetChunk` RPCs per fetch. K=32 → at 256 KiB avg chunks,
/// 50 ms cold S3 TTFB ≈ 160 MB/s; warm-moka (~1 ms in-cluster RTT) is
/// NIC-limited. tonic's p2c picks per call so 32 in-flight spread
/// across all SERVING store replicas — adding replicas adds bandwidth
/// without builder code change.
///
/// TODO(dataplane2): promote to a config field once A/B numbers are in.
pub const CHUNK_FETCH_CONCURRENCY: usize = 32;

/// Per-chunk transient retry attempts (inside the `.map` closure, before
/// failing the whole stream). Separate from `RETRY_BACKOFF` — chunk
/// retries are tight (no sleep; the buffered stream provides natural
/// spacing) and cheap (one chunk, not a whole NAR re-spool).
const CHUNK_RETRY_ATTEMPTS: usize = 2;

/// `AsyncWrite` adapter over a sync `std::fs::File` — does BLOCKING disk
/// I/O directly in `poll_write`.
///
/// **Only safe when polled from a thread that is allowed to block** — a
/// dedicated FUSE thread or `spawn_blocking`, which is
/// `fetch_extract_insert`'s caller contract. `Handle::block_on(fut)` from
/// such a thread polls `fut` ON that thread, so a blocking `poll_write`
/// blocks the already-blocking thread (correct) rather than a runtime
/// worker.
///
/// Why not `tokio::fs::File`: every write would `spawn_blocking`, which
/// inside `Handle::block_on` from an outer `spawn_blocking` adds a second
/// layer of blocking-pool round-trips per chunk. Under heavy
/// parallel-process load (workspace `nextest`) this surfaced as
/// load-dependent EIO in the fetch tests; the sync adapter is both
/// simpler and faster (no per-chunk thread hop).
struct SyncSpool(std::fs::File);

impl SyncSpool {
    /// Truncate + rewind for retry. Sync (caller is on a blocking thread).
    fn reset(&mut self) -> io::Result<()> {
        use std::io::Seek;
        self.0.set_len(0)?;
        self.0.seek(io::SeekFrom::Start(0))?;
        Ok(())
    }
}

impl tokio::io::AsyncWrite for SyncSpool {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::task::Poll::Ready(io::Write::write(&mut self.0, buf))
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(io::Write::flush(&mut self.0))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// Per-slice wait for the `WaitFor` arm's condvar heartbeat. NOT a deadline:
/// after each slice we check whether the fetcher finished and loop if not. The
/// real deadline is `fetch_timeout + WAIT_SLOP` (see `wait_deadline()`). 30s
/// in prod for visible debug logs on slow fetches; 200ms in tests so the
/// concurrent-waiter test runs in under a second.
#[cfg(not(test))]
const WAIT_SLICE: Duration = Duration::from_secs(30);
#[cfg(test)]
const WAIT_SLICE: Duration = Duration::from_millis(200);

/// Slop added to `fetch_timeout` for the `WaitFor` loop's deadline. Absorbs
/// the gap between `block_on` returning and the guard's `Drop` firing. If we
/// exceed `fetch_timeout + WAIT_SLOP`, the fetcher's own timeout should
/// already have dropped the guard; something is deeply wrong (executor
/// starvation?) and EAGAIN is the least-bad errno.
const WAIT_SLOP: Duration = Duration::from_secs(30);

/// Minimum expected store→builder throughput for JIT fetch-timeout
/// sizing. I-178: 15 MiB/s is a conservative floor — half the ~30 MB/s
/// observed in cluster (`rio_builder_fuse_fetch_bytes_total` ÷
/// `rio_builder_fuse_fetch_duration_seconds`). A 1.9 GB NAR at this
/// floor needs ≈127 s; the previous flat 60 s timeout aborted the fetch
/// mid-stream → daemon ENOENT → PermanentFailure poison.
///
/// Tune DOWN if `rio_builder_input_materialization_failures_total` is
/// sustained nonzero (means real throughput is below this floor —
/// cross-AZ builders, S3 throttle).
pub const JIT_MIN_THROUGHPUT_BPS: u64 = 15 * 1024 * 1024;

/// Per-path JIT fetch timeout: `max(base, nar_size / MIN_THROUGHPUT)`.
///
/// `base` is `fuse_fetch_timeout` (60 s) so small paths are unchanged
/// from pre-I-178 behavior. Large paths get a size-proportional budget
/// — the I-178 1.9 GB input gets ≈127 s instead of the flat 60 s that
/// aborted it mid-stream.
///
/// Under JIT (I-043 redesign) the FUSE callback IS the fetch site —
/// the daemon's `lstat` blocks in `request_wait_answer` for this
/// duration on a cold input. The size-aware budget is therefore
/// load-bearing for correctness (a too-short timeout → EIO →
/// `InfrastructureFailure`), not just an optimization.
pub fn jit_fetch_timeout(base: Duration, nar_size: u64) -> Duration {
    base.max(Duration::from_secs(
        nar_size.div_ceil(JIT_MIN_THROUGHPUT_BPS),
    ))
}

/// Backoff schedule for retrying transient store-gRPC errors
/// (`Unavailable` / `Unknown` — server restarting, transport disconnect)
/// inside [`fetch_extract_insert`]. Five delays = six attempts. Total
/// wait ~17.6s (× [`jitter`] per step → ~[8.8s, 26.4s)), sized to
/// survive a `replicas: 1` store rolling restart (~10s old-pod-SIGTERM
/// → new-pod-Ready) without surfacing `EIO` to the build sandbox.
/// I-039: a deploy mid-LLVM-build was killing 40min of work with an
/// opaque `Input/output error` on `stat()`.
///
/// I-189: schedule extended `[…, 5s]` → `[…, 5s, 10s]` and jittered at
/// the call site (NOT baked into this const — the const stays
/// deterministic for tests/docs; jitter is applied where the delay is
/// consumed). Under `hello-deep-256x` (~38000 drvs), hundreds of
/// builders `GetPath` the same 164 MB gcc within seconds; every builder
/// hits the same h2 reset and then retries at the SAME instant — the
/// retry IS the herd. Per-attempt jitter breaks lockstep; the extra
/// 10 s step buys one more drain window.
///
/// Sits BELOW the circuit breaker: `ensure_cached` checks the breaker
/// before calling here, so if the store has been down long enough to
/// trip it we never reach this loop. The retry handles the transition
/// window (was-up → briefly-down → up-again); the breaker handles
/// the steady-state (down-for-a-while → fail-fast).
///
/// Short in tests so the permanent-failure path stays sub-second.
#[cfg(not(test))]
const RETRY_BACKOFF: &[Duration] = &[
    Duration::from_millis(100),
    Duration::from_millis(500),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];
#[cfg(test)]
const RETRY_BACKOFF: &[Duration] = &[
    Duration::from_millis(10),
    Duration::from_millis(50),
    Duration::from_millis(200),
    Duration::from_millis(500),
];

/// Jitter a backoff delay: `delay × U(0.5, 1.5)`.
///
/// I-189: under thundering-herd, every builder that hit the same
/// transient error retries at the same instant — the retry IS the herd.
/// ±50% spread breaks lockstep while keeping the expected delay equal
/// to the schedule entry. Applied at the `tokio::time::sleep` call
/// sites that consume [`RETRY_BACKOFF`], not baked into the const, so
/// the schedule stays inspectable and the test-cfg short schedule
/// stays deterministic in sum.
// r[impl builder.fuse.retry-jitter]
fn jitter(delay: Duration) -> Duration {
    rio_common::backoff::Jitter::Proportional(0.5).apply(delay)
}

impl NixStoreFs {
    /// Ensure a store path is cached locally, fetching from remote if needed.
    ///
    /// Returns the local filesystem path to the materialized store path.
    /// If another thread is already fetching, blocks on a condition variable
    /// in heartbeat slices until that fetch completes or
    /// `self.fetch_timeout + WAIT_SLOP` passes.
    ///
    /// Thin wrapper over [`Self::ensure_cached_with_timeout`] using the
    /// flat `self.fetch_timeout`. JIT lookup (`ops.rs`) calls the
    /// `_with_timeout` variant directly with a [`jit_fetch_timeout`]-
    /// derived per-path budget.
    pub(super) fn ensure_cached(&self, store_basename: &str) -> Result<PathBuf, Errno> {
        self.ensure_cached_with_timeout(store_basename, self.fetch_timeout)
    }

    /// [`Self::ensure_cached`] with an explicit per-call fetch timeout.
    ///
    /// `fetch_timeout` bounds BOTH the gRPC stream inside
    /// `fetch_extract_insert` AND the `WaitFor` loop's overall deadline
    /// (`fetch_timeout + WAIT_SLOP`). JIT lookup passes
    /// `jit_fetch_timeout(base, nar_size)` here so a 1.9 GB input gets
    /// ≈127 s instead of the flat 60 s that I-178 showed aborts it
    /// mid-stream.
    pub(super) fn ensure_cached_with_timeout(
        &self,
        store_basename: &str,
        fetch_timeout: Duration,
    ) -> Result<PathBuf, Errno> {
        match self.cache.get_path(store_basename) {
            Ok(Some(local_path)) => {
                // Self-healing fast path: the index says present — verify
                // disk agrees. If an external rm (debugging, interrupted
                // eviction) deleted the file but left the SQLite row,
                // trusting the index here makes the path PERMANENTLY
                // unfetchable (every call returns a path that doesn't exist;
                // we never fall through to fetch). Stat is one extra syscall
                // per store-path-root lookup — cheap, and ensure_cached only
                // runs when ops.rs already missed.
                match local_path.symlink_metadata() {
                    Ok(_) => {
                        // I-110c: drop any primed hint — we won't fetch.
                        // Keeps the hint map from accumulating entries
                        // for already-cached inputs across builds.
                        let _ = self.cache.take_manifest_hint(store_basename);
                        return Ok(local_path);
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        tracing::warn!(
                            store_path = store_basename,
                            local_path = %local_path.display(),
                            "cache index says present but disk disagrees; purging stale row and re-fetching"
                        );
                        metrics::counter!("rio_builder_fuse_index_divergence_total").increment(1);
                        self.cache.remove_stale(store_basename);
                        // fall through to try_start_fetch below
                    }
                    Err(e) => {
                        // EACCES/EIO on the stat — something else is wrong.
                        // Don't silently re-fetch (would mask disk failure).
                        tracing::error!(
                            store_path = store_basename,
                            error = %e,
                            "cache stat failed (not ENOENT)"
                        );
                        return Err(Errno::EIO);
                    }
                }
            }
            Ok(None) => {} // not cached, fetch below
            Err(e) => {
                tracing::error!(store_path = store_basename, error = %e, "FUSE cache index query failed");
                return Err(Errno::EIO);
            }
        }

        // Circuit breaker: fail fast if the store is down/degraded. Placed
        // AFTER the cache-hit fast-path — cache hits don't touch the store,
        // so a build whose inputs are all cached shouldn't EIO just because
        // the store is flaky. Placed BEFORE try_start_fetch — no point
        // acquiring a singleflight claim we won't use.
        self.circuit.check()?;

        match self.cache.try_start_fetch(store_basename) {
            FetchClaim::Fetch(_guard) => {
                // We own the fetch. _guard notifies waiters on drop (success,
                // error, or panic). The _permit bounds concurrent FUSE-thread
                // fetches so at least one thread stays free for hot-path ops.
                //
                // Permit is acquired AFTER the singleflight claim, so waiters
                // for this path don't contend for a permit — they're parked
                // on _guard's condvar, which is a cheap sleep, not a block_on.
                // If acquire() blocks here, we're the (fuse_threads)th
                // concurrent fetch; the builder that triggered this lookup
                // waits, which is the lesser evil vs. starving warm builds.
                let _permit = self.fetch_sem.acquire();
                let result = fetch_extract_insert(
                    &self.cache,
                    &self.clients,
                    &self.runtime,
                    fetch_timeout,
                    store_basename,
                );
                // Record for the circuit breaker. ENOENT is NOT a failure —
                // it's the normal response to lookup() probing unknown names
                // (.lock files, tmp paths). EIO/EFBIG/timeout ARE failures.
                // The WaitFor arm does NOT record — THIS thread (the fetcher)
                // is the one recording; waiters just observe the outcome.
                match &result {
                    Ok(_) => self.circuit.record(true),
                    Err(e) if e.code() == Errno::ENOENT.code() => {
                        // Path legitimately absent. Store answered; not a
                        // circuit-breaker failure. Also a success for the
                        // wall-clock check — the store is responsive.
                        self.circuit.record(true);
                    }
                    Err(_) => self.circuit.record(false),
                }
                result
            }
            FetchClaim::WaitFor(entry) => {
                // Another thread is fetching. The fetcher has `fetch_timeout`
                // to finish; a single wait(30s) returning false means "slow",
                // not "dead" — the guard's Drop fires even on panic, so a
                // truly dead fetcher would have notified. We loop wait() as a
                // heartbeat and bound the TOTAL wait at fetch_timeout + slop.
                // If we exceed that, the fetcher's own timeout should already
                // have fired and dropped the guard; something is deeply wrong
                // (executor starvation?) and EAGAIN is the least-bad errno.
                //
                // The fetcher MAY have a different (e.g. JIT size-scaled)
                // timeout than this waiter; we wait for OUR `fetch_timeout`
                // + slop. A waiter with a shorter budget may EAGAIN while a
                // long-budget fetcher is still healthy — acceptable, the
                // kernel re-issues the lookup and the next attempt sees the
                // populated cache.
                self.wait_for_fetcher(&entry, store_basename, fetch_timeout)
            }
        }
    }

    /// Park on the singleflight condvar until the fetcher finishes or
    /// `fetch_timeout + WAIT_SLOP` passes. See the `WaitFor` arm in
    /// `ensure_cached_with_timeout`.
    fn wait_for_fetcher(
        &self,
        entry: &InflightEntry,
        store_basename: &str,
        fetch_timeout: Duration,
    ) -> Result<PathBuf, Errno> {
        let wait_deadline = fetch_timeout + WAIT_SLOP;
        let started = std::time::Instant::now();
        loop {
            if entry.wait(WAIT_SLICE) {
                break; // fetcher done (success, error, or panic — guard dropped)
            }
            if entry.is_done() {
                // Belt-and-suspenders: wait() returned false but done flipped
                // between wait_timeout_while releasing and us checking. The
                // guard dropped; proceed to the cache check.
                break;
            }
            if started.elapsed() >= wait_deadline {
                tracing::warn!(
                    store_path = store_basename,
                    waited_secs = started.elapsed().as_secs(),
                    "fetcher exceeded fetch_timeout + slop; returning EAGAIN"
                );
                return Err(Errno::EAGAIN);
            }
            tracing::debug!(
                store_path = store_basename,
                waited_secs = started.elapsed().as_secs(),
                "waiting on concurrent fetch (fetcher still working)"
            );
        }
        // Fetcher finished — check cache. Fetcher-failure ⇒ cache empty ⇒
        // EIO. I-179: it MUST NOT be ENOENT — FUSE itself wouldn't
        // negative-cache (we never reply.entry'd), but overlayfs above us
        // DOES: `ovl_lookup` caches a lower's ENOENT as a negative dentry,
        // so the daemon's retry never reaches FUSE again → permanent
        // "input does not exist" until remount. A non-ENOENT error is
        // propagated to the caller WITHOUT a negative dentry. EIO matches
        // the Fetch arm's own failure errno and the spec's "fetch failure
        // is EIO, never ENOENT" rule. Index error ⇒ also EIO.
        // r[impl builder.fuse.jit-lookup]
        match self.cache.get_path(store_basename) {
            Ok(Some(p)) => Ok(p),
            Ok(None) => {
                tracing::warn!(
                    store_path = store_basename,
                    "fetcher guard dropped with cache empty (fetcher \
                     errored or panicked); returning EIO so overlayfs \
                     does not negative-cache (I-179)"
                );
                Err(Errno::EIO)
            }
            Err(e) => {
                tracing::error!(
                    store_path = store_basename,
                    error = %e,
                    "cache index query failed after wait"
                );
                Err(Errno::EIO)
            }
        }
    }
}

/// Why a prefetch returned without fetching. Not an error — both
/// mean "somebody else is/has handling/handled it."
///
/// Exposed so the prefetch metric can distinguish the cases (cache-hit
/// vs in-flight). Both are "success" from prefetch's perspective.
#[derive(Debug, Clone, Copy)]
pub enum PrefetchSkip {
    /// `cache.get_path()` returned Some — already on disk. Cheap
    /// check, harmless.
    AlreadyCached,
    /// `try_start_fetch` returned WaitFor — FUSE or another
    /// prefetch already owns it. We DON'T wait (that's
    /// ensure_cached's job for FUSE; prefetch is a hint). Return
    /// the semaphore permit + blocking-pool thread immediately.
    AlreadyInFlight,
}

/// Prefetch a store path. SYNC — call via `spawn_blocking`.
///
/// Cache methods use `runtime.block_on` internally (designed for
/// FUSE callbacks on dedicated blocking threads). Calling from an
/// async context would panic with nested-runtime. So this fn is
/// sync and the prefetch handler wraps it in spawn_blocking.
///
/// Returns:
/// - `Ok(None)`: fetched successfully, path is now in cache
/// - `Ok(Some(skip))`: didn't fetch, someone else has/had it
/// - `Err(errno)`: fetch failed (store error, disk full, etc)
///
/// `Err` is an actual problem the operator should see in metrics.
/// The prefetch caller logs at debug (prefetch is a hint — if the store
/// is flaky, the build's own FUSE ops will surface the real error).
///
/// Singleflight: shared with ensure_cached via the same `inflight`
/// map in Cache. If FUSE is fetching when prefetch arrives, we
/// get WaitFor and return immediately. If PREFETCH is fetching
/// when FUSE arrives, FUSE waits on our guard — which is fine,
/// we're in spawn_blocking so the wait doesn't starve the async
/// executor.
pub fn prefetch_path_blocking(
    cache: &Cache,
    clients: &StoreClients,
    runtime: &Handle,
    fetch_timeout: Duration,
    store_basename: &str,
) -> Result<Option<PrefetchSkip>, Errno> {
    // Fast-path: already cached (concurrent prefetch or earlier hint).
    match cache.get_path(store_basename) {
        Ok(Some(_)) => {
            // I-110c: drop any primed hint — we won't fetch.
            let _ = cache.take_manifest_hint(store_basename);
            return Ok(Some(PrefetchSkip::AlreadyCached));
        }
        Ok(None) => {} // not cached, proceed
        Err(e) => {
            tracing::debug!(store_path = store_basename, error = %e, "prefetch: cache query failed");
            return Err(Errno::EIO);
        }
    }

    match cache.try_start_fetch(store_basename) {
        FetchClaim::Fetch(_guard) => {
            // We own it. _guard's Drop notifies FUSE waiters (if any
            // arrive while we're fetching). Same free-fn delegation
            // as ensure_cached.
            fetch_extract_insert(cache, clients, runtime, fetch_timeout, store_basename)
                .map(|_| None)
        }
        FetchClaim::WaitFor(_entry) => {
            // Someone else has it. Don't wait — we'd hold the
            // blocking-pool thread and a semaphore permit for
            // something that's already happening. The whole point
            // of prefetch is to GET AHEAD; waiting defeats that.
            //
            // Dropping _entry (not calling .wait()) is fine — it's
            // just an Arc<InflightEntry>, dropping decrements the
            // refcount. The fetcher's guard still notifies OTHER
            // waiters (FUSE threads that called ensure_cached).
            Ok(Some(PrefetchSkip::AlreadyInFlight))
        }
    }
}

/// Fetch a chunked NAR via parallel `GetChunk` and write it to `spool`
/// in chunk order. Returns bytes written.
///
/// `buffered(concurrency)` (NOT `buffer_unordered`): chunks must
/// reassemble in manifest order, but the underlying RPCs run K-at-a-time
/// — `buffered` polls up to K futures concurrently and yields results in
/// submission order. Each `GetChunk` is an independent unary-ish RPC so
/// tonic's p2c balancer picks a (possibly different) replica per call.
///
/// Per-chunk transient errors (`Unavailable`/`Unknown`/`ResourceExhausted`)
/// retry `CHUNK_RETRY_ATTEMPTS` times in the closure; non-transient
/// errors (`NotFound`/`Unimplemented`/`InvalidArgument`) bubble up
/// immediately so the caller can fall back to `GetPath`.
///
/// `max_size` enforced cumulatively (same semantics as
/// [`rio_proto::client::collect_nar_stream_to_writer`]).
// r[impl builder.fuse.fetch-chunk-fanout]
pub(super) async fn fetch_chunks_to_spool(
    chunk_client: &ChunkServiceClient<Channel>,
    chunks: Vec<ChunkRef>,
    concurrency: usize,
    timeout: Duration,
    max_size: u64,
    spool: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<u64, NarCollectError> {
    let concurrency = concurrency.max(1);
    let fut = async {
        // Per-chunk fetch (with transient retry) as a closure so the
        // priming loop and the refill below produce the same future type
        // for `FuturesOrdered`.
        let fetch = |cr: ChunkRef| {
            let mut c = chunk_client.clone();
            async move {
                let mut attempt = 0usize;
                loop {
                    match fetch_one_chunk(&mut c, &cr.hash).await {
                        Ok(b) => {
                            if attempt > 0 {
                                metrics::counter!(
                                    "rio_builder_fuse_fetch_chunks_total",
                                    "outcome" => "retry_ok"
                                )
                                .increment(1);
                            }
                            return Ok::<_, NarCollectError>(b);
                        }
                        Err(status) if rio_common::grpc::is_transient(status.code()) => {
                            attempt += 1;
                            metrics::counter!(
                                "rio_builder_fuse_fetch_chunks_total",
                                "outcome" => "retry"
                            )
                            .increment(1);
                            if attempt > CHUNK_RETRY_ATTEMPTS {
                                return Err(NarCollectError::Stream(status));
                            }
                            // No sleep: K-1 other chunks are in flight;
                            // by the time this future is polled again the
                            // transient (replica restart, brief PG-pool
                            // saturation) has likely cleared. The outer
                            // RETRY_BACKOFF in fetch_extract_insert
                            // handles the "everything is down" case.
                        }
                        Err(status) => return Err(NarCollectError::Stream(status)),
                    }
                }
            }
        };

        // Manual `FuturesOrdered` window (semantically `.buffered(K)`)
        // so that on the first error we can STOP refilling but DRAIN
        // the ≤K-1 already-in-flight RPCs to completion instead of
        // dropping them. Dropping a `buffered` stream sends one
        // RST_STREAM per in-flight h2 stream; with K=CHUNK_FETCH_
        // CONCURRENCY=32 that exceeds h2's `max_pending_accept_reset_
        // streams` guard (default 20) → server replies GOAWAY(PROTOCOL_
        // ERROR) → the SHARED channel (StoreClients::from_channel) is
        // torn down → the immediately-following GetPath fallback fails
        // Internal/Cancelled → EIO. Draining costs at most K-1 already-
        // started RPCs (the iterator below is not advanced past the
        // error), so an Unimplemented store wastes ≤31 cheap error
        // round-trips, not the whole manifest.
        let mut chunks = chunks.into_iter();
        let mut in_flight = stream::FuturesOrdered::new();
        for cr in chunks.by_ref().take(concurrency) {
            in_flight.push_back(fetch(cr));
        }

        let mut written: u64 = 0;
        let mut first_err: Option<NarCollectError> = None;
        // r[impl builder.fuse.fetch-progress-timeout]
        // I-211: `timeout` bounds the gap between chunk completions, not
        // the whole fan-out. With K=CHUNK_FETCH_CONCURRENCY in flight, "no
        // chunk completed in `timeout`" means all K stalled — same
        // store-health signal the wall-clock bound was meant to detect,
        // without aborting healthy multi-GB fetches mid-stream.
        loop {
            let r = match tokio::time::timeout(timeout, in_flight.next()).await {
                Ok(Some(r)) => r,
                Ok(None) => break,
                Err(_) => {
                    return Err(NarCollectError::Stream(tonic::Status::deadline_exceeded(
                        format!("GetChunk fan-out idle for {timeout:?} (no chunk completed)"),
                    )));
                }
            };
            match r {
                Ok(bytes) if first_err.is_none() => {
                    let new_len = written.saturating_add(bytes.len() as u64);
                    if new_len > max_size {
                        first_err = Some(NarCollectError::SizeExceeded {
                            got: new_len,
                            limit: max_size,
                        });
                        continue; // drain in-flight, don't refill
                    }
                    spool.write_all(&bytes).await?;
                    written = new_len;
                    metrics::counter!(
                        "rio_builder_fuse_fetch_chunks_total",
                        "outcome" => "ok"
                    )
                    .increment(1);
                    if let Some(cr) = chunks.next() {
                        in_flight.push_back(fetch(cr));
                    }
                }
                // Already errored: discard late-arriving Ok bytes (the
                // spool will be reset by the caller anyway). Don't refill.
                Ok(_) => {}
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                    // Don't refill — let remaining in-flight complete.
                }
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        spool.flush().await?;
        Ok::<_, NarCollectError>(written)
    };
    fut.await
}

/// Drain one `GetChunk` server-stream into a `Vec<u8>`. The store sends
/// one message (chunks ≤ CHUNK_MAX), but drain defensively in case a
/// future store splits.
async fn fetch_one_chunk(
    c: &mut ChunkServiceClient<Channel>,
    digest: &[u8],
) -> Result<Vec<u8>, tonic::Status> {
    let mut s = c
        .get_chunk(GetChunkRequest {
            digest: digest.to_vec(),
        })
        .await?
        .into_inner();
    let mut buf = Vec::new();
    while let Some(m) = s.message().await? {
        if buf.is_empty() {
            buf = m.data; // common case: one message — avoid copy.
        } else {
            buf.extend_from_slice(&m.data);
        }
    }
    Ok(buf)
}

/// True when `e` from the chunk path means "this store can't serve
/// `GetChunk` for this manifest" — fall back to `GetPath` rather than
/// surface EIO. `NotFound` (chunk GC'd / manifest stale),
/// `Unimplemented` (old store binary), `FailedPrecondition` (store is
/// inline-only — `ChunkServiceImpl::require_cache`).
fn should_fallback_to_getpath(e: &NarCollectError) -> bool {
    matches!(
        e,
        NarCollectError::Stream(s)
            if matches!(
                s.code(),
                tonic::Code::NotFound
                    | tonic::Code::Unimplemented
                    | tonic::Code::FailedPrecondition
            )
    )
}

/// The actual fetch: gRPC → NAR parse → extract to tmp → rename →
/// cache.insert. Shared by ensure_cached and prefetch.
///
/// Free fn (not a method on NixStoreFs) so prefetch can call it
/// without a NixStoreFs (which is consumed by fuser::spawn_mount2).
/// Takes the three things it actually needs: cache, client, runtime.
///
/// SYNC with internal block_on — caller is either a FUSE thread
/// (dedicated blocking) or spawn_blocking. Never call from async.
///
/// Debug-level span: this is the slow path (cache miss → gRPC + NAR
/// extract), called at most once per store path per worker lifetime.
#[instrument(level = "debug", skip(cache, clients, runtime), fields(store_basename = %store_basename))]
fn fetch_extract_insert(
    cache: &Cache,
    clients: &StoreClients,
    runtime: &Handle,
    fetch_timeout: Duration,
    store_basename: &str,
) -> Result<PathBuf, Errno> {
    fetch_extract_insert_with(
        cache,
        clients,
        runtime,
        fetch_timeout,
        store_basename,
        FetchTransport::current(),
    )
}

/// [`fetch_extract_insert`] with explicit transport. Split out so tests
/// can drive the chunk path without touching the process-global
/// `OnceLock` (first test to read it wins).
#[instrument(level = "debug", skip(cache, clients, runtime), fields(store_basename = %store_basename, ?transport))]
fn fetch_extract_insert_with(
    cache: &Cache,
    clients: &StoreClients,
    runtime: &Handle,
    fetch_timeout: Duration,
    store_basename: &str,
    transport: FetchTransport,
) -> Result<PathBuf, Errno> {
    // Increment on miss (entry to this function), not on fetch success:
    // failed fetches (store outage, NAR parse error) are still cache
    // misses. The metric should spike during store outages so dashboards
    // surface the problem; incrementing only on success hides it.
    metrics::counter!("rio_builder_fuse_cache_misses_total").increment(1);
    let store_path = format!("/nix/store/{store_basename}");

    // I-055 layer 1: validate locally before touching the wire. nixpkgs
    // bootstrap-stage glibc carries a placeholder libidn2 reference with an
    // all-`e` hash (`eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-libidn2-2.3.8`) to break
    // the glibc↔libidn2 cycle — `replace-dependency` swaps it post-build.
    // nixbase32's alphabet (`0-9a-df-np-sv-z`) excludes `e` by design, so the
    // placeholder is GUARANTEED to fail StorePath::parse. ops.rs:168's length+
    // dot check passes it (47 chars, no leading dot); without this check we'd
    // gRPC the store, get InvalidArgument → EIO → circuit.record(false). Five
    // such lookups (a single configure run touches glibc's references several
    // times) trip the breaker → ALL FUSE reads ENOENT → nix-daemon's OWN
    // dynamic loader can't find libunistring.so.5 → daemon dies → unexpected
    // EOF → MiscFailure → poison. Scheduler then marks the builder
    // store-degraded and pulls it from the assignment pool. ENOENT here flows
    // through ensure_cached:166's existing record(true) — store wasn't asked,
    // but the path is unambiguously absent, which IS a healthy answer.
    if rio_nix::store_path::StorePath::parse(&store_path).is_err() {
        return Err(Errno::ENOENT);
    }

    let local_path = cache.cache_dir().join(store_basename);

    tracing::debug!(store_path = %store_path, "fetching from remote store");

    // Fetch NAR data via gRPC (async bridged to sync). `fetch_timeout`
    // (60s default from worker.toml) is an IDLE bound — it applies to the
    // initial RPC and to each subsequent stream message, NOT the whole
    // fetch wall-clock (I-211). A stalled store still trips at 60s and
    // unparks this FUSE thread; a healthy store streaming a 2.9 GB NAR
    // completes regardless of total duration. NOT `GRPC_STREAM_TIMEOUT`
    // (300s) — FUSE is the build-critical path; uploads get the longer
    // deadline.
    //
    // Transient errors (Unavailable/Unknown — store pod restarting,
    // transport disconnect) are retried with backoff: see RETRY_BACKOFF.
    // Singleflight + fetch_sem mean only one FUSE thread per unique-path
    // is parked here; the 8s worst-case retry window doesn't starve the
    // mount. Non-transient errors (NotFound, SizeExceeded, DeadlineExceeded,
    // InvalidArgument) fail immediately — those won't fix themselves.
    // r[impl builder.fuse.fetch-bounded-memory]
    // I-180: stream NAR bytes to a same-FS spool file, then extract via
    // restore_path_streaming. Peak heap is one 256 KiB chunk + BufReader
    // (≈ <1 MiB) regardless of NAR size — previously `Vec<u8> nar_data`
    // (1.8 GB) + parsed `NarNode` tree (1.8 GB) ≈ 3.6 GB peak for the
    // LLVM source path, OOMing 1 Gi-limit builders during input fetch.
    //
    // Spool first (not stream→extract direct): keeps the gRPC retry loop
    // simple — a transient mid-stream error → truncate spool + retry; no
    // half-written extracted tree to tear down. The double-write (spool
    // + extracted tree) costs ~1-2s on local NVMe at sequential-write
    // speed for a 1.8 GB NAR; spool deleted immediately after extract.
    //
    // Spool is a SYNC `std::fs::File` wrapped as `AsyncWrite` (see
    // [`SyncSpool`]): we're already on a dedicated blocking thread, so
    // sync disk I/O in `poll_write` is correct and avoids nesting
    // `tokio::fs`'s spawn_blocking inside this `block_on` (which under
    // heavy parallel-process load surfaced as load-dependent hangs/EIO
    // in the workspace test suite).
    //
    // Spool name pattern `*.nar-<16hex>`; the scopeguard below removes
    // it on any exit. A process-kill mid-spool leaves the orphan in
    // emptyDir, which dies with the pod.
    let spool_path = cache.cache_dir().join(format!(
        "{store_basename}.nar-{:016x}",
        rand::random::<u64>()
    ));
    // Guard: remove the spool on ANY exit (success or error). Runs after
    // the `?` early-returns below; the only way to leak a spool is a hard
    // kill, which the startup sweeper handles.
    let _spool_guard = scopeguard::guard(spool_path.clone(), |p| {
        let _ = std::fs::remove_file(&p);
    });
    let mut spool = SyncSpool(std::fs::File::create(&spool_path).map_err(|e| {
        tracing::error!(spool = %spool_path.display(), error = %e, "failed to create NAR spool file");
        Errno::EIO
    })?);

    let fetch_start = std::time::Instant::now();
    let (info, transport_label) = stream_nar_to_spool(
        cache,
        clients,
        runtime,
        fetch_timeout,
        &store_path,
        store_basename,
        transport,
        &spool_path,
        &mut spool,
    )?;
    drop(spool);
    metrics::histogram!(
        "rio_builder_fuse_fetch_duration_seconds",
        "transport" => transport_label
    )
    .record(fetch_start.elapsed().as_secs_f64());
    metrics::counter!("rio_builder_fuse_fetch_bytes_total").increment(info.nar_size);

    commit_to_cache(cache, &spool_path, &local_path, &store_path, store_basename)?;

    Ok(local_path)
}

/// Stream the NAR for `store_path` into `spool`, retrying transient
/// store-gRPC errors per [`RETRY_BACKOFF`].
///
/// Tries the chunk-fanout path first when `transport == GetChunk` and a
/// chunked manifest hint is primed; otherwise (or on fallback) drives
/// `GetPath`. Returns the `PathInfo` (from the manifest hint or the
/// `GetPath` `Info` frame) plus a static label for the duration metric.
///
/// SYNC with internal `block_on` — same caller contract as
/// `fetch_extract_insert`.
#[allow(clippy::too_many_arguments)]
fn stream_nar_to_spool(
    cache: &Cache,
    clients: &StoreClients,
    runtime: &Handle,
    fetch_timeout: Duration,
    store_path: &str,
    store_basename: &str,
    transport: FetchTransport,
    spool_path: &Path,
    spool: &mut SyncSpool,
) -> Result<(rio_proto::validated::ValidatedPathInfo, &'static str), Errno> {
    // I-110c: take (not clone) — taken once before the retry loop now so
    // the chunk-fanout branch can inspect `chunks`. On transient failure
    // the hint is consumed; retries (and the GetPath fallback) go to the
    // store with `manifest_hint=None`, which re-queries PG. Deliberate: a
    // transient mid-stream means the hint may be stale.
    let hint = cache.take_manifest_hint(store_basename);

    // Only use the chunk path if we HAVE a chunked manifest. inline-blob
    // hints (chunks empty) and absent hints fall through to GetPath —
    // dataplane2's "if None: fall back to GetPath" arm.
    let mut chunk_plan = match (transport, &hint) {
        (FetchTransport::GetChunk, Some(h)) if !h.chunks.is_empty() => {
            Some((h.chunks.clone(), h.info.clone()))
        }
        _ => None,
    };
    let transport_label: &'static str = if chunk_plan.is_some() {
        "getchunk"
    } else {
        "getpath"
    };

    let info = runtime.block_on(async {
        let mut store_client = clients.store.clone();
        let mut attempt = 0;
        loop {
            // dataplane2: chunk-fanout arm. Tried at most once per
            // fetch_extract_insert call — on any non-transient failure
            // (NotFound/Unimplemented/FailedPrecondition) we clear
            // `chunk_plan` and fall through to GetPath in THIS iteration;
            // on transient, the outer retry loop re-spools via GetPath
            // (chunk_plan stays None after first take).
            if let Some((chunks, raw_info)) = chunk_plan.take() {
                let n = chunks.len();
                match fetch_chunks_to_spool(
                    &clients.chunk,
                    chunks,
                    CHUNK_FETCH_CONCURRENCY,
                    fetch_timeout,
                    rio_common::limits::MAX_NAR_SIZE,
                    &mut *spool,
                )
                .await
                {
                    Ok(written) => {
                        tracing::debug!(
                            store_path = %store_path,
                            chunks = n,
                            bytes = written,
                            "GetChunk fan-out complete"
                        );
                        // PathInfo came from the manifest hint (no Info
                        // frame on the chunk path). Validate it the same
                        // way `get_path_nar_to_file` does. Missing →
                        // treat as not-found (hint was malformed).
                        return match raw_info {
                            Some(raw) => rio_proto::validated::ValidatedPathInfo::try_from(raw)
                                .map_err(|e| {
                                    tracing::warn!(error = %e, "manifest hint PathInfo invalid");
                                    Errno::EIO
                                }),
                            None => Err(Errno::ENOENT),
                        };
                    }
                    Err(e) if should_fallback_to_getpath(&e) => {
                        tracing::warn!(
                            store_path = %store_path,
                            error = %e,
                            "GetChunk unsupported/missing; falling back to GetPath"
                        );
                        metrics::counter!(
                            "rio_builder_fuse_fetch_chunks_total",
                            "outcome" => "fallback"
                        )
                        .increment(1);
                        if let Err(e) = spool.reset() {
                            tracing::error!(spool = %spool_path.display(), error = %e, "spool truncate failed on fallback");
                            return Err(Errno::EIO);
                        }
                        // fall through to GetPath below (same iteration)
                    }
                    Err(NarCollectError::SizeExceeded { got, limit }) => {
                        tracing::error!(store_path = %store_path, size = got, limit, "NAR exceeds MAX_NAR_SIZE (chunk path)");
                        return Err(Errno::EFBIG);
                    }
                    Err(e) if e.is_transient() => {
                        // Per-chunk retries already exhausted. Reset
                        // spool, back off, and retry the whole fetch
                        // via GetPath (chunk_plan is None now).
                        if let Err(reset_e) = spool.reset() {
                            tracing::error!(spool = %spool_path.display(), error = %reset_e, "spool truncate failed");
                            return Err(Errno::EIO);
                        }
                        match RETRY_BACKOFF.get(attempt) {
                            Some(&delay) => {
                                attempt += 1;
                                tracing::warn!(
                                    store_path = %store_path, attempt, error = %e,
                                    "GetChunk transient; retrying via GetPath"
                                );
                                tokio::time::sleep(jitter(delay)).await;
                                continue;
                            }
                            None => {
                                // I-189: error! (not warn!) — this is the
                                // terminal failure that surfaces as EIO to
                                // nix-daemon. Operators grepping for ERROR
                                // need the underlying gRPC status on the
                                // same line as the EIO, not on a separate
                                // warn-level line they have to correlate.
                                tracing::error!(store_path = %store_path, error = %e, "GetChunk transient — retries exhausted → EIO");
                                return Err(Errno::EIO);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(store_path = %store_path, error = %e, "GetChunk failed → EIO");
                        return Err(Errno::EIO);
                    }
                }
            }

            match rio_proto::client::get_path_nar_to_file(
                &mut store_client,
                store_path,
                fetch_timeout,
                rio_common::limits::MAX_NAR_SIZE,
                // Hint consumed above. On attempt 0 we still have the
                // original (cloned) hint to send; on retries it's None
                // — same staleness rationale as before.
                if attempt == 0 { hint.clone() } else { None },
                &[],
                &mut *spool,
            )
            .await
            {
                Ok(Some(info)) => {
                    if attempt > 0 {
                        tracing::debug!(
                            store_path = %store_path,
                            attempt,
                            "GetPath recovered after transient failure"
                        );
                    }
                    return Ok(info);
                }
                Ok(None) => {
                    // Path not in remote store. lookup() probes unknown
                    // names (.lock files, tmp paths); ENOENT is normal.
                    return Err(Errno::ENOENT);
                }
                Err(rio_proto::client::NarCollectError::SizeExceeded { got, limit }) => {
                    tracing::error!(
                        store_path = %store_path,
                        size = got,
                        limit,
                        "NAR exceeds MAX_NAR_SIZE"
                    );
                    return Err(Errno::EFBIG);
                }
                Err(e) if e.is_not_found() => return Err(Errno::ENOENT),
                // I-055 layer 2 (defense-in-depth): InvalidArgument is a
                // per-request verdict ("malformed path"), not a store-health
                // signal. Layer 1 above catches anything StorePath::parse
                // rejects; this catches the case where the store's validator
                // is stricter than ours. ENOENT flows through ensure_cached's
                // record(true) — never trips the breaker.
                Err(e) if e.is_invalid_argument() => return Err(Errno::ENOENT),
                // Transient classification (incl. Aborted for retryable
                // PG serialization conflict, I-189) lives in
                // rio_common::grpc::is_transient.
                Err(e) if e.is_transient() => match RETRY_BACKOFF.get(attempt) {
                    Some(&delay) => {
                        attempt += 1;
                        // Spool may be partially written; reset for retry.
                        // Sync ops (we're on a blocking thread).
                        if let Err(e) = spool.reset() {
                            tracing::error!(
                                spool = %spool_path.display(), error = %e,
                                "spool truncate failed on retry"
                            );
                            return Err(Errno::EIO);
                        }
                        // I-189: jitter so the herd's retries don't
                        // re-synchronize. Logged backoff is the actual
                        // (jittered) sleep, not the schedule entry.
                        let delay = jitter(delay);
                        tracing::warn!(
                            store_path = %store_path,
                            attempt,
                            max = RETRY_BACKOFF.len(),
                            backoff = ?delay,
                            error = %e,
                            "GetPath transient failure; retrying"
                        );
                        tokio::time::sleep(delay).await;
                    }
                    None => {
                        // I-189: error! (not warn!) — terminal failure
                        // that surfaces as EIO to nix-daemon. The
                        // underlying gRPC status (h2 BrokenPipe,
                        // ResourceExhausted, …) on this line is the
                        // root cause; ops.rs's "JIT fetch failed → EIO"
                        // only has the errno.
                        tracing::error!(
                            store_path = %store_path,
                            attempts = attempt + 1,
                            error = %e,
                            "GetPath transient failure — retries exhausted → EIO (was the store pod restarted?)"
                        );
                        return Err(Errno::EIO);
                    }
                },
                Err(e) => {
                    // I-189: error! (not warn!) — terminal non-transient
                    // failure → EIO. e.g., DataLoss (chunk reassembly),
                    // DeadlineExceeded (fetch_timeout). Aborted (PG
                    // serialization conflict) is now in is_transient()
                    // and handled by the arm above.
                    tracing::error!(store_path = %store_path, error = %e, "GetPath failed → EIO");
                    return Err(Errno::EIO);
                }
            }
        }
    })?;
    Ok((info, transport_label))
}

/// Extract `spool_path` → temp sibling tree → atomic rename into
/// `local_path` → record in `cache` index.
///
/// SYNC (caller is on a blocking thread). If extraction fails mid-way
/// (disk full, corrupt NAR), the partial tree stays in the tmp dir and is
/// cleaned up on next cache init, rather than being served as a broken
/// store path by subsequent lookups.
fn commit_to_cache(
    cache: &Cache,
    spool_path: &Path,
    local_path: &Path,
    store_path: &str,
    store_basename: &str,
) -> Result<(), Errno> {
    let tmp_path = local_path.with_extension(format!("tmp-{:016x}", rand::random::<u64>()));
    std::fs::File::open(spool_path)
        .map_err(rio_nix::nar::NarError::Io)
        .and_then(|f| {
            rio_nix::nar::restore_path_streaming(
                &mut BufReader::with_capacity(64 * 1024, f),
                &tmp_path,
            )
        })
        .map_err(|e| {
            // I-189: error! — terminal failure → EIO. ENOSPC (builder
            // ephemeral-storage limit hit) lands here as NarError::Io;
            // the io::Error inside is the root cause an operator needs.
            tracing::error!(
                store_path = %store_path,
                tmp_path = %tmp_path.display(),
                error = %e,
                "NAR extraction failed → EIO"
            );
            // Best-effort: remove the partial tmp tree.
            let _ = std::fs::remove_dir_all(&tmp_path);
            Errno::EIO
        })?;
    std::fs::rename(&tmp_path, local_path).map_err(|e| {
        let _ = std::fs::remove_dir_all(&tmp_path);
        tracing::error!(
            store_path = %store_path,
            tmp_path = %tmp_path.display(),
            local_path = %local_path.display(),
            error = %e,
            "failed to rename extracted NAR into cache"
        );
        Errno::EIO
    })?;

    // Record in cache index. If this fails, the path is on disk but
    // invisible to contains() — every subsequent access would re-fetch
    // the NAR, creating an infinite re-fetch loop under DB failure.
    // Fail loudly (EIO) so the build surfaces the real problem instead
    // of silently amplifying network traffic.
    if let Err(e) = cache.insert(store_basename) {
        tracing::error!(
            store_path = %store_basename,
            local_path = %local_path.display(),
            error = %e,
            "failed to record in cache index; path on disk but untracked"
        );
        return Err(Errno::EIO);
    }

    Ok(())
}
