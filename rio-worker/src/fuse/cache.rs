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

use rusqlite::Connection;

/// Errors from cache operations.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
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
/// Thread-safe: the SQLite connection is wrapped in a `Mutex` because
/// `rusqlite::Connection` is `Send` but not `Sync`.
pub struct Cache {
    /// Root directory where cached paths are materialized.
    cache_dir: PathBuf,
    /// Maximum cache size in bytes.
    max_size_bytes: u64,
    /// SQLite metadata index (protected by mutex for `&self` access).
    db: Mutex<Connection>,
    /// In-flight fetches to avoid duplicate downloads.
    inflight: Mutex<HashSet<String>>,
}

impl Cache {
    /// Create a new cache rooted at `cache_dir` with the given size limit.
    ///
    /// Creates the cache directory and SQLite index if they don't exist.
    pub fn new(cache_dir: PathBuf, max_size_gb: u64) -> Result<Self, CacheError> {
        std::fs::create_dir_all(&cache_dir)?;

        let db_path = cache_dir.join("cache_index.sqlite");
        let conn = Connection::open(&db_path)?;

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;

             CREATE TABLE IF NOT EXISTS cached_paths (
                 store_path TEXT PRIMARY KEY NOT NULL,
                 size_bytes INTEGER NOT NULL,
                 last_access INTEGER NOT NULL
             );

             CREATE INDEX IF NOT EXISTS idx_last_access
                 ON cached_paths(last_access);",
        )?;

        Ok(Self {
            cache_dir,
            max_size_bytes: max_size_gb * 1024 * 1024 * 1024,
            db: Mutex::new(conn),
            inflight: Mutex::new(HashSet::new()),
        })
    }

    /// Root directory where cached paths are materialized.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Check if a store path is cached.
    pub fn contains(&self, store_path: &str) -> bool {
        let db = self.db.lock().unwrap_or_else(|e| {
            tracing::error!("cache db lock poisoned, recovering");
            e.into_inner()
        });
        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM cached_paths WHERE store_path = ?1",
                [store_path],
                |row| row.get(0),
            )
            .unwrap_or(0);
        count > 0
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
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let db = self.db.lock().unwrap_or_else(|e| {
            tracing::error!("cache db lock poisoned, recovering");
            e.into_inner()
        });
        db.execute(
            "INSERT OR REPLACE INTO cached_paths (store_path, size_bytes, last_access)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![store_path, size_bytes as i64, now],
        )?;
        Ok(())
    }

    /// Update the last access timestamp for a cached path.
    fn touch(&self, store_path: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let db = self.db.lock().unwrap_or_else(|e| {
            tracing::error!("cache db lock poisoned, recovering");
            e.into_inner()
        });
        let _ = db.execute(
            "UPDATE cached_paths SET last_access = ?1 WHERE store_path = ?2",
            rusqlite::params![now, store_path],
        );
    }

    /// Total size of all cached paths in bytes.
    pub fn total_size(&self) -> u64 {
        let db = self.db.lock().unwrap_or_else(|e| {
            tracing::error!("cache db lock poisoned, recovering");
            e.into_inner()
        });
        let size: i64 = db
            .query_row(
                "SELECT COALESCE(SUM(size_bytes), 0) FROM cached_paths",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        size as u64
    }

    /// Evict least-recently-used entries until total size is under the limit.
    pub fn evict_if_needed(&self) -> Result<u64, CacheError> {
        let mut freed = 0u64;

        loop {
            let total = self.total_size();
            if total <= self.max_size_bytes {
                break;
            }

            let entry = {
                let db = self.db.lock().unwrap_or_else(|e| {
                    tracing::error!("cache db lock poisoned, recovering");
                    e.into_inner()
                });
                db.query_row(
                    "SELECT store_path, size_bytes FROM cached_paths
                     ORDER BY last_access ASC LIMIT 1",
                    [],
                    |row| {
                        Ok(CacheEntry {
                            store_path: row.get(0)?,
                            size_bytes: row.get::<_, i64>(1)? as u64,
                            last_access: 0,
                        })
                    },
                )
                .ok()
            };

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
            {
                let db = self.db.lock().unwrap_or_else(|e| {
                    tracing::error!("cache db lock poisoned, recovering");
                    e.into_inner()
                });
                db.execute(
                    "DELETE FROM cached_paths WHERE store_path = ?1",
                    [&entry.store_path],
                )?;
            }

            freed += entry.size_bytes;
            tracing::debug!(
                store_path = %entry.store_path,
                size = entry.size_bytes,
                "evicted cache entry"
            );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_new_creates_dir_and_db() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");

        let cache = Cache::new(cache_dir.clone(), 1).unwrap();
        assert!(cache_dir.exists());
        assert!(cache_dir.join("cache_index.sqlite").exists());
        assert_eq!(cache.total_size(), 0);
    }

    #[test]
    fn test_cache_insert_and_contains() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().join("cache"), 1).unwrap();

        assert!(!cache.contains("abc-hello-1.0"));

        cache.insert("abc-hello-1.0", 1024).unwrap();
        assert!(cache.contains("abc-hello-1.0"));
        assert_eq!(cache.total_size(), 1024);
    }

    #[test]
    fn test_cache_get_path() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let cache = Cache::new(cache_dir.clone(), 1).unwrap();

        assert!(cache.get_path("abc-hello-1.0").is_none());

        cache.insert("abc-hello-1.0", 512).unwrap();
        let path = cache.get_path("abc-hello-1.0").unwrap();
        assert_eq!(path, cache_dir.join("abc-hello-1.0"));
    }

    #[test]
    fn test_cache_eviction() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        // 1 byte max to force eviction
        let cache = Cache::new(cache_dir, 0).unwrap();
        // max_size_bytes = 0 * 1GB = 0

        cache.insert("old-path", 100).unwrap();
        cache.insert("new-path", 200).unwrap();

        let freed = cache.evict_if_needed().unwrap();
        assert!(freed > 0);
    }

    #[test]
    fn test_inflight_tracking() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().join("cache"), 1).unwrap();

        assert!(cache.try_start_fetch("abc-path"));
        assert!(!cache.try_start_fetch("abc-path"));

        cache.finish_fetch("abc-path");
        assert!(cache.try_start_fetch("abc-path"));
    }
}
