//! In-memory store backend for development and testing.
//!
//! Stores path metadata and NAR content in `HashMap`s protected by a single `RwLock`.
//! Can be pre-populated from a local Nix store at startup.

use std::collections::HashMap;
use std::sync::RwLock;

use rio_nix::store_path::StorePath;
use tracing::{debug, warn};

use super::traits::{NarReader, PathInfo, PathInfoBuilder, Store};

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

    fn read_inner(&self) -> std::sync::RwLockReadGuard<'_, StoreInner> {
        self.inner.read().unwrap_or_else(|e| {
            warn!("MemoryStore: recovering from poisoned read lock");
            e.into_inner()
        })
    }

    fn write_inner(&self) -> std::sync::RwLockWriteGuard<'_, StoreInner> {
        self.inner.write().unwrap_or_else(|e| {
            warn!("MemoryStore: recovering from poisoned write lock");
            e.into_inner()
        })
    }

    /// Insert a path with its metadata (and optionally NAR content).
    #[allow(dead_code)] // used by integration tests (separate crate)
    pub fn insert(&self, info: PathInfo, nar: Option<Vec<u8>>) {
        let key = info.path().clone();
        debug!(path = %key, "inserting path into memory store");
        let mut inner = self.write_inner();
        if let Some(nar_data) = nar {
            inner.nars.insert(key.clone(), nar_data);
        }
        inner.paths.insert(key, info);
    }

    /// Return the number of paths in the store.
    pub fn len(&self) -> usize {
        self.read_inner().paths.len()
    }

    /// Check if the store is empty.
    #[allow(dead_code)] // used by unit tests
    pub fn is_empty(&self) -> bool {
        self.read_inner().paths.is_empty()
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
        Ok(self.read_inner().paths.contains_key(path))
    }

    async fn query_path_info(&self, path: &StorePath) -> anyhow::Result<Option<PathInfo>> {
        Ok(self.read_inner().paths.get(path).cloned())
    }

    async fn query_valid_paths(&self, paths: &[StorePath]) -> anyhow::Result<Vec<StorePath>> {
        let inner = self.read_inner();
        let valid = paths
            .iter()
            .filter(|p| inner.paths.contains_key(*p))
            .cloned()
            .collect();
        Ok(valid)
    }

    // TODO: clones the full NAR Vec on every read. A future S3-backed store
    // would stream chunks as `Bytes` without cloning; this is fine for the
    // in-memory dev/test backend.
    async fn nar_from_path(&self, path: &StorePath) -> anyhow::Result<Option<NarReader>> {
        Ok(self
            .read_inner()
            .nars
            .get(path)
            .cloned()
            .map(|data| Box::new(std::io::Cursor::new(data)) as NarReader))
    }

    async fn query_path_from_hash_part(
        &self,
        hash_part: &str,
    ) -> anyhow::Result<Option<StorePath>> {
        Ok(self
            .read_inner()
            .paths
            .keys()
            .find(|p| p.hash_part() == hash_part)
            .cloned())
    }

    async fn add_signatures(&self, path: &StorePath, sigs: Vec<String>) -> anyhow::Result<()> {
        let mut inner = self.write_inner();
        if let Some(info) = inner.paths.get_mut(path) {
            info.add_sigs(sigs);
        }
        Ok(())
    }

    async fn add_path<'n>(
        &self,
        info: PathInfo,
        mut nar_data: Box<dyn tokio::io::AsyncRead + Send + Unpin + 'n>,
    ) -> anyhow::Result<()> {
        use tokio::io::AsyncReadExt;

        let key = info.path().clone();
        debug!(path = %key, "adding path to memory store");

        // Always drain the full stream (reader may be wire-backed).
        // Pre-allocate based on declared size to avoid repeated reallocation.
        let capacity =
            (info.nar_size() as usize).min(rio_nix::protocol::wire::MAX_FRAMED_TOTAL as usize);
        let mut data = Vec::with_capacity(capacity);
        nar_data.read_to_end(&mut data).await?;

        // ALWAYS validate, even for idempotent case — corrupt uploads must be
        // rejected, not silently accepted.
        super::validate::validate_nar(&data, &info)?;

        // Idempotent: if already present, don't overwrite.
        // Check AFTER validation so corrupt data is never silently accepted.
        // TOCTOU note: another task could insert between validate and write_inner().
        // This is benign: both tasks validated, the second becomes a no-op.
        let mut inner = self.write_inner();
        if inner.paths.contains_key(&key) {
            debug!(path = %key, "path already exists, skipping");
            return Ok(());
        }
        inner.nars.insert(key.clone(), data);
        inner.paths.insert(key, info);
        Ok(())
    }
}

