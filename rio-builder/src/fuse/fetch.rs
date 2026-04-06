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

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fuser::Errno;
use tokio::runtime::Handle;
use tonic::transport::Channel;
use tracing::instrument;

use rio_proto::StoreServiceClient;

use super::NixStoreFs;
use super::cache::{Cache, FetchClaim, InflightEntry};

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

impl NixStoreFs {
    /// Global deadline for the `WaitFor` loop: `fetch_timeout + WAIT_SLOP`.
    /// Was a `const` (`GRPC_STREAM_TIMEOUT` + 30s) before T1b made the
    /// fetch timeout config-driven; now computed from `self.fetch_timeout`.
    fn wait_deadline(&self) -> Duration {
        self.fetch_timeout + WAIT_SLOP
    }

    /// Ensure a store path is cached locally, fetching from remote if needed.
    ///
    /// Returns the local filesystem path to the materialized store path.
    /// If another thread is already fetching, blocks on a condition variable
    /// in heartbeat slices until that fetch completes or `wait_deadline()`
    /// (`fetch_timeout` + 30s slop) passes.
    pub(super) fn ensure_cached(&self, store_basename: &str) -> Result<PathBuf, Errno> {
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
                    Ok(_) => return Ok(local_path),
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
                    &self.store_client,
                    &self.runtime,
                    self.fetch_timeout,
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
                // (60s default) to finish; a single wait(30s) returning false
                // means "slow", not "dead" — the guard's Drop fires even on
                // panic, so a truly dead fetcher would have notified. We loop
                // wait() as a heartbeat and bound the TOTAL wait at
                // fetch_timeout + slop. If we exceed that, the fetcher's own
                // timeout should already have fired and dropped the guard;
                // something is deeply wrong (executor starvation?) and EAGAIN
                // is the least-bad errno.
                self.wait_for_fetcher(&entry, store_basename)
            }
        }
    }

    /// Park on the singleflight condvar until the fetcher finishes or the
    /// global deadline passes. See the `WaitFor` arm in `ensure_cached`.
    fn wait_for_fetcher(
        &self,
        entry: &InflightEntry,
        store_basename: &str,
    ) -> Result<PathBuf, Errno> {
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
            if started.elapsed() >= self.wait_deadline() {
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
        // Fetcher finished — check cache. Fetcher-failure ⇒ Ok(None) ⇒ ENOENT
        // (kernel will re-lookup; ATTR_TTL won't cache a negative here because
        // we never reply.entry'd). Index error ⇒ EIO (loud, not silent retry).
        match self.cache.get_path(store_basename) {
            Ok(Some(p)) => Ok(p),
            Ok(None) => Err(Errno::ENOENT),
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
    /// `cache.get_path()` returned Some — already on disk. The
    /// scheduler's bloom filter should have caught this (it doesn't
    /// send hints for paths the worker has), but bloom false
    /// negatives + stale filter (10s heartbeat interval) mean some
    /// slip through. Cheap check, harmless.
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
    store_client: &StoreServiceClient<Channel>,
    runtime: &Handle,
    fetch_timeout: Duration,
    store_basename: &str,
) -> Result<Option<PrefetchSkip>, Errno> {
    // Fast-path: already cached. Bloom should have caught this
    // scheduler-side but stale filters happen.
    match cache.get_path(store_basename) {
        Ok(Some(_)) => return Ok(Some(PrefetchSkip::AlreadyCached)),
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
            fetch_extract_insert(cache, store_client, runtime, fetch_timeout, store_basename)
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

/// The actual fetch: gRPC → NAR parse → extract to tmp → rename →
/// cache.insert → evict. Shared by ensure_cached and prefetch.
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
#[instrument(level = "debug", skip(cache, store_client, runtime), fields(store_basename = %store_basename))]
fn fetch_extract_insert(
    cache: &Cache,
    store_client: &StoreServiceClient<Channel>,
    runtime: &Handle,
    fetch_timeout: Duration,
    store_basename: &str,
) -> Result<PathBuf, Errno> {
    // Increment on miss (entry to this function), not on fetch success:
    // failed fetches (store outage, NAR parse error) are still cache
    // misses. The metric should spike during store outages so dashboards
    // surface the problem; incrementing only on success hides it.
    metrics::counter!("rio_builder_fuse_cache_misses_total").increment(1);
    let store_path = format!("/nix/store/{store_basename}");
    let local_path = cache.cache_dir().join(store_basename);

    tracing::debug!(store_path = %store_path, "fetching from remote store");

    // Fetch NAR data via gRPC (async bridged to sync). Timeout bounds the
    // entire fetch (initial call + stream drain) — a stalled store would
    // otherwise block this FUSE thread forever, and a few stalls exhaust
    // the FUSE thread pool and freeze the whole mount. Uses `fetch_timeout`
    // (60s default from worker.toml), NOT `GRPC_STREAM_TIMEOUT` (300s) —
    // FUSE is the build-critical path; uploads get the longer deadline.
    let fetch_start = std::time::Instant::now();
    let nar_data = runtime.block_on(async {
        let mut client = store_client.clone();
        match rio_proto::client::get_path_nar(
            &mut client,
            &store_path,
            fetch_timeout,
            rio_common::limits::MAX_NAR_SIZE,
            &[],
        )
        .await
        {
            Ok(Some((_info, nar))) => Ok(nar),
            Ok(None) => {
                // Path not in remote store. lookup() probes unknown names
                // (.lock files, tmp paths); ENOENT is normal here.
                Err(Errno::ENOENT)
            }
            Err(rio_proto::client::NarCollectError::SizeExceeded { got, limit }) => {
                tracing::error!(
                    store_path = %store_path,
                    size = got,
                    limit,
                    "NAR exceeds MAX_NAR_SIZE"
                );
                Err(Errno::EFBIG)
            }
            Err(e) if e.is_not_found() => Err(Errno::ENOENT),
            Err(e) => {
                tracing::warn!(store_path = %store_path, error = %e, "GetPath failed");
                Err(Errno::EIO)
            }
        }
    })?;
    metrics::histogram!("rio_builder_fuse_fetch_duration_seconds")
        .record(fetch_start.elapsed().as_secs_f64());
    metrics::counter!("rio_builder_fuse_fetch_bytes_total").increment(nar_data.len() as u64);

    // Parse and extract NAR to local disk
    let node = rio_nix::nar::parse(&mut io::Cursor::new(&nar_data)).map_err(|e| {
        tracing::warn!(store_path = %store_path, error = %e, "NAR parse failed");
        Errno::EIO
    })?;

    // Extract to a temp sibling dir, then atomically rename into place.
    // If extraction fails mid-way (disk full, etc.), the partial tree stays
    // in the tmp dir and is cleaned up on next cache init, rather than
    // being served as a broken store path by subsequent lookups.
    let tmp_path = local_path.with_extension(format!("tmp-{:016x}", rand::random::<u64>()));
    rio_nix::nar::extract_to_path(&node, &tmp_path).map_err(|e| {
        tracing::warn!(
            store_path = %store_path,
            tmp_path = %tmp_path.display(),
            error = %e,
            "NAR extraction failed"
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
    let size = dir_size(&local_path);
    if let Err(e) = cache.insert(store_basename, size) {
        tracing::error!(
            store_path = %store_basename,
            local_path = %local_path.display(),
            error = %e,
            "failed to record in cache index; path on disk but untracked"
        );
        return Err(Errno::EIO);
    }

    // Evict old entries if needed (best-effort)
    if let Err(e) = cache.evict_if_needed() {
        tracing::warn!(error = %e, "cache eviction failed");
    }

    Ok(local_path)
}

/// Recursively compute the size of a directory tree.
///
/// Returns 0 and logs on I/O error. A silent 0 on error means the cache
/// index records size=0 for a large NAR — eviction never selects it (it
/// "takes no space"), and the cache can fill past its limit. The warn!
/// makes this visible.
pub(super) fn dir_size(path: &Path) -> u64 {
    match dir_size_inner(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "dir_size failed; recording 0 (cache accounting will drift)"
            );
            0
        }
    }
}

fn dir_size_inner(path: &Path) -> io::Result<u64> {
    let meta = path.symlink_metadata()?;
    if meta.is_file() {
        return Ok(meta.len());
    }
    if !meta.is_dir() {
        // symlink, fifo, etc. — 0 contribution
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        total += dir_size_inner(&entry?.path())?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dir_size() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("a.txt"), "hello")?;
        fs::write(dir.path().join("b.txt"), "world!")?;
        fs::create_dir(dir.path().join("sub"))?;
        fs::write(dir.path().join("sub/c.txt"), "nested")?;

        let size = dir_size(dir.path());
        // 5 + 6 + 6 = 17 bytes of file content
        assert_eq!(size, 17);
        Ok(())
    }

    /// Symlinks contribute 0 (they're not regular files, not dirs).
    /// Covers the `!meta.is_dir() → Ok(0)` branch.
    #[test]
    fn test_dir_size_symlink_contributes_zero() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("real.txt"), "hello")?; // 5 bytes
        std::os::unix::fs::symlink("real.txt", dir.path().join("link"))?;

        // 5 bytes from real.txt; link contributes 0.
        assert_eq!(dir_size(dir.path()), 5);
        Ok(())
    }

    /// Nonexistent path → 0 (logged at warn!, not panic). Covers the Err arm
    /// in the outer dir_size wrapper.
    #[test]
    fn test_dir_size_nonexistent_path_returns_zero() {
        let size = dir_size(Path::new("/nonexistent/rio-test-dir-size-xyz"));
        assert_eq!(size, 0);
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
    use rio_test_support::grpc::{MockStore, spawn_mock_store_with_client};

    /// Short fetch timeout for tests — MockStore either responds instantly
    /// or is gated via Notify; no test needs the full 60s.
    const TEST_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

    /// Harness: spawn MockStore + Cache in a tempdir. Returns everything the
    /// tests need, including the runtime handle for prefetch's block_on calls.
    async fn setup_fetch_harness() -> (
        Arc<Cache>,
        StoreServiceClient<Channel>,
        MockStore,
        tempfile::TempDir,
        Handle,
        tokio::task::JoinHandle<()>,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = Arc::new(
            Cache::new(dir.path().to_path_buf(), 10, None)
                .await
                .expect("Cache::new"),
        );
        let (store, client, server_handle) = spawn_mock_store_with_client()
            .await
            .expect("spawn mock store");
        let rt = Handle::current();
        (cache, client, store, dir, rt, server_handle)
    }

    /// Seed MockStore with a valid single-file NAR → prefetch fetches,
    /// extracts to cache_dir, inserts into SQLite index → Ok(None) ("fetched").
    /// Verify the extracted file exists on disk with the right contents.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_prefetch_success_roundtrip() {
        let (cache, client, store, dir, rt, _srv) = setup_fetch_harness().await;

        // Seed: single-file NAR containing "hello". Basename must be a valid
        // nixbase32 store path basename (32-char hash + name).
        let basename = test_store_basename("fetchtest");
        let store_path = format!("/nix/store/{basename}");
        let (nar, hash) = make_nar(b"hello");
        store.seed(make_path_info(&store_path, &nar, hash), nar);

        // Call prefetch via spawn_blocking — Cache methods use block_on
        // internally, nested-runtime panics if called from async context.
        let cache_cl = Arc::clone(&cache);
        let client_cl = client.clone();
        let basename_cl = basename.clone();
        let result = tokio::task::spawn_blocking(move || {
            prefetch_path_blocking(&cache_cl, &client_cl, &rt, TEST_FETCH_TIMEOUT, &basename_cl)
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
    }

    /// MockStore has no seeded paths → GetPath returns NotFound → ENOENT.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_prefetch_not_found_returns_enoent() {
        let (cache, client, _store, _dir, rt, _srv) = setup_fetch_harness().await;

        let basename = test_store_basename("missing");
        let result = tokio::task::spawn_blocking(move || {
            prefetch_path_blocking(&cache, &client, &rt, TEST_FETCH_TIMEOUT, &basename)
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

    /// MockStore.fail_get_path = true → GetPath returns Unavailable → EIO.
    /// Covers the `Err(e) => EIO` arm in fetch_extract_insert (non-NotFound
    /// gRPC error).
    #[tokio::test(flavor = "multi_thread")]
    async fn test_prefetch_store_unavailable_returns_eio() {
        let (cache, client, store, _dir, rt, _srv) = setup_fetch_harness().await;
        store.fail_get_path.store(true, Ordering::SeqCst);

        let basename = test_store_basename("unavail");
        let result = tokio::task::spawn_blocking(move || {
            prefetch_path_blocking(&cache, &client, &rt, TEST_FETCH_TIMEOUT, &basename)
        })
        .await
        .expect("spawn_blocking join");

        let err = result.expect_err("expected Err(EIO)");
        assert_eq!(err.code(), Errno::EIO.code(), "expected EIO, got: {err:?}");
    }

    /// MockStore.get_path_garbage = true → GetPath returns valid PathInfo
    /// but garbage NAR bytes → nar::parse fails → EIO. Covers the NAR
    /// parse-error arm in fetch_extract_insert.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_prefetch_nar_parse_error_returns_eio() {
        let (cache, client, store, _dir, rt, _srv) = setup_fetch_harness().await;

        // Seed a valid PathInfo (so the MockStore lookup finds it) but
        // enable garbage mode so the NAR bytes are malformed.
        let basename = test_store_basename("garbage");
        let store_path = format!("/nix/store/{basename}");
        let (nar, hash) = make_nar(b"real");
        store.seed(make_path_info(&store_path, &nar, hash), nar);
        store.get_path_garbage.store(true, Ordering::SeqCst);

        let result = tokio::task::spawn_blocking(move || {
            prefetch_path_blocking(&cache, &client, &rt, TEST_FETCH_TIMEOUT, &basename)
        })
        .await
        .expect("spawn_blocking join");

        let err = result.expect_err("expected Err(EIO) from NAR parse failure");
        assert_eq!(err.code(), Errno::EIO.code(), "expected EIO, got: {err:?}");
    }

    /// Second prefetch of the same path returns PrefetchSkip::AlreadyCached
    /// (fast path hits cache.get_path() → Some).
    #[tokio::test(flavor = "multi_thread")]
    async fn test_prefetch_already_cached_skip() {
        let (cache, client, store, _dir, rt, _srv) = setup_fetch_harness().await;

        let basename = test_store_basename("twice");
        let store_path = format!("/nix/store/{basename}");
        let (nar, hash) = make_nar(b"x");
        store.seed(make_path_info(&store_path, &nar, hash), nar);

        // First fetch: Ok(None) — actually fetched.
        let (c1, cl1, r1, b1) = (
            Arc::clone(&cache),
            client.clone(),
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
        let (c2, cl2, r2, b2) = (Arc::clone(&cache), client.clone(), rt.clone(), basename);
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
        client: StoreServiceClient<Channel>,
        rt: Handle,
        fuse_threads: u32,
    ) -> Arc<NixStoreFs> {
        Arc::new(NixStoreFs::new(
            cache,
            client,
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
    // r[verify builder.fuse.lookup-caches]
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn test_concurrent_waiters_no_eagain_during_slow_fetch() {
        let (cache, client, store, dir, rt, _srv) = setup_fetch_harness().await;

        let basename = test_store_basename("slowfetch");
        let store_path = format!("/nix/store/{basename}");
        let (nar, hash) = make_nar(b"slow-payload");
        store.seed(make_path_info(&store_path, &nar, hash), nar);
        store.get_path_gate_armed.store(true, Ordering::SeqCst);

        // N=5 concurrent ensure_cached. One wins Fetch, four get WaitFor.
        // fuse_threads=N → permits = N-1 = 4 ≥ 1 fetcher, so the semaphore
        // doesn't interfere (only one thread fetches this singleflight path).
        const N: usize = 5;
        let fs = make_fs(Arc::clone(&cache), client, rt, N as u32);

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

    /// `ensure_cached` with a stale index row (file rm'd, SQLite row intact)
    /// detects the divergence, purges the row, re-fetches, and succeeds.
    /// Before the self-heal fix, this returned Ok(path-that-doesn't-exist)
    /// forever — every subsequent lookup would ENOENT in the caller's stat.
    ///
    // r[verify builder.fuse.cache-lru]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_ensure_cached_self_heals_index_disk_divergence() {
        let (cache, client, store, dir, rt, _srv) = setup_fetch_harness().await;

        let basename = test_store_basename("diverge");
        let store_path = format!("/nix/store/{basename}");
        let (nar, hash) = make_nar(b"heal-me");
        store.seed(make_path_info(&store_path, &nar, hash), nar);

        let fs = make_fs(Arc::clone(&cache), client, rt, 4);

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
