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

use std::io;
use std::io::BufReader;
use std::path::PathBuf;
use std::time::Duration;

use fuser::Errno;
use futures_util::stream::{self, StreamExt};
use tokio::io::AsyncWriteExt;
use tokio::runtime::Handle;
use tonic::transport::Channel;
use tracing::instrument;

use rio_proto::StoreServiceClient;
use rio_proto::client::NarCollectError;
use rio_proto::store::chunk_service_client::ChunkServiceClient;
use rio_proto::types::{ChunkRef, GetChunkRequest};

use super::NixStoreFs;
use super::cache::{Cache, FetchClaim, InflightEntry};

/// Bundles `StoreServiceClient` + `ChunkServiceClient` over the SAME
/// (typically p2c-balanced) `tonic::transport::Channel`. Clone is cheap
/// — both wrap the channel, which is `Arc`-internal.
///
/// dataplane2: the chunk-fanout fetch path needs `ChunkServiceClient`
/// alongside the existing `StoreServiceClient`. Bundling them keeps the
/// invariant that both share one balanced channel (so `GetChunk` p2c-
/// fans across the same SERVING replicas as `GetPath`) and threads
/// through every `prefetch_path_blocking` call site as one parameter.
#[derive(Clone)]
pub struct StoreClients {
    pub store: StoreServiceClient<Channel>,
    pub chunk: ChunkServiceClient<Channel>,
}

impl StoreClients {
    /// Wrap both clients over a single `Channel`. Sets the same
    /// max-message-size on both (chunks are ≤1 MiB but the headroom
    /// matches `connect_store`'s convention).
    pub fn from_channel(ch: Channel) -> Self {
        let max = rio_common::grpc::max_message_size();
        Self {
            store: StoreServiceClient::new(ch.clone())
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
            chunk: ChunkServiceClient::new(ch)
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
        }
    }
}

/// How `fetch_extract_insert` pulls NAR bytes from rio-store.
///
/// `GetPath` (default): single server-stream RPC; the store reassembles
/// chunks. Pinned to one replica per fetch; throughput bounded by the
/// store's `.buffered(8)` × chunk-size / S3-RTT.
///
/// `GetChunk` (opt-in): builder drives reassembly via parallel
/// `ChunkService.GetChunk` over the manifest hint's chunk list. Each
/// RPC is independent so the p2c balancer fans across all SERVING
/// replicas. See `r[builder.fuse.fetch-chunk-fanout]` and
/// `.stress-test/PLAN-DATAPLANE2.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FetchTransport {
    #[default]
    GetPath,
    GetChunk,
}

impl FetchTransport {
    /// Process-global transport, set once from config at startup via
    /// [`init`]. FUSE callbacks read this — they have no `Config`
    /// handle (run on `fuser`'s thread pool with only `Arc<Cache>`).
    /// Default `getpath` — chunk fan-out is opt-in until A/B'd live.
    pub fn current() -> Self {
        *CELL.get_or_init(Self::default)
    }

    /// Set the process-global transport. Call once from `main()` after
    /// config load, before mounting FUSE. Subsequent calls are no-ops.
    pub fn init(t: Self) {
        let _ = CELL.set(t);
        if t == Self::GetChunk {
            tracing::info!("FUSE fetch transport: getchunk (parallel chunk fan-out)");
        }
    }
}

