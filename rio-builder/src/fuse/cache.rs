//! Local cache for FUSE store paths backed by the pod's emptyDir.
//!
//! Cached store paths are materialized as directory trees on disk (not stored
//! as NAR blobs). On cache miss the worker fetches the NAR via `GetPath`,
//! parses it, and extracts it to disk via [`rio_nix::nar::extract_to_path`].
//!
//! A lightweight in-memory SQLite index tracks which paths are cached. There
//! is no eviction: builders are ephemeral (one build per pod), so the cache
//! lives exactly as long as the input closure it holds and is discarded with
//! the pod's emptyDir.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::runtime::Handle;

/// Errors from cache operations.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] sqlx::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Per-path coordination for in-flight fetches.
///
/// Waiters block on `cv` until `done` flips to true.
pub struct InflightEntry {
    done: Mutex<bool>,
    cv: Condvar,
}

impl InflightEntry {
    /// Block the current thread until the fetch completes or `timeout` elapses.
    ///
    /// Returns `true` if the fetch completed, `false` on timeout. A `false`
    /// return does NOT mean the fetcher is dead — the guard's `Drop` impl
    /// fires on success, error, and panic alike. `false` means only "still
    /// working after `timeout`." Callers that need to distinguish "slow"
    /// from "dead" should loop on `wait()` while [`Self::is_done`] stays
    /// `false`; the fetcher's own timeout (`GRPC_STREAM_TIMEOUT`) is the
    /// real deadline.
    pub fn wait(&self, timeout: Duration) -> bool {
        let done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        let (done, wait_result) = self
            .cv
            .wait_timeout_while(done, timeout, |d| !*d)
            .unwrap_or_else(|e| e.into_inner());
        !wait_result.timed_out() && *done
    }

    /// Cheap check: has the fetcher finished (guard dropped)? No condvar wait.
    /// Use after a timed-out `wait()` to decide whether to wait again.
    pub fn is_done(&self) -> bool {
        *self.done.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Outcome of [`Cache::try_start_fetch`].
pub enum FetchClaim<'a> {
    /// Caller owns the fetch. The guard notifies all waiters when dropped,
    /// regardless of fetch success or failure.
    Fetch(FetchGuard<'a>),
    /// Another thread is already fetching. Caller should wait on the entry.
    WaitFor(Arc<InflightEntry>),
}

/// RAII guard for an in-flight fetch claim.
///
/// On drop, removes the path from the inflight map and notifies all waiters.
/// This fires even if the fetcher panics, ensuring waiters are never stuck
/// indefinitely (they also have a belt-and-suspenders timeout).
pub struct FetchGuard<'a> {
    cache: &'a Cache,
    path: String,
}

impl Drop for FetchGuard<'_> {
    fn drop(&mut self) {
        let entry = {
            let mut inflight = self
                .cache
                .inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            inflight.remove(&self.path)
        };
        if let Some(entry) = entry {
            *entry.done.lock().unwrap_or_else(|e| e.into_inner()) = true;
            entry.cv.notify_all();
        }
    }
}

/// Blocking counting semaphore for FUSE-thread fetch concurrency.
///
/// `tokio::sync::Semaphore` is async; we're in a sync FUSE callback that
/// hasn't entered `block_on` yet. Building on `Mutex+Condvar` (same as
/// `InflightEntry`) avoids a dependency and a nested-block_on wart.
///
/// No `try_acquire` — a FUSE-initiated fetch MUST eventually happen (the
/// build depends on it). We always block; the semaphore just serializes
/// the rate so some FUSE threads stay free for the hot path.
pub(super) struct FetchSemaphore {
    permits: Mutex<usize>,
    cv: Condvar,
}

impl FetchSemaphore {
    pub(super) fn new(permits: usize) -> Self {
        Self {
            permits: Mutex::new(permits),
            cv: Condvar::new(),
        }
    }

    pub(super) fn acquire(&self) -> FetchPermit<'_> {
        let mut p = self.permits.lock().unwrap_or_else(|e| e.into_inner());
        while *p == 0 {
            p = self.cv.wait(p).unwrap_or_else(|e| e.into_inner());
        }
        *p -= 1;
        FetchPermit { sem: self }
    }
}

pub(super) struct FetchPermit<'a> {
    sem: &'a FetchSemaphore,
}

