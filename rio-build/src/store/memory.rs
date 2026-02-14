//! In-memory store backend for development and testing.
//!
//! Stores path metadata and NAR content in `HashMap`s protected by a single `RwLock`.
//! Can be pre-populated from a local Nix store at startup.

use std::collections::HashMap;
use std::sync::RwLock;

use rio_nix::store_path::StorePath;
use tracing::{debug, warn};

use super::traits::{PathInfo, Store};

/// Inner state protected by a single `RwLock`, ensuring atomicity of
/// insert operations across both maps.
struct StoreInner {
    /// Path metadata indexed by store path.
    paths: HashMap<StorePath, PathInfo>,
    /// NAR content indexed by store path.
    nars: HashMap<StorePath, Vec<u8>>,
}

/// An in-memory implementation of the Store trait.
///
/// Thread-safe via `std::sync::RwLock`. We use the std lock rather than
/// `tokio::sync::RwLock` because the critical sections are short HashMap
/// lookups/inserts with no `.await` points inside. Holding a std `RwLock`
/// across an `.await` would be wrong, but we never do — each lock is
/// acquired and released within a single synchronous block. The std lock
/// avoids the overhead of the async lock's cooperative scheduling.
///
/// Suitable for development and testing, not for production use (no
/// persistence, bounded by memory).
pub struct MemoryStore {
    inner: RwLock<StoreInner>,
}

impl MemoryStore {
    /// Create an empty in-memory store.
    pub fn new() -> Self {
        MemoryStore {
            inner: RwLock::new(StoreInner {
                paths: HashMap::new(),
                nars: HashMap::new(),
            }),
        }
    }

    /// Insert a path with its metadata (and optionally NAR content).
    #[allow(dead_code)] // used by integration tests (separate crate)
    pub fn insert(&self, info: PathInfo, nar: Option<Vec<u8>>) {
        let key = info.path().clone();
        debug!(path = %key, "inserting path into memory store");
        let mut inner = self.inner.write().unwrap_or_else(|e| {
            warn!("MemoryStore: recovering from poisoned write lock");
            e.into_inner()
        });
        if let Some(nar_data) = nar {
            inner.nars.insert(key.clone(), nar_data);
        }
        inner.paths.insert(key, info);
    }

    /// Return the number of paths in the store.
    pub fn len(&self) -> usize {
        self.inner
            .read()
            .unwrap_or_else(|e| {
                warn!("MemoryStore: recovering from poisoned read lock");
                e.into_inner()
            })
            .paths
            .len()
    }

    /// Check if the store is empty.
    #[allow(dead_code)] // used by unit tests
    pub fn is_empty(&self) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|e| {
                warn!("MemoryStore: recovering from poisoned read lock");
                e.into_inner()
            })
            .paths
            .is_empty()
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Store for MemoryStore {
    async fn is_valid_path(&self, path: &StorePath) -> anyhow::Result<bool> {
        let inner = self.inner.read().unwrap_or_else(|e| {
            warn!("MemoryStore: recovering from poisoned read lock");
            e.into_inner()
        });
        Ok(inner.paths.contains_key(path))
    }

    async fn query_path_info(&self, path: &StorePath) -> anyhow::Result<Option<PathInfo>> {
        let inner = self.inner.read().unwrap_or_else(|e| {
            warn!("MemoryStore: recovering from poisoned read lock");
            e.into_inner()
        });
        Ok(inner.paths.get(path).cloned())
    }

    async fn query_valid_paths(&self, paths: &[StorePath]) -> anyhow::Result<Vec<StorePath>> {
        let inner = self.inner.read().unwrap_or_else(|e| {
            warn!("MemoryStore: recovering from poisoned read lock");
            e.into_inner()
        });
        let valid = paths
            .iter()
            .filter(|p| inner.paths.contains_key(*p))
            .cloned()
            .collect();
        Ok(valid)
    }

    async fn nar_from_path(&self, path: &StorePath) -> anyhow::Result<Option<Vec<u8>>> {
        let inner = self.inner.read().unwrap_or_else(|e| {
            warn!("MemoryStore: recovering from poisoned read lock");
            e.into_inner()
        });
        Ok(inner.nars.get(path).cloned())
    }
}