static CELL: std::sync::OnceLock<FetchTransport> = std::sync::OnceLock::new();

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

    let fetch_start = std::time::Instant::now();
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
                    &mut spool,
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
                &store_path,
                fetch_timeout,
                rio_common::limits::MAX_NAR_SIZE,
                // Hint consumed above. On attempt 0 we still have the
                // original (cloned) hint to send; on retries it's None
                // — same staleness rationale as before.
                if attempt == 0 { hint.clone() } else { None },
                &[],
                &mut spool,
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
    drop(spool);
    metrics::histogram!(
        "rio_builder_fuse_fetch_duration_seconds",
        "transport" => transport_label
    )
    .record(fetch_start.elapsed().as_secs_f64());
    metrics::counter!("rio_builder_fuse_fetch_bytes_total").increment(info.nar_size);

    // Extract spool → temp sibling tree (sync — already on a blocking
    // thread), then atomically rename into place. If extraction fails
    // mid-way (disk full, corrupt NAR), the partial tree stays in the
    // tmp dir and is cleaned up on next cache init, rather than being
    // served as a broken store path by subsequent lookups.
    let tmp_path = local_path.with_extension(format!("tmp-{:016x}", rand::random::<u64>()));
    std::fs::File::open(&spool_path)
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
    std::fs::rename(&tmp_path, &local_path).map_err(|e| {
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

    Ok(local_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// I-189 Option A: jitter actually varies and stays in
    /// `[0.5×base, 1.5×base)`. Under herd, lockstep retry IS the herd;
    /// this proves the lockstep is broken.
    // r[verify builder.fuse.retry-jitter]
    #[test]
    fn test_jitter_range_and_variance() {
        let base = RETRY_BACKOFF[0];
        let lo = base.mul_f64(0.5);
        let hi = base.mul_f64(1.5);
        let samples: Vec<Duration> = (0..100).map(|_| jitter(base)).collect();
        for s in &samples {
            assert!(
                *s >= lo && *s <= hi,
                "jitter({base:?}) = {s:?} outside [{lo:?}, {hi:?}]"
            );
        }
        // Not all identical — jitter actually varied. P(100 identical
        // f64 draws) ≈ 0; if this fires, jitter() is a no-op.
        assert!(
            samples.iter().any(|s| *s != samples[0]),
            "jitter produced 100 identical samples"
        );
    }

    /// I-178: per-path JIT fetch timeout = max(base, nar_size /
    /// MIN_THROUGHPUT). A 2 GB NAR at 15 MiB/s ≈ 128 s; the base 60 s
    /// would have aborted it mid-stream → daemon ENOENT →
    /// PermanentFailure poison. A 1 KB NAR keeps the base.
    // r[verify builder.fuse.jit-lookup]
    #[test]
    fn test_jit_fetch_timeout_scales_with_nar_size() {
        let base = Duration::from_secs(60);

        // Small input: floor at base.
        assert_eq!(jit_fetch_timeout(base, 1024), base);
        assert_eq!(jit_fetch_timeout(base, 0), base);

        // 2 GB input: ceil(2_000_000_000 / 15_728_640) = 128 s > 60 s.
        let two_gb = jit_fetch_timeout(base, 2_000_000_000);
        assert!(
            two_gb >= Duration::from_secs(127),
            "2 GB @ 15 MiB/s floor must get ≥127 s, got {two_gb:?}"
        );
        assert!(
            two_gb < Duration::from_secs(200),
            "sanity upper bound (catches MIN_THROUGHPUT being lowered \
             without revisiting this test): {two_gb:?}"
        );

        // The 1.9 GB NAR from the I-178 incident.
        let i178 = jit_fetch_timeout(base, 1_901_554_624);
        assert!(
            i178 > base,
            "I-178's 1.9 GB input must exceed the 60 s base that poisoned it"
        );
    }

    // ========================================================================
    // fetch_extract_insert tests via prefetch_path_blocking
    // ========================================================================
    //
    // fetch_extract_insert is module-private; we test it through
    // prefetch_path_blocking (its public caller). prefetch is SYNC with
    // internal block_on — it MUST be called from spawn_blocking to avoid
    // nested-runtime panic (Cache methods use Handle::block_on internally).
    //
    // Multi-thread runtime required: spawn_blocking runs the closure on a
    // separate thread pool; that thread's block_on needs a worker thread
    // free on the main runtime to actually process the SQL/gRPC futures.

    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use rio_test_support::fixtures::{make_nar, make_path_info, test_store_basename};
    use rio_test_support::grpc::{MockStore, spawn_mock_store};

    /// Short fetch timeout for tests — MockStore either responds instantly
    /// or is gated via Notify; no test needs the full 60s.
    const TEST_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

    /// Harness: spawn MockStore + Cache in a tempdir. Returns everything the
    /// tests need, including the runtime handle for prefetch's block_on calls.
    /// `StoreClients` (not just `StoreServiceClient`) so the chunk-fanout
    /// path is exercisable; `MockStore` serves both services on one port.
    async fn setup_fetch_harness() -> (
        Arc<Cache>,
        StoreClients,
        MockStore,
        tempfile::TempDir,
        Handle,
        tokio::task::JoinHandle<()>,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = Arc::new(
            Cache::new(dir.path().to_path_buf())
                .await
                .expect("Cache::new"),
        );
        let (store, addr, server_handle) = spawn_mock_store().await.expect("spawn mock store");
        let ch = rio_proto::client::connect_channel(&addr.to_string())
            .await
            .expect("connect");
        let clients = StoreClients::from_channel(ch);
        let rt = Handle::current();
        (cache, clients, store, dir, rt, server_handle)
    }

    /// Seed MockStore with a valid single-file NAR → prefetch fetches,
    /// spools to a `.nar-*` tempfile, `restore_path_streaming`s to cache_dir,
    /// inserts into SQLite index → Ok(None) ("fetched"). Verify the extracted
    /// file exists on disk with the right contents and the spool is gone.
    // r[verify builder.fuse.fetch-bounded-memory]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_prefetch_success_roundtrip() {
        let (cache, clients, store, dir, rt, _srv) = setup_fetch_harness().await;

        // Seed: single-file NAR containing "hello". Basename must be a valid
        // nixbase32 store path basename (32-char hash + name).
        let basename = test_store_basename("fetchtest");
        let store_path = format!("/nix/store/{basename}");
        let (nar, hash) = make_nar(b"hello");
        store.seed(make_path_info(&store_path, &nar, hash), nar);

        // Call prefetch via spawn_blocking — Cache methods use block_on
        // internally, nested-runtime panics if called from async context.
        let cache_cl = Arc::clone(&cache);
        let clients_cl = clients.clone();
        let basename_cl = basename.clone();
        let result = tokio::task::spawn_blocking(move || {
            prefetch_path_blocking(
                &cache_cl,
                &clients_cl,
                &rt,
                TEST_FETCH_TIMEOUT,
                &basename_cl,
            )
        })
        .await
        .expect("spawn_blocking join");

        // Ok(None) means "fetched successfully" (not skipped).
        assert!(
            matches!(result, Ok(None)),
            "expected Ok(None) (fetched), got: {result:?}"
        );

        // The extracted NAR should be on disk: single-file NARs extract to a
        // plain file (not a directory) at cache_dir/basename.
        let local = dir.path().join(&basename);
        assert!(local.exists(), "extracted path should exist: {local:?}");
        let content = std::fs::read(&local).expect("read extracted file");
        assert_eq!(content, b"hello");

        // And the cache index should know about it.
        // (Use spawn_blocking — cache.contains also uses block_on.)
        let cache_cl = Arc::clone(&cache);
        let basename_cl = basename.clone();
        let contains = tokio::task::spawn_blocking(move || cache_cl.contains(&basename_cl))
            .await
            .expect("join")
            .expect("contains query");
        assert!(contains, "cache index should record the path");

        // I-180: the `.nar-*` spool tempfile must be removed post-extract
        // (scopeguard) — only the extracted tree remains in cache_dir.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_str().is_some_and(|n| n.contains(".nar-")))
            .collect();
        assert!(
            leftovers.is_empty(),
            "spool file should be removed after extract, found: {leftovers:?}"
        );
    }

    /// Directory NAR (multiple files + nested + symlink) round-trips
    /// through the spool→restore_path_streaming path. The single-file
    /// test above only exercises the regular-file branch of restore_node.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_prefetch_directory_nar() {
        use rio_nix::nar::{NarEntry, NarNode, serialize};
        use sha2::Digest;

        let (cache, clients, store, dir, rt, _srv) = setup_fetch_harness().await;

        let basename = test_store_basename("dirfetch");
        let store_path = format!("/nix/store/{basename}");
        // A small directory tree: file + executable + nested + symlink.
        let node = NarNode::Directory {
            entries: vec![
                NarEntry {
                    name: "bin".into(),
                    node: NarNode::Directory {
                        entries: vec![NarEntry {
                            name: "tool".into(),
                            node: NarNode::Regular {
                                executable: true,
                                contents: b"#!/bin/sh\necho ok\n".to_vec(),
                            },
                        }],
                    },
                },
                NarEntry {
                    name: "data.txt".into(),
                    node: NarNode::Regular {
                        executable: false,
                        contents: b"payload bytes".to_vec(),
                    },
                },
                NarEntry {
                    name: "link".into(),
                    node: NarNode::Symlink {
                        target: "data.txt".into(),
                    },
                },
            ],
        };
        let mut nar = Vec::new();
        serialize(&mut nar, &node).unwrap();
        let hash: [u8; 32] = sha2::Sha256::digest(&nar).into();
        store.seed(make_path_info(&store_path, &nar, hash), nar);

        let cache_cl = Arc::clone(&cache);
        let clients_cl = clients.clone();
        let basename_cl = basename.clone();
        let result = tokio::task::spawn_blocking(move || {
            prefetch_path_blocking(
                &cache_cl,
                &clients_cl,
                &rt,
                TEST_FETCH_TIMEOUT,
                &basename_cl,
            )
        })
        .await
        .expect("spawn_blocking join");
        assert!(
            matches!(result, Ok(None)),
            "expected fetched, got {result:?}"
        );

        let local = dir.path().join(&basename);
        assert_eq!(
            std::fs::read(local.join("data.txt")).unwrap(),
            b"payload bytes"
        );
        assert_eq!(
            std::fs::read_link(local.join("link")).unwrap(),
            std::path::Path::new("data.txt")
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(local.join("bin/tool"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "executable bit must survive restore");
    }

    /// I-055: nixpkgs bootstrap placeholder hash (`eeee…` — `e` is not in
    /// nixbase32) must short-circuit to ENOENT WITHOUT calling the store.
    /// `fail_get_path=true` arms the proof: if gRPC were called we'd see
    /// Unavailable→retry→EIO with ≥RETRY_BACKOFF total elapsed. Layer 1's
    /// pre-gRPC StorePath::parse rejects the basename → immediate ENOENT.
    /// ensure_cached:166 then records this as a breaker SUCCESS (path
    /// definitively absent = store gave a healthy answer, even though we
    /// never asked it).
    // r[verify builder.fuse.circuit-breaker+2]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_prefetch_invalid_basename_enoent_no_grpc() {
        let (cache, clients, store, _dir, rt, _srv) = setup_fetch_harness().await;
        // Arm Unavailable: if we DO call GetPath, we get retry→EIO not ENOENT.
        store.fail_get_path.store(true, Ordering::SeqCst);

        // The actual nixpkgs bootstrap placeholder. 32 `e`s — every byte
        // outside nixbase32's alphabet.
        let placeholder = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-libidn2-2.3.8";
        let start = std::time::Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            prefetch_path_blocking(&cache, &clients, &rt, TEST_FETCH_TIMEOUT, placeholder)
        })
        .await
        .expect("spawn_blocking join");

        let err = result.expect_err("expected Err(ENOENT)");
        assert_eq!(
            err.code(),
            Errno::ENOENT.code(),
            "expected ENOENT (local-validation reject), got: {err:?}. \
             EIO here means we hit the gRPC path despite the parse failure."
        );

        // No retry backoff observed → never entered the retry loop → never
        // called gRPC. test-cfg RETRY_BACKOFF totals ~760ms; sub-100ms is
        // unambiguously the pre-gRPC return.
        let elapsed = start.elapsed();
        let backoff_floor: Duration = RETRY_BACKOFF.iter().sum();
        assert!(
            elapsed < backoff_floor,
            "expected immediate return (<{backoff_floor:?}), got {elapsed:?} — \
             suggests we entered the gRPC retry loop"
        );
    }

    /// MockStore has no seeded paths → GetPath returns NotFound → ENOENT.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_prefetch_not_found_returns_enoent() {
        let (cache, clients, _store, _dir, rt, _srv) = setup_fetch_harness().await;

        let basename = test_store_basename("missing");
        let result = tokio::task::spawn_blocking(move || {
            prefetch_path_blocking(&cache, &clients, &rt, TEST_FETCH_TIMEOUT, &basename)
        })
        .await
        .expect("spawn_blocking join");

        // fuser::Errno doesn't implement PartialEq — compare via .code().
        let err = result.expect_err("expected Err(ENOENT)");
        assert_eq!(
            err.code(),
            Errno::ENOENT.code(),
            "expected ENOENT, got: {err:?}"
        );
    }

    /// MockStore.fail_get_path = true → GetPath returns Unavailable →
    /// retried RETRY_BACKOFF.len() times → still Unavailable → EIO.
    /// Covers the retry-exhausted arm in fetch_extract_insert.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_prefetch_store_unavailable_returns_eio() {
        let (cache, clients, store, _dir, rt, _srv) = setup_fetch_harness().await;
        store.fail_get_path.store(true, Ordering::SeqCst);

        let basename = test_store_basename("unavail");
        let start = std::time::Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            prefetch_path_blocking(&cache, &clients, &rt, TEST_FETCH_TIMEOUT, &basename)
        })
        .await
        .expect("spawn_blocking join");

        let err = result.expect_err("expected Err(EIO)");
        assert_eq!(err.code(), Errno::EIO.code(), "expected EIO, got: {err:?}");

        // Total backoff should have been observed (cfg(test): ~760ms ×
        // jitter ∈ [0.5, 1.5) per step → floor ~380ms). Lower-bound
        // check — proves retries happened, not just immediate EIO.
        let elapsed = start.elapsed();
        let min: Duration = RETRY_BACKOFF.iter().sum::<Duration>().mul_f64(0.5);
        assert!(
            elapsed >= min,
            "expected ≥{min:?} total backoff (proves retries fired), got {elapsed:?}"
        );
    }

    /// I-039: store pod restarts mid-build → transient Unavailable →
    /// retry recovers → build survives. MockStore.fail_get_path starts
    /// true; the test flips it false mid-retry, simulating the new pod
    /// coming Ready. Prefetch should complete successfully (Ok(None),
    /// not EIO).
    #[tokio::test(flavor = "multi_thread")]
    async fn test_prefetch_transient_unavailable_recovers() {
        let (cache, clients, store, dir, rt, _srv) = setup_fetch_harness().await;

        // Seed valid path so the post-recovery fetch has something to return.
        let basename = test_store_basename("transient");
        let store_path = format!("/nix/store/{basename}");
        let (nar, hash) = make_nar(b"survived");
        store.seed(make_path_info(&store_path, &nar, hash), nar);

        // Store starts "down". The first attempt + RETRY_BACKOFF[0] (10ms)
        // backoff + second attempt all hit Unavailable.
        store.fail_get_path.store(true, Ordering::SeqCst);

        let cache_cl = Arc::clone(&cache);
        let clients_cl = clients.clone();
        let basename_cl = basename.clone();
        let task = tokio::task::spawn_blocking(move || {
            prefetch_path_blocking(
                &cache_cl,
                &clients_cl,
                &rt,
                TEST_FETCH_TIMEOUT,
                &basename_cl,
            )
        });

        // Sleep past the first two backoffs (10ms+50ms cfg(test)) so at
        // least two retries land on the failing store, then "restart" it.
        // The third attempt (after 200ms backoff) sees the recovered store.
        tokio::time::sleep(RETRY_BACKOFF[0] + RETRY_BACKOFF[1] + Duration::from_millis(20)).await;
        store.fail_get_path.store(false, Ordering::SeqCst);

        let result = task.await.expect("spawn_blocking join");
        assert!(
            matches!(result, Ok(None)),
            "expected Ok(None) (recovered + fetched), got: {result:?}"
        );

        // The NAR should be on disk — full roundtrip completed.
        let local = dir.path().join(&basename);
        let content = std::fs::read(&local).expect("read extracted file");
        assert_eq!(content, b"survived");
    }

    /// MockStore.get_path_garbage = true → GetPath returns valid PathInfo
    /// but garbage NAR bytes → `restore_path_streaming` fails → EIO.
    /// Covers the NAR parse-error arm in fetch_extract_insert.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_prefetch_nar_parse_error_returns_eio() {
        let (cache, clients, store, _dir, rt, _srv) = setup_fetch_harness().await;

        // Seed a valid PathInfo (so the MockStore lookup finds it) but
        // enable garbage mode so the NAR bytes are malformed.
        let basename = test_store_basename("garbage");
        let store_path = format!("/nix/store/{basename}");
        let (nar, hash) = make_nar(b"real");
        store.seed(make_path_info(&store_path, &nar, hash), nar);
        store.get_path_garbage.store(true, Ordering::SeqCst);

        let result = tokio::task::spawn_blocking(move || {
            prefetch_path_blocking(&cache, &clients, &rt, TEST_FETCH_TIMEOUT, &basename)
        })
        .await
        .expect("spawn_blocking join");

        let err = result.expect_err("expected Err(EIO) from NAR parse failure");
        assert_eq!(err.code(), Errno::EIO.code(), "expected EIO, got: {err:?}");
    }

    /// I-211: a fetch whose total wall-clock exceeds `fetch_timeout`
    /// completes as long as every inter-chunk gap is below the timeout.
    /// 5 chunks × 200ms = 1s total against a 500ms idle bound — pre-I-211
    /// the 500ms wall-clock wrapper aborted at chunk 2-3 → EIO.
    // r[verify builder.fuse.fetch-progress-timeout]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_prefetch_idle_timeout_slow_but_progressing_ok() {
        let (cache, clients, store, dir, rt, _srv) = setup_fetch_harness().await;

        // 320 KiB payload → 5+ NarChunks at MockStore's 64 KiB stride.
        let payload = vec![0xab; 320 * 1024];
        let basename = test_store_basename("i211-slow");
        let store_path = format!("/nix/store/{basename}");
        let (nar, hash) = make_nar(&payload);
        store.seed(make_path_info(&store_path, &nar, hash), nar);
        store.get_path_chunk_delay_ms.store(200, Ordering::SeqCst);

        let idle = Duration::from_millis(500);
        let started = std::time::Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            prefetch_path_blocking(&cache, &clients, &rt, idle, &basename)
        })
        .await
        .expect("spawn_blocking join");

        assert!(
            matches!(result, Ok(None)),
            "I-211: total > idle timeout but per-chunk gap < idle timeout MUST succeed; got: {result:?}"
        );
        assert!(
            started.elapsed() > idle,
            "test precondition: total fetch time ({:?}) must exceed idle timeout ({idle:?}) \
             or this isn't proving the wall-clock bound is gone",
            started.elapsed()
        );
        let local = dir.path().join(test_store_basename("i211-slow"));
        let content = std::fs::read(&local).expect("read extracted file");
        assert_eq!(content.len(), payload.len());
    }

    /// I-211: a stream that goes silent for longer than `fetch_timeout`
    /// trips the idle bound on the FIRST stalled gap → DeadlineExceeded
    /// (non-transient) → EIO without retry. This is the I-165 stuck-store
    /// behavior the idle bound preserves.
    // r[verify builder.fuse.fetch-progress-timeout]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_prefetch_idle_timeout_stalled_chunk_eio() {
        let (cache, clients, store, _dir, rt, _srv) = setup_fetch_harness().await;

        let basename = test_store_basename("i211-stall");
        let store_path = format!("/nix/store/{basename}");
        let (nar, hash) = make_nar(&vec![0xcd; 128 * 1024]);
        store.seed(make_path_info(&store_path, &nar, hash), nar);
        // 800ms gap > 300ms idle bound → first NarChunk after Info trips.
        store.get_path_chunk_delay_ms.store(800, Ordering::SeqCst);

        let idle = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            prefetch_path_blocking(&cache, &clients, &rt, idle, &basename)
        })
        .await
        .expect("spawn_blocking join");

        let err = result.expect_err("expected Err(EIO) from idle timeout");
        assert_eq!(err.code(), Errno::EIO.code(), "expected EIO, got: {err:?}");
        // Tripped on the first gap, not after multiple — bound is per-chunk.
        // Allow slack for CI variance but assert it's well under the 800ms
        // gap (i.e., the receiver gave up, not the sender).
        assert!(
            started.elapsed() < Duration::from_millis(700),
            "idle timeout should trip near 300ms, took {:?}",
            started.elapsed()
        );
    }

    /// Second prefetch of the same path returns PrefetchSkip::AlreadyCached
    /// (fast path hits cache.get_path() → Some).
    #[tokio::test(flavor = "multi_thread")]
    async fn test_prefetch_already_cached_skip() {
        let (cache, clients, store, _dir, rt, _srv) = setup_fetch_harness().await;

        let basename = test_store_basename("twice");
        let store_path = format!("/nix/store/{basename}");
        let (nar, hash) = make_nar(b"x");
        store.seed(make_path_info(&store_path, &nar, hash), nar);

        // First fetch: Ok(None) — actually fetched.
        let (c1, cl1, r1, b1) = (
            Arc::clone(&cache),
            clients.clone(),
            rt.clone(),
            basename.clone(),
        );
        let first = tokio::task::spawn_blocking(move || {
            prefetch_path_blocking(&c1, &cl1, &r1, TEST_FETCH_TIMEOUT, &b1)
        })
        .await
        .expect("join");
        assert!(matches!(first, Ok(None)), "first fetch: {first:?}");

        // Second fetch: Ok(Some(AlreadyCached)) — fast-path skip.
        let (c2, cl2, r2, b2) = (Arc::clone(&cache), clients.clone(), rt.clone(), basename);
        let second = tokio::task::spawn_blocking(move || {
            prefetch_path_blocking(&c2, &cl2, &r2, TEST_FETCH_TIMEOUT, &b2)
        })
        .await
        .expect("join");
        assert!(
            matches!(second, Ok(Some(PrefetchSkip::AlreadyCached))),
            "second fetch: {second:?}"
        );
    }

    // ========================================================================
    // dataplane2: chunk-fanout transport
    // ========================================================================

    /// Seed MockStore with a chunked NAR (fixed 8-byte chunks → multiple
    /// `GetChunk` calls), prime the manifest hint, drive
    /// `fetch_extract_insert_with(transport=GetChunk)`. Asserts:
    /// - extracted file matches input bytes (order-preserving reassembly)
    /// - `GetChunk` was called once per chunk (fan-out happened)
    /// - `GetPath` was NOT called (chunk path took it)
    /// - spool cleaned up
    // r[verify builder.fuse.fetch-chunk-fanout]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_fetch_via_chunks_reassembles_correctly() {
        let (cache, clients, store, dir, rt, _srv) = setup_fetch_harness().await;

        // 80 bytes / 8-byte chunks = 10 GetChunk RPCs. Non-repeating
        // payload so an out-of-order reassembly would be detectable.
        let payload: Vec<u8> = (0u8..80).collect();
        let basename = test_store_basename("chunk-fanout");
        let store_path = format!("/nix/store/{basename}");
        let (nar, hash) = make_nar(&payload);
        let info = make_path_info(&store_path, &nar, hash);
        let chunk_refs = store.seed_chunked(info.clone(), nar.clone(), 8);
        assert!(chunk_refs.len() >= 10, "want multiple chunks");

        // Prime the hint cache (what prefetch_manifests does in prod).
        cache.prime_manifest_hints([(
            basename.clone(),
            rio_proto::types::ManifestHint {
                info: Some(info.clone().into()),
                chunks: chunk_refs.clone(),
                inline_blob: Vec::new(),
            },
        )]);

        let cache_cl = Arc::clone(&cache);
        let clients_cl = clients.clone();
        let basename_cl = basename.clone();
        let result = tokio::task::spawn_blocking(move || {
            // Explicit transport — bypasses the OnceLock'd env read.
            fetch_extract_insert_with(
                &cache_cl,
                &clients_cl,
                &rt,
                TEST_FETCH_TIMEOUT,
                &basename_cl,
                FetchTransport::GetChunk,
            )
        })
        .await
        .expect("join");
        assert!(result.is_ok(), "expected Ok(path), got: {result:?}");

        // Reassembly: extracted file = original payload.
        let local = dir.path().join(&basename);
        let content = std::fs::read(&local).expect("read extracted");
        assert_eq!(content, payload, "chunk reassembly must preserve order");

        // Fan-out: one GetChunk per manifest entry; no GetPath.
        assert_eq!(
            store.get_chunk_calls.load(Ordering::SeqCst) as usize,
            chunk_refs.len(),
            "one GetChunk per chunk"
        );
        assert!(
            store.get_path_hints.read().unwrap().is_empty(),
            "GetPath must not be called when chunk path succeeds"
        );

        // Spool cleaned up (same invariant as the GetPath test).
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_str().is_some_and(|n| n.contains(".nar-")))
            .collect();
        assert!(leftovers.is_empty(), "spool leaked: {leftovers:?}");
    }

    /// Store returns Unimplemented for `GetChunk` → fetch falls back to
    /// `GetPath` and still succeeds. Covers the "old store binary" arm.
    ///
    /// Flake-fix strategy (structural, prod bug): `chunk_size=4` over
    /// the ~128-byte NAR yields ~30 chunks → all CHUNK_FETCH_CONCURRENCY
    /// =32 slots fire at once. With the old `.buffered()`+`?` fan-out,
    /// the first Unimplemented dropped the stream → ~29 RST_STREAMs →
    /// h2's rapid-reset guard (`max_pending_accept_reset_streams`=20)
    /// sent GOAWAY(PROTOCOL_ERROR) on the shared channel → the GetPath
    /// fallback hit Internal/Cancelled → EIO ~15-20% of runs. Fixed in
    /// `fetch_chunks_to_spool` by draining in-flight on error instead of
    /// dropping. Retry/widen rejected: this was a real prod fallback-path
    /// bug (StoreClients shares one Channel — main.rs), not test noise;
    /// the small chunk size is kept deliberately as the regression guard.
    // r[verify builder.fuse.fetch-chunk-fanout]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_fetch_via_chunks_unimplemented_falls_back_to_getpath() {
        let (cache, clients, store, dir, rt, _srv) = setup_fetch_harness().await;
        store.get_chunk_unimplemented.store(true, Ordering::SeqCst);

        let basename = test_store_basename("chunk-fallback");
        let store_path = format!("/nix/store/{basename}");
        let (nar, hash) = make_nar(b"fallback-body");
        let info = make_path_info(&store_path, &nar, hash);
        // chunk_size=4 ⇒ ~30 chunks ⇒ fan-out saturates concurrency.
        // Do NOT raise this — see doc comment above.
        let chunk_refs = store.seed_chunked(info.clone(), nar.clone(), 4);
        assert!(
            chunk_refs.len() > CHUNK_FETCH_CONCURRENCY.min(20),
            "want enough chunks to saturate the fan-out window"
        );
        cache.prime_manifest_hints([(
            basename.clone(),
            rio_proto::types::ManifestHint {
                info: Some(info.into()),
                chunks: chunk_refs,
                inline_blob: Vec::new(),
            },
        )]);

        let (cache_cl, clients_cl, basename_cl) =
            (Arc::clone(&cache), clients.clone(), basename.clone());
        let result = tokio::task::spawn_blocking(move || {
            fetch_extract_insert_with(
                &cache_cl,
                &clients_cl,
                &rt,
                TEST_FETCH_TIMEOUT,
                &basename_cl,
                FetchTransport::GetChunk,
            )
        })
        .await
        .expect("join");
        assert!(
            result.is_ok(),
            "fallback to GetPath should succeed: {result:?}"
        );

        let content = std::fs::read(dir.path().join(&basename)).expect("read extracted");
        assert_eq!(content, b"fallback-body");
        assert!(
            store.get_chunk_calls.load(Ordering::SeqCst) >= 1,
            "chunk path was attempted"
        );
        assert_eq!(
            store.get_path_hints.read().unwrap().len(),
            1,
            "GetPath fallback fired exactly once"
        );
    }

    /// `FetchTransport` default + Eq. The figment env layer
    /// (`RIO_FETCH_TRANSPORT=getchunk`) goes through serde's
    /// `rename_all = "lowercase"` derive — exercised by the
    /// `jail_roundtrip!` test in config.rs.
    #[test]
    fn test_fetch_transport_default() {
        assert_eq!(FetchTransport::default(), FetchTransport::GetPath);
        assert_ne!(FetchTransport::GetPath, FetchTransport::GetChunk);
    }

    // ========================================================================
    // ensure_cached tests (remediation 16: loop-wait + self-heal + semaphore)
    // ========================================================================
    //
    // ensure_cached is a method on NixStoreFs; unlike prefetch_path_blocking
    // it needs a full fs instance (for self.fetch_sem). NixStoreFs is Send+Sync
    // (all fields are sync primitives / Arc / atomics) so Arc-wrapping lets
    // spawn_blocking share it across the test's worker threads.

    /// Build a NixStoreFs wrapped in Arc for cross-thread ensure_cached tests.
    /// `fuse_threads` controls the fetch semaphore permits (threads - 1, min 1).
    fn make_fs(
        cache: Arc<Cache>,
        clients: StoreClients,
        rt: Handle,
        fuse_threads: u32,
    ) -> Arc<NixStoreFs> {
        Arc::new(NixStoreFs::new(
            cache,
            clients,
            rt,
            false,
            fuse_threads,
            TEST_FETCH_TIMEOUT,
        ))
    }

    /// Concurrent `ensure_cached` calls for the same path during a slow fetch
    /// all succeed — none get EAGAIN. Before the loop-wait fix, waiters timed
    /// out at WAIT_TIMEOUT=30s while the fetcher was still healthy.
    ///
    /// Mechanism: MockStore's get_path is gated on a Notify. One ensure_cached
    /// wins Fetch and parks in block_on(GetPath) at the gate. The other N-1 get
    /// WaitFor and park on the condvar. We sleep past one WAIT_SLICE (200ms in
    /// cfg(test)) so each waiter does at least one heartbeat loop iteration —
    /// the point where the OLD code returned EAGAIN. Then we open the gate;
    /// the fetcher completes, guard drops, notify_all wakes all waiters, and
    /// every call returns Ok(path).
    ///
    // r[verify builder.fuse.lookup-caches+2]
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn test_concurrent_waiters_no_eagain_during_slow_fetch() {
        let (cache, clients, store, dir, rt, _srv) = setup_fetch_harness().await;

        let basename = test_store_basename("slowfetch");
        let store_path = format!("/nix/store/{basename}");
        let (nar, hash) = make_nar(b"slow-payload");
        store.seed(make_path_info(&store_path, &nar, hash), nar);
        store.get_path_gate_armed.store(true, Ordering::SeqCst);

        // N=5 concurrent ensure_cached. One wins Fetch, four get WaitFor.
        // fuse_threads=N → permits = N-1 = 4 ≥ 1 fetcher, so the semaphore
        // doesn't interfere (only one thread fetches this singleflight path).
        const N: usize = 5;
        let fs = make_fs(Arc::clone(&cache), clients, rt, N as u32);

        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let fs = Arc::clone(&fs);
            let bn = basename.clone();
            handles.push(tokio::task::spawn_blocking(move || fs.ensure_cached(&bn)));
        }

        // Let the fetcher reach the gate and the waiters park on the condvar.
        // Sleep past one WAIT_SLICE so waiters do ≥1 heartbeat iteration —
        // the OLD code returned EAGAIN here. 2× slice + margin for CI jitter.
        tokio::time::sleep(WAIT_SLICE * 2 + std::time::Duration::from_millis(100)).await;

        // Release the fetcher.
        store.get_path_gate.notify_waiters();

        // All N must succeed with the same path. Zero EAGAIN.
        let mut paths = Vec::with_capacity(N);
        for h in handles {
            let r = h.await.expect("join");
            let p = r.expect("ensure_cached must succeed (no EAGAIN)");
            paths.push(p);
        }
        assert!(
            paths.iter().all(|p| p == &paths[0]),
            "all waiters see same path: {paths:?}"
        );
        assert!(paths[0].exists(), "fetched path on disk: {:?}", paths[0]);
        assert_eq!(std::fs::read(&paths[0]).expect("read"), b"slow-payload");
        drop(dir); // keep tempdir alive to here
    }

    /// I-179: when the singleflight guard drops with the cache STILL empty
    /// (fetcher errored or panicked), `wait_for_fetcher` MUST return EIO,
    /// not ENOENT. ENOENT would be negative-cached by overlayfs above the
    /// FUSE mount → daemon's retry never reaches FUSE again → permanent
    /// "input does not exist" until remount. EIO propagates without a
    /// negative dentry, so the daemon's retry re-asks FUSE.
    ///
    /// Mechanism: take the Fetch claim manually (so we control guard
    /// lifetime), spawn a `wait_for_fetcher` waiter, drop the guard
    /// without ever inserting into the cache → waiter wakes, finds
    /// `cache.get_path == Ok(None)`, returns EIO.
    ///
    // r[verify builder.fuse.jit-lookup]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_wait_for_fetcher_guard_drop_cache_empty_is_eio() {
        let (cache, clients, _store, dir, rt, _srv) = setup_fetch_harness().await;
        let fs = make_fs(Arc::clone(&cache), clients, rt, 4);

        let basename = test_store_basename("guard-drop-eio");

        // Take the Fetch claim ourselves; we will NOT populate the cache.
        let FetchClaim::Fetch(guard) = cache.try_start_fetch(&basename) else {
            panic!("first claim must be Fetch");
        };
        // Second claim is the waiter's InflightEntry.
        let FetchClaim::WaitFor(entry) = cache.try_start_fetch(&basename) else {
            panic!("second claim must be WaitFor");
        };

        // Park a waiter on the condvar via spawn_blocking (wait_for_fetcher
        // is sync). fetch_timeout is irrelevant to the path under test —
        // the guard drop wakes the waiter long before the deadline.
        let waiter = {
            let fs = Arc::clone(&fs);
            let bn = basename.clone();
            tokio::task::spawn_blocking(move || {
                fs.wait_for_fetcher(&entry, &bn, TEST_FETCH_TIMEOUT)
            })
        };

        // Let the waiter reach the condvar (one WAIT_SLICE tick is plenty).
        tokio::time::sleep(WAIT_SLICE / 2).await;

        // Simulate fetcher failure: drop the guard WITHOUT inserting into
        // the cache. FetchGuard::drop flips `done` and notify_all()s.
        drop(guard);

        let result = waiter.await.expect("join");
        let err = result.expect_err("guard dropped with cache empty ⇒ Err");
        assert_eq!(
            err.code(),
            Errno::EIO.code(),
            "I-179: must be EIO (overlay-safe), NOT ENOENT \
             (overlay would negative-cache); got {err:?}"
        );
        // And specifically NOT the old behavior:
        assert_ne!(err.code(), Errno::ENOENT.code());

        // Sanity: cache really is empty for this basename.
        let bn = basename.clone();
        let cached = tokio::task::spawn_blocking(move || cache.get_path(&bn))
            .await
            .expect("join")
            .expect("get_path");
        assert!(cached.is_none(), "cache must be empty: {cached:?}");
        drop(dir);
    }

    /// `ensure_cached` with a stale index row (file rm'd, SQLite row intact)
    /// detects the divergence, purges the row, re-fetches, and succeeds.
    /// Before the self-heal fix, this returned Ok(path-that-doesn't-exist)
    /// forever — every subsequent lookup would ENOENT in the caller's stat.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_ensure_cached_self_heals_index_disk_divergence() {
        let (cache, clients, store, dir, rt, _srv) = setup_fetch_harness().await;

        let basename = test_store_basename("diverge");
        let store_path = format!("/nix/store/{basename}");
        let (nar, hash) = make_nar(b"heal-me");
        store.seed(make_path_info(&store_path, &nar, hash), nar);

        let fs = make_fs(Arc::clone(&cache), clients, rt, 4);

        // First fetch: populates both disk and index.
        let p1 = {
            let fs = Arc::clone(&fs);
            let bn = basename.clone();
            tokio::task::spawn_blocking(move || fs.ensure_cached(&bn))
                .await
                .expect("join")
                .expect("first fetch")
        };
        assert!(p1.exists(), "first fetch materialized on disk: {p1:?}");

        // Simulate external rm: delete the file, leave the index row intact.
        // Single-file NARs extract to a plain file (not a dir) — see
        // test_prefetch_success_roundtrip.
        std::fs::remove_file(&p1).expect("rm cache file");
        assert!(!p1.exists(), "precondition: file gone from disk");

        // Index still says present (contains() uses block_on internally).
        let still_indexed = {
            let cache = Arc::clone(&cache);
            let bn = basename.clone();
            tokio::task::spawn_blocking(move || cache.contains(&bn))
                .await
                .expect("join")
                .expect("contains query")
        };
        assert!(
            still_indexed,
            "precondition: index row survives external rm"
        );

        // Second ensure_cached: should stat the fast-path return, detect
        // ENOENT, purge the row, re-fetch, and return a VALID path.
        let p2 = {
            let fs = Arc::clone(&fs);
            let bn = basename.clone();
            tokio::task::spawn_blocking(move || fs.ensure_cached(&bn))
                .await
                .expect("join")
                .expect("second fetch (self-heal)")
        };
        assert!(p2.exists(), "self-healed path exists: {p2:?}");
        assert_eq!(std::fs::read(&p2).expect("read"), b"heal-me");

        // The index row was re-inserted by fetch_extract_insert (so the
        // self-heal's remove_stale was followed by a fresh insert).
        let reindexed = {
            let cache = Arc::clone(&cache);
            let bn = basename.clone();
            tokio::task::spawn_blocking(move || cache.contains(&bn))
                .await
                .expect("join")
                .expect("contains query")
        };
        assert!(reindexed, "index row re-inserted after self-heal fetch");
        drop(dir);
    }
}
