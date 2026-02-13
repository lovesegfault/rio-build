//! In-memory store backend for development and testing.
//!
//! Stores path metadata and NAR content in `HashMap`s protected by `RwLock`.
//! Can be pre-populated from a local Nix store at startup.

use std::collections::HashMap;
use std::sync::RwLock;

use rio_nix::store_path::StorePath;
use tracing::debug;

use super::traits::{PathInfo, Store};

/// An in-memory implementation of the Store trait.
///
/// Thread-safe via `RwLock`. Suitable for development and testing,
/// not for production use (no persistence, bounded by memory).
pub struct MemoryStore {
    /// Path metadata indexed by store path string.
    paths: RwLock<HashMap<String, PathInfo>>,
    /// NAR content indexed by store path string.
    nars: RwLock<HashMap<String, Vec<u8>>>,
}

#[allow(dead_code)]
impl MemoryStore {
    /// Create an empty in-memory store.
    pub fn new() -> Self {
        MemoryStore {
            paths: RwLock::new(HashMap::new()),
            nars: RwLock::new(HashMap::new()),
        }
    }

    /// Insert a path with its metadata (and optionally NAR content).
    pub fn insert(&self, info: PathInfo, nar: Option<Vec<u8>>) {
        let key = info.path.to_string();
        debug!(path = %key, "inserting path into memory store");
        self.paths.write().unwrap().insert(key.clone(), info);
        if let Some(nar_data) = nar {
            self.nars.write().unwrap().insert(key, nar_data);
        }
    }

    /// Return the number of paths in the store.
    pub fn len(&self) -> usize {
        self.paths.read().unwrap().len()
    }

    /// Check if the store is empty.
    pub fn is_empty(&self) -> bool {
        self.paths.read().unwrap().is_empty()
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
        let key = path.to_string();
        Ok(self.paths.read().unwrap().contains_key(&key))
    }

    async fn query_path_info(&self, path: &StorePath) -> anyhow::Result<Option<PathInfo>> {
        let key = path.to_string();
        Ok(self.paths.read().unwrap().get(&key).cloned())
    }

    async fn query_valid_paths(&self, paths: &[StorePath]) -> anyhow::Result<Vec<StorePath>> {
        let store = self.paths.read().unwrap();
        let valid = paths
            .iter()
            .filter(|p| store.contains_key(&p.to_string()))
            .cloned()
            .collect();
        Ok(valid)
    }

    async fn nar_from_path(&self, path: &StorePath) -> anyhow::Result<Option<Vec<u8>>> {
        let key = path.to_string();
        Ok(self.nars.read().unwrap().get(&key).cloned())
    }
}

#[cfg(test)]
mod tests {
    use rio_nix::hash::{HashAlgo, NixHash};

    use super::*;

    fn make_test_path_info() -> PathInfo {
        let path =
            StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1").unwrap();
        PathInfo {
            path,
            deriver: None,
            nar_hash: NixHash::compute(HashAlgo::SHA256, b"fake nar content"),
            references: vec![],
            registration_time: 1700000000,
            nar_size: 12345,
            ultimate: true,
            sigs: vec![],
            ca: None,
        }
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
        let path = info.path.clone();

        store.insert(info, Some(b"fake nar content".to_vec()));

        assert!(store.is_valid_path(&path).await.unwrap());
        let queried = store.query_path_info(&path).await.unwrap().unwrap();
        assert_eq!(queried.nar_size, 12345);
        assert_eq!(
            store.nar_from_path(&path).await.unwrap().unwrap(),
            b"fake nar content"
        );
    }

    #[tokio::test]
    async fn test_query_valid_paths() {
        let store = MemoryStore::new();
        let info = make_test_path_info();
        let existing_path = info.path.clone();
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