/// Import a store path from the local Nix store by shelling out to `nix` CLI.
///
/// Calls `nix path-info --json <path>` for metadata and `nix-store --dump <path>`
/// for NAR content. Requires `nix` in PATH. Returns `None` on any failure.
#[allow(dead_code)] // used by integration tests (separate crate)
pub fn import_from_nix_store(store_path: &str) -> Option<(PathInfo, Vec<u8>)> {
    use rio_nix::hash::NixHash;
    use rio_nix::store_path::StorePath;
    use std::process::Command;

    // Get metadata via nix path-info --json
    let output = Command::new("nix")
        .args(["path-info", "--json", store_path])
        .output()
        .ok()
        .or_else(|| {
            debug!(store_path, "nix path-info command failed to execute");
            None
        })?;
    if !output.status.success() {
        debug!(store_path, status = %output.status, "nix path-info returned non-zero exit");
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok().or_else(|| {
        debug!(store_path, "failed to parse nix path-info JSON output");
        None
    })?;
    let info = json
        .as_object()
        .and_then(|o| o.get(store_path))
        .and_then(|v| v.as_object())
        .or_else(|| {
            debug!(store_path, "nix path-info JSON missing expected structure");
            None
        })?;

    let path = StorePath::parse(store_path).ok().or_else(|| {
        debug!(store_path, "failed to parse store path");
        None
    })?;
    let nar_hash_str = info.get("narHash")?.as_str()?;
    let nar_hash = NixHash::parse(nar_hash_str).ok().or_else(|| {
        debug!(store_path, nar_hash_str, "failed to parse narHash");
        None
    })?;
    let nar_size = info.get("narSize").and_then(|v| v.as_u64()).or_else(|| {
        debug!(store_path, "missing or invalid narSize");
        None
    })?;

    let references: Vec<StorePath> = info
        .get("references")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter_map(|s| StorePath::parse(s).ok())
                .collect()
        })
        .unwrap_or_default();

    let deriver = info
        .get("deriver")
        .and_then(|d| d.as_str())
        .and_then(|s| StorePath::parse(s).ok());

    let sigs: Vec<String> = info
        .get("signatures")
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let ca = info.get("ca").and_then(|c| c.as_str()).map(String::from);

    let registration_time = info
        .get("registrationTime")
        .and_then(|t| t.as_u64())
        .unwrap_or(0);

    let ultimate = info
        .get("ultimate")
        .and_then(|u| u.as_bool())
        .unwrap_or(false);

    let path_info = PathInfo::new(
        path,
        deriver,
        nar_hash,
        references,
        registration_time,
        nar_size,
        ultimate,
        sigs,
        ca,
    );

    // Get NAR content via nix-store --dump
    let nar_output = Command::new("nix-store")
        .args(["--dump", store_path])
        .output()
        .ok()
        .or_else(|| {
            debug!(store_path, "nix-store --dump command failed to execute");
            None
        })?;
    if !nar_output.status.success() {
        debug!(store_path, status = %nar_output.status, "nix-store --dump returned non-zero exit");
        return None;
    }

    Some((path_info, nar_output.stdout))
}

#[cfg(test)]
mod tests {
    use rio_nix::hash::{HashAlgo, NixHash};

    use super::*;

    fn make_test_path_info() -> PathInfo {
        let path =
            StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1").unwrap();
        PathInfo::new(
            path,
            None,
            NixHash::compute(HashAlgo::SHA256, b"fake nar content"),
            vec![],
            1700000000,
            12345,
            true,
            vec![],
            None,
        )
    }

    #[tokio::test]
    async fn test_empty_store() {
        let store = MemoryStore::new();
        let path =
            StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1").unwrap();
        assert!(!store.is_valid_path(&path).await.unwrap());
        assert!(store.query_path_info(&path).await.unwrap().is_none());
        assert!(store.nar_from_path(&path).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_insert_and_query() {
        let store = MemoryStore::new();
        let info = make_test_path_info();
        let path = info.path().clone();

        store.insert(info, Some(b"fake nar content".to_vec()));

        assert!(store.is_valid_path(&path).await.unwrap());
        let queried = store.query_path_info(&path).await.unwrap().unwrap();
        assert_eq!(queried.nar_size(), 12345);
        assert_eq!(
            store.nar_from_path(&path).await.unwrap().unwrap(),
            b"fake nar content"
        );
    }

    #[tokio::test]
    async fn test_query_valid_paths() {
        let store = MemoryStore::new();
        let info = make_test_path_info();
        let existing_path = info.path().clone();
        store.insert(info, None);

        let missing_path =
            StorePath::parse("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-missing-1.0").unwrap();

        let valid = store
            .query_valid_paths(&[existing_path.clone(), missing_path])
            .await
            .unwrap();
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0], existing_path);
    }

    #[tokio::test]
    async fn test_len_and_is_empty() {
        let store = MemoryStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);

        store.insert(make_test_path_info(), None);
        assert!(!store.is_empty());
        assert_eq!(store.len(), 1);
    }
}