impl Drop for FetchPermit<'_> {
    fn drop(&mut self) {
        *self.sem.permits.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.sem.cv.notify_one();
    }
}

/// Local cache manager backed by the pod's emptyDir with an in-memory
/// SQLite index.
///
/// Thread-safe: uses a connection pool for concurrent access. FUSE callbacks
/// are synchronous, so all DB ops are bridged via `Handle::block_on`.
pub struct Cache {
    /// Root directory where cached paths are materialized.
    cache_dir: PathBuf,
    /// SQLite metadata index (async pool, bridged via block_on).
    pool: SqlitePool,
    /// Tokio runtime handle for block_on bridging.
    runtime: Handle,
    /// In-flight fetches with per-path condition variables for waiter notification.
    inflight: Mutex<HashMap<String, Arc<InflightEntry>>>,
    /// I-110c: per-path manifest hints, keyed by store basename.
    /// Primed by the executor (one `BatchGetManifest` before daemon
    /// spawn); consumed by `fetch_extract_insert` (which removes on
    /// read so memory doesn't accumulate across builds). When a hint
    /// is present, `GetPath` carries it and the store skips its two
    /// PG lookups — the S3 chunk fetch still happens per-path, but
    /// PG sees ≤2 queries/builder for the whole input closure instead
    /// of ~1600.
    ///
    /// `Mutex<HashMap>` like `inflight` — short critical sections
    /// (insert/remove only), called from FUSE threads + the executor.
    manifest_hints: Mutex<HashMap<String, rio_proto::types::ManifestHint>>,
    /// JIT-fetch allowlist: store basenames the current build's input
    /// closure contains, with their NAR sizes (for size-aware fetch
    /// timeout). Populated by [`Self::register_inputs`] (executor,
    /// after `compute_input_closure`, before daemon spawn). FUSE
    /// `lookup()` consults it via [`Self::jit_classify`]: present →
    /// block-and-fetch; absent → fast ENOENT. NEVER returns ENOENT
    /// for a present entry — fetch failure → EIO so overlay doesn't
    /// negative-cache (the I-043 redesign).
    ///
    /// `RwLock<Option<_>>`:
    ///   `None`  → JIT not armed (`register_inputs` not yet called —
    ///             the warm-gate prefetch batch and tests): lookup
    ///             returns fast ENOENT.
    ///   `Some`  → JIT armed: ONLY names in the map are fetched.
    ///
    /// Lives on `Cache` (not `NixStoreFs`) for the same reason as
    /// `manifest_hints`: `NixStoreFs` is consumed by
    /// `fuser::spawn_mount2`; the executor only holds `Arc<Cache>`.
    /// Not thread-local: FUSE callbacks run on `fuser`'s own thread
    /// pool, not the executor's tokio worker.
    known_inputs: RwLock<Option<HashMap<String, u64 /* nar_size */>>>,
}

/// Classification of a top-level FUSE `lookup` name against the
/// per-build JIT allowlist. See [`Cache::jit_classify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitClass {
    /// JIT not armed (`register_inputs` not yet called). `lookup`
    /// returns fast ENOENT; `handle_prefetch_hint` applies the
    /// warm-gate size cap.
    NotArmed,
    /// JIT armed; name is NOT a registered input. Daemon probes
    /// (`.lock`, `.chroot`, output-path checks) land here. Fast ENOENT,
    /// no store contact.
    NotInput,
    /// JIT armed; name IS a registered input. `lookup` MUST block on
    /// fetch and on any failure return EIO (never ENOENT — overlay
    /// would negative-cache it).
    KnownInput { nar_size: u64 },
}

impl Cache {
    /// Create a new cache rooted at `cache_dir`.
    ///
    /// Creates the cache directory and SQLite index if they don't exist.
    /// Must be called from within a tokio runtime (captures the current `Handle`).
    ///
    pub async fn new(cache_dir: PathBuf) -> Result<Self, CacheError> {
        std::fs::create_dir_all(&cache_dir)?;

        let runtime = Handle::current();

        // The pod's emptyDir filesystem is discarded at exit, so the
        // cache index lives only in memory — persisting it on tiny-class
        // node storage cost >1s per write under load (I-141: ~10s wasted
        // per build).
        //
        // r[impl builder.fuse.cache-ephemeral-memory]
        // :memory: with a pool: each pooled connection would be a
        // SEPARATE in-memory DB. Pin to one connection and disable
        // idle/lifetime reaping (closing it would drop the DB and lose
        // the index mid-build). Single-connection serialization is fine
        // — in-memory SQLite ops are µs-scale and FUSE already
        // serializes on the inflight map for the hot path.
        let pool = SqlitePoolOptions::new()
            .min_connections(1)
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(SqliteConnectOptions::new().filename(":memory:"))
            .await?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS cached_paths (
                store_path TEXT PRIMARY KEY NOT NULL
            )"#,
        )
        .execute(&pool)
        .await?;

