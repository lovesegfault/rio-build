//! LRU cache for FUSE store paths backed by local SSD.
//!
//! Cached store paths are materialized as directory trees on disk (not stored
//! as NAR blobs). On cache miss the worker fetches the NAR via `GetPath`,
//! parses it, and extracts it to disk via [`rio_nix::nar::extract_to_path`].
//!
//! A lightweight SQLite index tracks cached paths, sizes, and access
//! timestamps for LRU eviction decisions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::runtime::Handle;

/// Errors from cache operations.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] sqlx::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("NAR error: {0}")]
    Nar(#[from] rio_nix::nar::NarError),
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
    /// Returns `true` if the fetch completed, `false` on timeout.
    pub fn wait(&self, timeout: Duration) -> bool {
        let done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        let (done, wait_result) = self
            .cv
            .wait_timeout_while(done, timeout, |d| !*d)
            .unwrap_or_else(|e| e.into_inner());
        !wait_result.timed_out() && *done
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

/// Metadata for a cached store path.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// The store path basename (e.g. "abc...-hello-1.0").
    pub store_path: String,
    /// Size in bytes on disk.
    pub size_bytes: u64,
    /// Unix timestamp of last access.
    pub last_access: i64,
}

/// LRU cache manager backed by local SSD with SQLite metadata index.
///
/// Thread-safe: uses a connection pool for concurrent access. FUSE callbacks
/// are synchronous, so all DB ops are bridged via `Handle::block_on`.
pub struct Cache {
    /// Root directory where cached paths are materialized.
    cache_dir: PathBuf,
    /// Maximum cache size in bytes.
    max_size_bytes: u64,
    /// SQLite metadata index (async pool, bridged via block_on).
    pool: SqlitePool,
    /// Tokio runtime handle for block_on bridging.
    runtime: Handle,
    /// In-flight fetches with per-path condition variables for waiter notification.
    inflight: Mutex<HashMap<String, Arc<InflightEntry>>>,
}

impl Cache {
    /// Create a new cache rooted at `cache_dir` with the given size limit.
    ///
    /// Creates the cache directory and SQLite index if they don't exist.
    /// Must be called from within a tokio runtime (captures the current `Handle`).
    pub async fn new(cache_dir: PathBuf, max_size_gb: u64) -> Result<Self, CacheError> {
        std::fs::create_dir_all(&cache_dir)?;

        // Clean up stale .tmp-* directories left behind by interrupted NAR
        // extractions (fetch_and_extract extracts to a sibling tmp dir then
        // renames atomically; a crash mid-extraction leaves the tmp dir behind).
        Self::clean_stale_tmp_dirs(&cache_dir);

        let db_path = cache_dir.join("cache_index.sqlite");
        let url = format!("sqlite://{}?mode=rwc", db_path.display());
        let runtime = Handle::current();

        let pool = SqlitePool::connect(&url).await?;

        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&pool)
            .await?;
        sqlx::query("PRAGMA synchronous=NORMAL")
            .execute(&pool)
            .await?;
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS cached_paths (
                store_path TEXT PRIMARY KEY NOT NULL,
                size_bytes INTEGER NOT NULL,
                last_access INTEGER NOT NULL
            )"#,
        )
        .execute(&pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_last_access ON cached_paths(last_access)")
            .execute(&pool)
            .await?;

        Ok(Self {
            cache_dir,
            max_size_bytes: max_size_gb * 1024 * 1024 * 1024,
            pool,
            runtime,
            inflight: Mutex::new(HashMap::new()),
        })
    }

    /// Remove stale `*.tmp-*` directories from the cache root. These are
    /// remnants of interrupted NAR extractions.
    fn clean_stale_tmp_dirs(cache_dir: &Path) {
        let Ok(entries) = std::fs::read_dir(cache_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            // Match foo.tmp-<hex> (the pattern used by fetch_and_extract).
            if name_str.contains(".tmp-") {
                let path = entry.path();
                if let Err(e) = std::fs::remove_dir_all(&path) {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to remove stale tmp extraction dir"
                    );
                } else {
                    tracing::info!(path = %path.display(), "removed stale tmp extraction dir");
                }
            }
        }
    }

    /// Root directory where cached paths are materialized.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Check if a store path is cached.
    pub fn contains(&self, store_path: &str) -> bool {
        let pool = &self.pool;
        self.runtime.block_on(async {
            let count: i64 = match sqlx::query_scalar(
                "SELECT COUNT(*) FROM cached_paths WHERE store_path = ?1",
            )
            .bind(store_path)
            .fetch_one(pool)
            .await
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(store_path, error = %e, "FUSE cache contains() query failed");
                    0
                }
            };
            count > 0
        })
    }

    /// Get the full filesystem path for a cached store path.
    ///
    /// Returns `None` if the path is not cached.
    pub fn get_path(&self, store_path: &str) -> Option<PathBuf> {
        if self.contains(store_path) {
            self.touch(store_path);
            Some(self.cache_dir.join(store_path))
        } else {
            None
        }
    }

    /// Record a store path as cached after extraction.
    pub fn insert(&self, store_path: &str, size_bytes: u64) -> Result<(), CacheError> {
        let now = unix_now();
        let pool = &self.pool;
        self.runtime.block_on(async {
            sqlx::query(
                "INSERT OR REPLACE INTO cached_paths (store_path, size_bytes, last_access)
                 VALUES (?1, ?2, ?3)",
            )
            .bind(store_path)
            .bind(size_bytes as i64)
            .bind(now)
            .execute(pool)
            .await?;
            Ok::<_, sqlx::Error>(())
        })?;
        // Update cache size metric (ground-truth from DB)
        metrics::gauge!("rio_worker_fuse_cache_size_bytes").set(self.total_size() as f64);
        Ok(())
    }

    /// Update the last access timestamp for a cached path.
    fn touch(&self, store_path: &str) {
        let now = unix_now();
        let pool = &self.pool;
        self.runtime.block_on(async {
            if let Err(e) =
                sqlx::query("UPDATE cached_paths SET last_access = ?1 WHERE store_path = ?2")
                    .bind(now)
                    .bind(store_path)
                    .execute(pool)
                    .await
            {
                tracing::warn!(store_path, error = %e, "FUSE cache touch() failed");
            }
        });
    }

    /// Total size of all cached paths in bytes.
    pub fn total_size(&self) -> u64 {
        let pool = &self.pool;
        self.runtime.block_on(async {
            let size: i64 =
                match sqlx::query_scalar("SELECT COALESCE(SUM(size_bytes), 0) FROM cached_paths")
                    .fetch_one(pool)
                    .await
                {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "FUSE cache total_size() query failed");
                        0
                    }
                };
            size as u64
        })
    }

    /// Evict least-recently-used entries until total size is under the limit.
    pub fn evict_if_needed(&self) -> Result<u64, CacheError> {
        let mut freed = 0u64;

        loop {
            let total = self.total_size();
            if total <= self.max_size_bytes {
                break;
            }

            let pool = &self.pool;
            let entry = self.runtime.block_on(async {
                let row: Option<(String, i64)> = match sqlx::query_as(
                    "SELECT store_path, size_bytes FROM cached_paths
                     ORDER BY last_access ASC LIMIT 1",
                )
                .fetch_optional(pool)
                .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(error = %e, "FUSE cache eviction query failed");
                        None
                    }
                };
                row.map(|(path, size)| CacheEntry {
                    store_path: path,
                    size_bytes: size as u64,
                    last_access: 0,
                })
            });

            let Some(entry) = entry else {
                break;
            };

            // Remove from disk
            let path = self.cache_dir.join(&entry.store_path);
            if path.exists()
                && let Err(e) = std::fs::remove_dir_all(&path)
            {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to remove evicted cache entry from disk"
                );
            }

            // Remove from index
            let store_path = entry.store_path.clone();
            self.runtime.block_on(async {
                sqlx::query("DELETE FROM cached_paths WHERE store_path = ?1")
                    .bind(&store_path)
                    .execute(pool)
                    .await
            })?;

            freed += entry.size_bytes;
            tracing::debug!(
                store_path = %entry.store_path,
                size = entry.size_bytes,
                "evicted cache entry"
            );
        }

        // Update cache size metric after eviction
        if freed > 0 {
            metrics::gauge!("rio_worker_fuse_cache_size_bytes").set(self.total_size() as f64);
        }
        Ok(freed)
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
            Entry::Occupied(e) => FetchClaim::WaitFor(e.get().clone()),
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

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Cache sync methods use `block_on`, so tests create the cache in async
    /// context then exercise the sync methods via `spawn_blocking`.
    async fn make_cache(cache_dir: PathBuf, max_size_gb: u64) -> Arc<Cache> {
        Arc::new(Cache::new(cache_dir, max_size_gb).await.unwrap())
    }

    #[tokio::test]
    async fn test_cache_new_creates_dir_and_db() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");

        let cache = make_cache(cache_dir.clone(), 1).await;
        assert!(cache_dir.exists());
        assert!(cache_dir.join("cache_index.sqlite").exists());

        let size = tokio::task::spawn_blocking(move || cache.total_size())
            .await
            .unwrap();
        assert_eq!(size, 0);
    }

    #[tokio::test]
    async fn test_cache_insert_and_contains() {
        let dir = tempfile::tempdir().unwrap();
        let cache = make_cache(dir.path().join("cache"), 1).await;

        tokio::task::spawn_blocking(move || {
            assert!(!cache.contains("abc-hello-1.0"));

            cache.insert("abc-hello-1.0", 1024).unwrap();
            assert!(cache.contains("abc-hello-1.0"));
            assert_eq!(cache.total_size(), 1024);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_cache_get_path() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let cache = make_cache(cache_dir.clone(), 1).await;

        tokio::task::spawn_blocking(move || {
            assert!(cache.get_path("abc-hello-1.0").is_none());

            cache.insert("abc-hello-1.0", 512).unwrap();
            let path = cache.get_path("abc-hello-1.0").unwrap();
            assert_eq!(path, cache_dir.join("abc-hello-1.0"));
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_cache_eviction() {
        let dir = tempfile::tempdir().unwrap();
        // 0 GB max to force eviction
        let cache = make_cache(dir.path().join("cache"), 0).await;

        tokio::task::spawn_blocking(move || {
            cache.insert("old-path", 100).unwrap();
            cache.insert("new-path", 200).unwrap();

            let freed = cache.evict_if_needed().unwrap();
            assert!(freed > 0);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_inflight_claim_and_notify() {
        let dir = tempfile::tempdir().unwrap();
        let cache = make_cache(dir.path().join("cache"), 1).await;

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
    }

    #[tokio::test]
    async fn test_inflight_concurrent_wait() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::time::Instant;

        let dir = tempfile::tempdir().unwrap();
        let cache = make_cache(dir.path().join("cache"), 1).await;
        let fetch_count = Arc::new(AtomicU32::new(0));

        // Two threads race to fetch the same path. Exactly one should get
        // Fetch; the other should WaitFor and be woken promptly (not after
        // the old 1.4s backoff).
        let (c1, f1) = (cache.clone(), fetch_count.clone());
        let (c2, f2) = (cache.clone(), fetch_count.clone());

        let t1 = std::thread::spawn(move || do_claim(&c1, &f1));
        let t2 = std::thread::spawn(move || do_claim(&c2, &f2));

        let (d1, d2) = (t1.join().unwrap(), t2.join().unwrap());

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
    }

    #[tokio::test]
    async fn test_inflight_wait_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let cache = make_cache(dir.path().join("cache"), 1).await;

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
    }

    #[tokio::test]
    async fn test_tmp_cleanup_on_init() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().to_path_buf();

        // Create a stale tmp dir that mimics an interrupted extraction.
        let stale_tmp = cache_dir.join("abc-hello.tmp-deadbeef12345678");
        std::fs::create_dir_all(&stale_tmp).unwrap();
        std::fs::write(stale_tmp.join("partial_file"), b"incomplete").unwrap();
        assert!(stale_tmp.exists());

        // Also create a legitimate (non-tmp) entry to verify it's NOT removed.
        let real_entry = cache_dir.join("def-world");
        std::fs::create_dir_all(&real_entry).unwrap();

        // Cache::new should clean the stale tmp dir but leave the real entry.
        let _cache = Cache::new(cache_dir, 10).await.unwrap();
        assert!(
            !stale_tmp.exists(),
            "stale tmp dir should be removed on init"
        );
        assert!(
            real_entry.exists(),
            "real cache entries must not be removed"
        );
    }
}
