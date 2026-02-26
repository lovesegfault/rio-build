//! LRU cache for FUSE store paths backed by local SSD.
//!
//! Cached store paths are materialized as directory trees on disk (not stored
//! as NAR blobs). On cache miss the worker fetches the NAR via `GetPath`,
//! parses it, and extracts it to disk via [`rio_nix::nar::extract_to_path`].
//!
//! A lightweight SQLite index tracks cached paths, sizes, and access
//! timestamps for LRU eviction decisions.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
    /// In-flight fetches to avoid duplicate downloads.
    inflight: Mutex<HashSet<String>>,
}

impl Cache {
    /// Create a new cache rooted at `cache_dir` with the given size limit.
    ///
    /// Creates the cache directory and SQLite index if they don't exist.
    /// Must be called from within a tokio runtime (captures the current `Handle`).
    pub async fn new(cache_dir: PathBuf, max_size_gb: u64) -> Result<Self, CacheError> {
        std::fs::create_dir_all(&cache_dir)?;

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
            inflight: Mutex::new(HashSet::new()),
        })
    }

    /// Root directory where cached paths are materialized.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Check if a store path is cached.
    pub fn contains(&self, store_path: &str) -> bool {
        let pool = &self.pool;
        self.runtime.block_on(async {
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM cached_paths WHERE store_path = ?1")
                    .bind(store_path)
                    .fetch_one(pool)
                    .await
                    .unwrap_or(0);
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
            let _ = sqlx::query("UPDATE cached_paths SET last_access = ?1 WHERE store_path = ?2")
                .bind(now)
                .bind(store_path)
                .execute(pool)
                .await;
        });
    }

    /// Total size of all cached paths in bytes.
    pub fn total_size(&self) -> u64 {
        let pool = &self.pool;
        self.runtime.block_on(async {
            let size: i64 =
                sqlx::query_scalar("SELECT COALESCE(SUM(size_bytes), 0) FROM cached_paths")
                    .fetch_one(pool)
                    .await
                    .unwrap_or(0);
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
                let row: Option<(String, i64)> = sqlx::query_as(
                    "SELECT store_path, size_bytes FROM cached_paths
                     ORDER BY last_access ASC LIMIT 1",
                )
                .fetch_optional(pool)
                .await
                .ok()
                .flatten();
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

    /// Try to mark a path as in-flight (being fetched).
    ///
    /// Returns `true` if the path was not already in-flight (caller should fetch).
    /// Returns `false` if another task is already fetching it.
    pub fn try_start_fetch(&self, store_path: &str) -> bool {
        let mut inflight = self.inflight.lock().unwrap_or_else(|e| {
            tracing::error!("inflight lock poisoned, recovering");
            e.into_inner()
        });
        inflight.insert(store_path.to_string())
    }

    /// Mark a path as no longer in-flight.
    pub fn finish_fetch(&self, store_path: &str) {
        let mut inflight = self.inflight.lock().unwrap_or_else(|e| {
            tracing::error!("inflight lock poisoned, recovering");
            e.into_inner()
        });
        inflight.remove(store_path);
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
    async fn test_inflight_tracking() {
        let dir = tempfile::tempdir().unwrap();
        let cache = make_cache(dir.path().join("cache"), 1).await;

        // inflight tracking doesn't use block_on, so can test directly
        assert!(cache.try_start_fetch("abc-path"));
        assert!(!cache.try_start_fetch("abc-path"));

        cache.finish_fetch("abc-path");
        assert!(cache.try_start_fetch("abc-path"));
    }
}
