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

use fuser::Errno;
use tokio::runtime::Handle;
use tonic::transport::Channel;
use tracing::instrument;

use rio_proto::StoreServiceClient;

use super::NixStoreFs;
use super::cache::{Cache, FetchClaim};

impl NixStoreFs {
    /// Ensure a store path is cached locally, fetching from remote if needed.
    ///
    /// Returns the local filesystem path to the materialized store path.
    /// If another thread is already fetching, blocks on a condition variable
    /// until that fetch completes (or a 30s timeout, then returns EAGAIN).
    pub(super) fn ensure_cached(&self, store_basename: &str) -> Result<PathBuf, Errno> {
        match self.cache.get_path(store_basename) {
            Ok(Some(local_path)) => return Ok(local_path),
            Ok(None) => {} // not cached, fetch below
            Err(e) => {
                tracing::error!(store_path = store_basename, error = %e, "FUSE cache index query failed");
                return Err(Errno::EIO);
            }
        }

        match self.cache.try_start_fetch(store_basename) {
            FetchClaim::Fetch(_guard) => {
                // We own the fetch. _guard notifies waiters on drop (success,
                // error, or panic) — no explicit cleanup needed.
                // Delegate to the free fn — same code path prefetch uses.
                fetch_extract_insert(
                    &self.cache,
                    &self.store_client,
                    &self.runtime,
                    store_basename,
                )
            }
            FetchClaim::WaitFor(entry) => {
                // Another thread is fetching. Wait for it with a timeout as
                // belt-and-suspenders against a stuck fetcher (the guard's
                // Drop impl fires even on panic, so this timeout is defensive).
                const WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
                if !entry.wait(WAIT_TIMEOUT) {
                    tracing::warn!(
                        store_path = store_basename,
                        timeout_secs = WAIT_TIMEOUT.as_secs(),
                        "concurrent fetch did not complete within timeout, returning EAGAIN"
                    );
                    return Err(Errno::EAGAIN);
                }
                // Fetch completed — check cache again. Fetcher failure =>
                // Ok(None) => ENOENT so the FUSE caller can retry.
                // Index error => EIO (loud failure, not silent re-fetch).
                match self.cache.get_path(store_basename) {
                    Ok(Some(p)) => Ok(p),
                    Ok(None) => Err(Errno::ENOENT),
                    Err(e) => {
                        tracing::error!(store_path = store_basename, error = %e, "FUSE cache index query failed after wait");
                        Err(Errno::EIO)
                    }
                }
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
            fetch_extract_insert(cache, store_client, runtime, store_basename).map(|_| None)
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
    store_basename: &str,
) -> Result<PathBuf, Errno> {
    // Increment on miss (entry to this function), not on fetch success:
    // failed fetches (store outage, NAR parse error) are still cache
    // misses. The metric should spike during store outages so dashboards
    // surface the problem; incrementing only on success hides it.
    metrics::counter!("rio_worker_fuse_cache_misses_total").increment(1);
    let store_path = format!("/nix/store/{store_basename}");
    let local_path = cache.cache_dir().join(store_basename);

    tracing::debug!(store_path = %store_path, "fetching from remote store");

    // Fetch NAR data via gRPC (async bridged to sync). Timeout bounds the
    // entire fetch (initial call + stream drain) — a stalled store would
    // otherwise block this FUSE thread forever, and a few stalls exhaust
    // the FUSE thread pool and freeze the whole mount.
    let fetch_start = std::time::Instant::now();
    let nar_data = runtime.block_on(async {
        let mut client = store_client.clone();
        match rio_proto::client::get_path_nar(
            &mut client,
            &store_path,
            rio_common::grpc::GRPC_STREAM_TIMEOUT,
            rio_common::limits::MAX_NAR_SIZE,
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
    metrics::histogram!("rio_worker_fuse_fetch_duration_seconds")
        .record(fetch_start.elapsed().as_secs_f64());

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
            Cache::new(dir.path().to_path_buf(), 10)
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
            prefetch_path_blocking(&cache_cl, &client_cl, &rt, &basename_cl)
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
            prefetch_path_blocking(&cache, &client, &rt, &basename)
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
            prefetch_path_blocking(&cache, &client, &rt, &basename)
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
            prefetch_path_blocking(&cache, &client, &rt, &basename)
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
        let first =
            tokio::task::spawn_blocking(move || prefetch_path_blocking(&c1, &cl1, &r1, &b1))
                .await
                .expect("join");
        assert!(matches!(first, Ok(None)), "first fetch: {first:?}");

        // Second fetch: Ok(Some(AlreadyCached)) — fast-path skip.
        let (c2, cl2, r2, b2) = (Arc::clone(&cache), client.clone(), rt.clone(), basename);
        let second =
            tokio::task::spawn_blocking(move || prefetch_path_blocking(&c2, &cl2, &r2, &b2))
                .await
                .expect("join");
        assert!(
            matches!(second, Ok(Some(PrefetchSkip::AlreadyCached))),
            "second fetch: {second:?}"
        );
    }
}