/// Import a store path from the local Nix store by shelling out to `nix` CLI.
///
/// Calls `nix path-info --json <path>` for metadata and `nix-store --dump <path>`
/// for NAR content. Requires `nix` in PATH.
#[allow(dead_code)] // used by integration tests (separate crate)
pub fn import_from_nix_store(store_path: &str) -> anyhow::Result<(PathInfo, Vec<u8>)> {
    use anyhow::Context;
    use rio_nix::hash::NixHash;
    use rio_nix::store_path::StorePath;
    use std::process::Command;

    // Get metadata via nix path-info --json
    let output = Command::new("nix")
        .args(["path-info", "--json", store_path])
        .output()
        .context("nix path-info command failed to execute")?;
    if !output.status.success() {
        anyhow::bail!(
            "nix path-info returned non-zero exit status {} for {store_path}",
            output.status
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("failed to parse nix path-info JSON output")?;
    let info = json
        .as_object()
        .and_then(|o| o.get(store_path))
        .and_then(|v| v.as_object())
        .ok_or_else(|| {
            anyhow::anyhow!("nix path-info JSON missing expected structure for {store_path}")
        })?;

    let path = StorePath::parse(store_path).context("failed to parse store path")?;
    let nar_hash_str = info
        .get("narHash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing or non-string narHash field for {store_path}"))?;
    let nar_hash = NixHash::parse(nar_hash_str).context("failed to parse narHash")?;
    let nar_size = info
        .get("narSize")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow::anyhow!("missing or invalid narSize for {store_path}"))?;

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

    let path_info = PathInfoBuilder::new(path, nar_hash, nar_size)
        .deriver(deriver)
        .references(references)
        .registration_time(registration_time)
        .ultimate(ultimate)
        .sigs(sigs)
        .ca(ca)
        .build()
        .context("failed to construct PathInfo")?;

    // Get NAR content via nix-store --dump
    let nar_output = Command::new("nix-store")
        .args(["--dump", store_path])
        .output()
        .context("nix-store --dump command failed to execute")?;
    if !nar_output.status.success() {
        anyhow::bail!(
            "nix-store --dump returned non-zero exit status {} for {store_path}",
            nar_output.status
        );
    }

    Ok((path_info, nar_output.stdout))
}

#[cfg(test)]
mod tests {
    use rio_nix::hash::{HashAlgo, NixHash};
    use tokio::io::AsyncReadExt;

    use super::*;