        Ok(Self {
            cache_dir,
            pool,
            runtime,
            inflight: Mutex::new(HashMap::new()),
            manifest_hints: Mutex::new(HashMap::new()),
            known_inputs: RwLock::new(None),
        })
    }

    /// Arm JIT fetch for the upcoming build. `inputs` is the
    /// `(basename, nar_size)` projection of `compute_input_closure`'s
    /// result (already computed as `input_sized` at the executor).
    ///
    /// EXTENDS the existing map (no implicit clear): store paths are
    /// immutable. Memory: ~60 B/entry × ~1k paths for the one build.
    // r[impl builder.fuse.jit-register]
    pub fn register_inputs(&self, inputs: impl IntoIterator<Item = (String, u64)>) {
        let mut g = self.known_inputs.write().unwrap_or_else(|e| e.into_inner());
        g.get_or_insert_with(HashMap::new).extend(inputs);
    }

    /// Classify `basename` against the JIT allowlist. See [`JitClass`].
    /// `None` → not armed (warm-gate prefetch window). Armed + present →
    /// block-and-fetch with size-aware timeout. Armed + absent → fast
    /// ENOENT.
    pub fn jit_classify(&self, basename: &str) -> JitClass {
        match &*self.known_inputs.read().unwrap_or_else(|e| e.into_inner()) {
            None => JitClass::NotArmed,
            Some(m) => match m.get(basename) {
                Some(&nar_size) => JitClass::KnownInput { nar_size },
                None => JitClass::NotInput,
            },
        }
    }

    /// Current size of the JIT allowlist (`None` → 0). For the
    /// `rio_builder_jit_inputs_registered` gauge.
    pub fn known_inputs_len(&self) -> usize {
        self.known_inputs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or(0, HashMap::len)
    }

    /// I-110c: prime the manifest-hint map. Called once per build by
    /// the executor (after `BatchGetManifest`), before the FUSE-warm
    /// stat loop. Keys are store BASENAMES (`abc..-hello`, not
    /// `/nix/store/abc..-hello`) — that's what `fetch_extract_insert`
    /// has in hand.
    pub fn prime_manifest_hints(
        &self,
        hints: impl IntoIterator<Item = (String, rio_proto::types::ManifestHint)>,
    ) {
        let mut map = self
            .manifest_hints
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.extend(hints);
    }

    /// I-110c: take (remove) the hint for `store_basename`, if any.
    /// Removed on read — once the path is fetched the hint is dead
    /// weight, and concurrent builds churn the closure set so leaving
    /// entries would grow unbounded.
    pub fn take_manifest_hint(
        &self,
        store_basename: &str,
    ) -> Option<rio_proto::types::ManifestHint> {
        self.manifest_hints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(store_basename)
    }

    /// Root directory where cached paths are materialized.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Check if a store path is cached.
    ///
    /// Propagates SQLite errors so callers can distinguish "not cached"
    /// from "index query failed". Treating DB errors as not-cached would
    /// trigger re-fetches for every FUSE op during a SQLite hiccup,
    /// saturating store bandwidth and masking the root cause.
    #[cfg(test)]
    pub fn contains(&self, store_path: &str) -> Result<bool, CacheError> {
        let pool = &self.pool;
        self.runtime.block_on(async {
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM cached_paths WHERE store_path = ?1")
                    .bind(store_path)
                    .fetch_one(pool)
                    .await?;
            Ok(count > 0)
        })
    }

    /// Get the full filesystem path for a cached store path.
    ///
    /// Returns `Ok(None)` if the path is not cached, `Err` on index failure.
    pub fn get_path(&self, store_path: &str) -> Result<Option<PathBuf>, CacheError> {
        let pool = &self.pool;
        let present: bool = self.runtime.block_on(async {
            let row = sqlx::query("SELECT 1 FROM cached_paths WHERE store_path = ?1")
                .bind(store_path)
                .fetch_optional(pool)
                .await?;
            Ok::<_, CacheError>(row.is_some())
        })?;
        Ok(present.then(|| self.cache_dir.join(store_path)))
    }

    /// Remove a stale index row. Called when the index says "present" but
    /// the file is gone from disk (external rm, interrupted eviction).
    /// Best-effort: logs on failure but doesn't propagate — if the DELETE
    /// fails, the next fetch will `INSERT OR REPLACE` over it anyway.
    pub fn remove_stale(&self, store_path: &str) {
        let pool = &self.pool;
        if let Err(e) = self.runtime.block_on(async {
            sqlx::query("DELETE FROM cached_paths WHERE store_path = ?1")
                .bind(store_path)
                .execute(pool)
                .await
        }) {
            tracing::warn!(
                store_path,
                error = %e,
                "failed to remove stale index row (will be overwritten on re-fetch)"
            );
        }
    }

    /// Record a store path as cached after extraction.
    pub fn insert(&self, store_path: &str) -> Result<(), CacheError> {
        let pool = &self.pool;
        self.runtime.block_on(async {
            sqlx::query("INSERT OR REPLACE INTO cached_paths (store_path) VALUES (?1)")
                .bind(store_path)
                .execute(pool)
                .await?;
            Ok::<_, sqlx::Error>(())
        })?;
        Ok(())
    }

    /// Try to claim responsibility for fetching a path.
    ///
    /// Returns [`FetchClaim::Fetch`] if the path was not already in-flight;
    /// the caller must perform the fetch. The returned guard notifies all
    /// waiters on drop (including on panic).
    ///
    /// Returns [`FetchClaim::WaitFor`] if another thread is already fetching;
    /// the caller should block on [`InflightEntry::wait`].
    pub fn try_start_fetch(&self, store_path: &str) -> FetchClaim<'_> {
        use std::collections::hash_map::Entry;
        let mut inflight = self.inflight.lock().unwrap_or_else(|e| {
            tracing::error!("inflight lock poisoned, recovering");
            e.into_inner()
        });
        match inflight.entry(store_path.to_string()) {
            Entry::Occupied(e) => FetchClaim::WaitFor(Arc::clone(e.get())),
            Entry::Vacant(e) => {
                e.insert(Arc::new(InflightEntry {
                    done: Mutex::new(false),
                    cv: Condvar::new(),
                }));
                FetchClaim::Fetch(FetchGuard {
                    cache: self,
                    path: store_path.to_string(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Cache sync methods use `block_on`, so tests create the cache in async
    /// context then exercise the sync methods via `spawn_blocking`.
    async fn make_cache(cache_dir: PathBuf) -> anyhow::Result<Arc<Cache>> {
        Ok(Arc::new(Cache::new(cache_dir).await?))
    }

    #[tokio::test]
    async fn test_cache_new_creates_dir_memory_index() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache_dir = dir.path().join("cache");

        let cache = make_cache(cache_dir.clone()).await?;
        // cache_dir is created (NAR trees land here); the SQLite index
        // is in :memory: only.
        assert!(cache_dir.exists());
        assert!(
            !cache_dir.join("cache_index.sqlite").exists(),
            "index must be in-memory, not on disk"
        );

        let present = tokio::task::spawn_blocking(move || cache.contains("nope")).await??;
        assert!(!present);
        Ok(())
    }

    /// JIT allowlist: unarmed → NotArmed; register → KnownInput /
    /// NotInput. Second register EXTENDS, not replaces.
    // r[verify builder.fuse.jit-register]
    #[tokio::test]
    async fn test_jit_classify_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = make_cache(dir.path().join("cache")).await?;

        let hello = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello";
        let world = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-world";
        let probe = "cccccccccccccccccccccccccccccccc-out.drv.lock";

        // Unarmed: everything is NotArmed (warm-gate prefetch window).
        assert_eq!(cache.jit_classify(hello), JitClass::NotArmed);
        assert_eq!(cache.jit_classify(probe), JitClass::NotArmed);
        assert_eq!(cache.known_inputs_len(), 0);

        // Arm with one input.
        cache.register_inputs([(hello.to_owned(), 1024)]);
        assert_eq!(
            cache.jit_classify(hello),
            JitClass::KnownInput { nar_size: 1024 }
        );
        assert_eq!(cache.jit_classify(world), JitClass::NotInput);
        assert_eq!(cache.jit_classify(probe), JitClass::NotInput);
        assert_eq!(cache.known_inputs_len(), 1);

        // Second register EXTENDS (hello stays, world added).
        cache.register_inputs([(world.to_owned(), 2_000_000_000)]);
        assert_eq!(
            cache.jit_classify(hello),
            JitClass::KnownInput { nar_size: 1024 }
        );
        assert_eq!(
            cache.jit_classify(world),
            JitClass::KnownInput {
                nar_size: 2_000_000_000
            }
        );
        assert_eq!(cache.known_inputs_len(), 2);
        Ok(())
    }

    /// Index round-trips across the pinned single :memory: connection.
    /// Guards against a future refactor that lets the pool open a second
    /// :memory: connection (which would be a separate, empty DB).
    // r[verify builder.fuse.cache-ephemeral-memory]
    #[tokio::test]
    async fn test_cache_insert_and_contains() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = make_cache(dir.path().join("cache")).await?;

        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            assert!(!cache.contains("abc-hello-1.0")?);

            cache.insert("abc-hello-1.0")?;
            assert!(cache.contains("abc-hello-1.0")?);
            Ok(())
        })
        .await??;
        Ok(())
    }

    #[tokio::test]
    async fn test_cache_get_path() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache_dir = dir.path().join("cache");
        let cache = make_cache(cache_dir.clone()).await?;

        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            assert!(cache.get_path("abc-hello-1.0")?.is_none());

            cache.insert("abc-hello-1.0")?;
            let path = cache.get_path("abc-hello-1.0")?.expect("just inserted");
            assert_eq!(path, cache_dir.join("abc-hello-1.0"));
            Ok(())
        })
        .await??;
        Ok(())
    }

    /// DB errors from contains()/get_path() must propagate, not be treated
    /// as "not cached". Otherwise a SQLite hiccup would trigger re-fetches
    /// for every FUSE op, saturating store bandwidth and masking root cause.
    #[tokio::test]
    async fn test_cache_db_error_propagates() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = make_cache(dir.path().join("cache")).await?;

        // Close the pool to force DB errors on all subsequent queries.
        let pool = cache.pool.clone();
        pool.close().await;

        tokio::task::spawn_blocking(move || {
            // contains() must return Err, not false.
            let result = cache.contains("some-path");
            assert!(
                matches!(result, Err(CacheError::Sqlite(_))),
                "contains() on closed pool should return Err, got {result:?}"
            );

            // get_path() must return Err, not None.
            let result = cache.get_path("some-path");
            assert!(
                matches!(result, Err(CacheError::Sqlite(_))),
                "get_path() on closed pool should return Err, got {result:?}"
            );
        })
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_inflight_claim_and_notify() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = make_cache(dir.path().join("cache")).await?;

        // First claim should succeed.
        let FetchClaim::Fetch(guard) = cache.try_start_fetch("abc-path") else {
            panic!("first claim should return Fetch");
        };
        // Second claim should get WaitFor.
        let FetchClaim::WaitFor(entry) = cache.try_start_fetch("abc-path") else {
            panic!("second claim should return WaitFor");
        };

        // Drop guard → should notify. Waiter should see completion immediately.
        drop(guard);
        assert!(entry.wait(Duration::from_millis(100)));

        // After guard drop, path is no longer inflight — can claim again.
        let FetchClaim::Fetch(_) = cache.try_start_fetch("abc-path") else {
            panic!("should be able to re-claim after guard drop");
        };
        Ok(())
    }

    #[tokio::test]
    async fn test_inflight_concurrent_wait() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::time::Instant;

        let dir = tempfile::tempdir()?;
        let cache = make_cache(dir.path().join("cache")).await?;
        let fetch_count = Arc::new(AtomicU32::new(0));

        // Two threads race to fetch the same path. Exactly one should get
        // Fetch; the other should WaitFor and be woken promptly by the
        // condvar notify, not time out.
        let (c1, f1) = (Arc::clone(&cache), Arc::clone(&fetch_count));
        let (c2, f2) = (cache, Arc::clone(&fetch_count));

        let t1 = std::thread::spawn(move || do_claim(&c1, &f1));
        let t2 = std::thread::spawn(move || do_claim(&c2, &f2));

        let (d1, d2) = (t1.join().expect("thread"), t2.join().expect("thread"));

        // Exactly one fetch happened.
        assert_eq!(fetch_count.load(Ordering::SeqCst), 1);
        // Both threads finished in roughly the fetcher's sleep time (200ms),
        // not the old backoff total (1.4s). Generous bound for CI.
        assert!(d1 < Duration::from_millis(800), "t1 took {d1:?}");
        assert!(d2 < Duration::from_millis(800), "t2 took {d2:?}");

        fn do_claim(cache: &Cache, fetch_count: &AtomicU32) -> Duration {
            let start = Instant::now();
            match cache.try_start_fetch("race-path") {
                FetchClaim::Fetch(_guard) => {
                    fetch_count.fetch_add(1, Ordering::SeqCst);
                    // Simulate a fetch taking 200ms.
                    std::thread::sleep(Duration::from_millis(200));
                    // _guard drops here, notifying the waiter.
                }
                FetchClaim::WaitFor(entry) => {
                    assert!(
                        entry.wait(Duration::from_secs(5)),
                        "waiter should be notified"
                    );
                }
            }
            start.elapsed()
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_inflight_wait_timeout() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = make_cache(dir.path().join("cache")).await?;

        // Claim the fetch but never drop the guard.
        let FetchClaim::Fetch(guard) = cache.try_start_fetch("stuck-path") else {
            panic!("first claim should return Fetch");
        };
        let FetchClaim::WaitFor(entry) = cache.try_start_fetch("stuck-path") else {
            panic!("second claim should return WaitFor");
        };

        // Wait should time out since the guard is never dropped.
        let start = std::time::Instant::now();
        assert!(!entry.wait(Duration::from_millis(100)));
        assert!(start.elapsed() >= Duration::from_millis(100));

        drop(guard);
        Ok(())
    }

    /// `is_done()` reflects guard-drop state without waiting on the condvar.
    /// Before guard drop: `false`. After guard drop: `true`.
    #[tokio::test]
    async fn test_inflight_is_done_tracks_guard_drop() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = make_cache(dir.path().join("cache")).await?;

        let FetchClaim::Fetch(guard) = cache.try_start_fetch("done-path") else {
            panic!("first claim should return Fetch");
        };
        let FetchClaim::WaitFor(entry) = cache.try_start_fetch("done-path") else {
            panic!("second claim should return WaitFor");
        };

        // Guard still held: not done.
        assert!(!entry.is_done(), "is_done should be false while guard held");

        // wait() times out → false. is_done() is STILL false (fetcher working).
        assert!(!entry.wait(Duration::from_millis(50)));
        assert!(!entry.is_done(), "is_done still false after timed-out wait");

        // Drop the guard: now done.
        drop(guard);
        assert!(entry.is_done(), "is_done should be true after guard drop");

        // And wait() now returns immediately true.
        assert!(entry.wait(Duration::from_secs(1)));
        Ok(())
    }

    /// FetchSemaphore: permits are consumed by acquire, restored on permit drop.
    /// Second acquire blocks until first permit drops.
    #[test]
    fn test_fetch_semaphore_permit_raii() {
        let sem = Arc::new(FetchSemaphore::new(1));

        // First acquire succeeds immediately.
        let p1 = sem.acquire();

        // Second acquire on another thread blocks until p1 drops.
        let sem2 = Arc::clone(&sem);
        let started = std::time::Instant::now();
        let t = std::thread::spawn(move || {
            let _p2 = sem2.acquire();
            started.elapsed()
        });

        // Hold p1 for 100ms, then drop.
        std::thread::sleep(Duration::from_millis(100));
        drop(p1);

        // t should have waited ~100ms for the permit.
        let waited = t.join().expect("thread");
        assert!(
            waited >= Duration::from_millis(90),
            "second acquire should have blocked, waited only {waited:?}"
        );

        // Both permits released: a fresh acquire succeeds immediately.
        let start = std::time::Instant::now();
        let _p3 = sem.acquire();
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "acquire with available permit should be immediate"
        );
    }
}