    fn make_test_path_info() -> PathInfo {
        let path =
            StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1").unwrap();
        PathInfoBuilder::new(
            path,
            NixHash::compute(HashAlgo::SHA256, b"fake nar content"),
            12345,
        )
        .registration_time(1700000000)
        .ultimate(true)
        .build()
        .unwrap()
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
        let mut reader = store.nar_from_path(&path).await.unwrap().unwrap();
        let mut data = Vec::new();
        reader.read_to_end(&mut data).await.unwrap();
        assert_eq!(data, b"fake nar content");
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

    /// Build a PathInfo whose hash and size match the given NAR data.
    fn make_matching_info(nar_data: &[u8]) -> PathInfo {
        let path =
            StorePath::parse("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-add-test-1.0").unwrap();
        PathInfoBuilder::new(
            path,
            NixHash::compute(HashAlgo::SHA256, nar_data),
            nar_data.len() as u64,
        )
        .build()
        .unwrap()
    }

    #[tokio::test]
    async fn test_add_path_via_trait() {
        let store = MemoryStore::new();
        let nar = b"nar data for add_path test";
        let info = make_matching_info(nar);
        let path = info.path().clone();

        store
            .add_path(info, Box::new(std::io::Cursor::new(nar.to_vec())))
            .await
            .unwrap();

        assert!(store.is_valid_path(&path).await.unwrap());
        let mut reader = store.nar_from_path(&path).await.unwrap().unwrap();
        let mut data = Vec::new();
        reader.read_to_end(&mut data).await.unwrap();
        assert_eq!(data, nar);
    }

    #[tokio::test]
    async fn test_add_path_idempotent() {
        let store = MemoryStore::new();
        let nar = b"idempotent nar data";
        let info = make_matching_info(nar);
        let path = info.path().clone();

        store
            .add_path(info.clone(), Box::new(std::io::Cursor::new(nar.to_vec())))
            .await
            .unwrap();
        // Second add with same valid data should succeed without overwriting
        store
            .add_path(info, Box::new(std::io::Cursor::new(nar.to_vec())))
            .await
            .unwrap();

        // Original data should be preserved
        let mut reader = store.nar_from_path(&path).await.unwrap().unwrap();
        let mut data = Vec::new();
        reader.read_to_end(&mut data).await.unwrap();
        assert_eq!(data, nar);
    }

    #[tokio::test]
    async fn test_add_path_rejects_hash_mismatch() {
        let store = MemoryStore::new();
        let nar = b"correct data";
        let info = make_matching_info(nar);
        let path = info.path().clone();

        let err = store
            .add_path(
                info,
                Box::new(std::io::Cursor::new(b"wrong data!!".to_vec())),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("hash mismatch"),
            "expected 'hash mismatch', got: {err}"
        );
        // Path should NOT have been inserted
        assert!(!store.is_valid_path(&path).await.unwrap());
    }

    #[tokio::test]
    async fn test_add_path_rejects_size_mismatch() {
        let store = MemoryStore::new();
        let nar = b"size test";
        let info = make_matching_info(nar);
        let path = info.path().clone();

        let err = store
            .add_path(info, Box::new(std::io::Cursor::new(b"short".to_vec())))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("size mismatch"),
            "expected 'size mismatch', got: {err}"
        );
        assert!(!store.is_valid_path(&path).await.unwrap());
    }

    #[tokio::test]
    async fn test_add_path_idempotent_validates_corrupt() {
        let store = MemoryStore::new();
        let nar = b"valid nar content";
        let info = make_matching_info(nar);
        let path = info.path().clone();

        // First add succeeds
        store
            .add_path(info.clone(), Box::new(std::io::Cursor::new(nar.to_vec())))
            .await
            .unwrap();

        // Second add with corrupt data should be rejected even though path exists
        let err = store
            .add_path(
                info,
                Box::new(std::io::Cursor::new(b"corrupt data!!!!".to_vec())),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("size mismatch") || err.to_string().contains("hash mismatch"),
            "expected validation error, got: {err}"
        );

        // Original valid data should still be in the store
        let mut reader = store.nar_from_path(&path).await.unwrap().unwrap();
        let mut data = Vec::new();
        reader.read_to_end(&mut data).await.unwrap();
        assert_eq!(data, nar);
    }

    #[tokio::test]
    async fn test_add_signatures_to_existing_path() {
        let store = MemoryStore::new();
        let info = make_test_path_info();
        let path = info.path().clone();
        store.insert(info, None);

        store
            .add_signatures(&path, vec!["key1:sig1".to_string()])
            .await
            .unwrap();

        let queried = store.query_path_info(&path).await.unwrap().unwrap();
        assert_eq!(queried.sigs(), &["key1:sig1"]);

        // Add more, including a duplicate
        store
            .add_signatures(
                &path,
                vec!["key2:sig2".to_string(), "key1:sig1".to_string()],
            )
            .await
            .unwrap();
        let queried = store.query_path_info(&path).await.unwrap().unwrap();
        assert_eq!(queried.sigs(), &["key1:sig1", "key2:sig2"]);
    }

    #[tokio::test]
    async fn test_add_signatures_to_missing_path() {
        let store = MemoryStore::new();
        let path =
            StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-missing-1.0").unwrap();

        // Should succeed as a no-op
        store
            .add_signatures(&path, vec!["key1:sig1".to_string()])
            .await
            .unwrap();
    }
}
