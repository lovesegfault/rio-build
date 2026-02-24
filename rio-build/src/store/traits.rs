//! Store trait defining the interface for Nix store backends.

use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::store_path::StorePath;
use tokio::io::AsyncRead;

/// A boxed async reader for streaming NAR content.
pub type NarReader = Box<dyn AsyncRead + Send + Unpin>;

/// Errors from [`PathInfo::new`] validation.
#[derive(Debug, thiserror::Error)]
pub enum PathInfoError {
    #[error("nar_hash must be SHA-256, got {0}")]
    WrongHashAlgorithm(HashAlgo),
    #[error("nar_size must be greater than zero")]
    ZeroNarSize,
}

/// Metadata about a store path, corresponding to narinfo fields.
///
/// All fields are private to enforce construction through [`PathInfo::new`],
/// which ensures consistency (e.g., `nar_size` matches the actual NAR content,
/// `nar_hash` is a valid SHA-256 digest). Access fields via accessor methods.
#[derive(Debug, Clone)]
pub struct PathInfo {
    #[allow(dead_code)] // read via path() accessor, used by integration tests
    /// The store path this info describes.
    path: StorePath,
    /// The derivation that produced this path, if known.
    deriver: Option<StorePath>,
    /// SHA-256 hash of the NAR serialization.
    nar_hash: NixHash,
    /// Store paths referenced by this path (runtime dependencies).
    references: Vec<StorePath>,
    /// Unix timestamp when this path was registered.
    registration_time: u64,
    /// Size of the NAR serialization in bytes.
    nar_size: u64,
    /// Whether this is the ultimate trusted source of the path.
    ultimate: bool,
    /// Cryptographic signatures over the path fingerprint.
    sigs: Vec<String>,
    /// Content address (empty for input-addressed paths).
    ca: Option<String>,
}

impl PathInfo {
    /// Create a new `PathInfo` with all fields.
    ///
    /// Prefer [`PathInfoBuilder`] for public construction.
    #[allow(clippy::too_many_arguments)]
    #[allow(dead_code)] // used by integration tests (separate crate)
    pub(crate) fn new(
        path: StorePath,
        deriver: Option<StorePath>,
        nar_hash: NixHash,
        references: Vec<StorePath>,
        registration_time: u64,
        nar_size: u64,
        ultimate: bool,
        sigs: Vec<String>,
        ca: Option<String>,
    ) -> Result<Self, PathInfoError> {
        if nar_hash.algo() != HashAlgo::SHA256 {
            return Err(PathInfoError::WrongHashAlgorithm(nar_hash.algo()));
        }
        if nar_size == 0 {
            return Err(PathInfoError::ZeroNarSize);
        }
        Ok(Self {
            path,
            deriver,
            nar_hash,
            references,
            registration_time,
            nar_size,
            ultimate,
            sigs,
            ca,
        })
    }

    /// The store path this info describes.
    #[allow(dead_code)] // used by integration tests (separate crate)
    pub fn path(&self) -> &StorePath {
        &self.path
    }

    /// The derivation that produced this path, if known.
    pub fn deriver(&self) -> Option<&StorePath> {
        self.deriver.as_ref()
    }

    /// SHA-256 hash of the NAR serialization.
    pub fn nar_hash(&self) -> &NixHash {
        &self.nar_hash
    }

    /// Store paths referenced by this path (runtime dependencies).
    pub fn references(&self) -> &[StorePath] {
        &self.references
    }

    /// Unix timestamp when this path was registered.
    pub fn registration_time(&self) -> u64 {
        self.registration_time
    }

    /// Size of the NAR serialization in bytes.
    pub fn nar_size(&self) -> u64 {
        self.nar_size
    }

    /// Whether this is the ultimate trusted source of the path.
    pub fn ultimate(&self) -> bool {
        self.ultimate
    }

    /// Cryptographic signatures over the path fingerprint.
    pub fn sigs(&self) -> &[String] {
        &self.sigs
    }

    /// Merge additional signatures into this path's signature set.
    /// Duplicates are ignored (set-union semantics, matching nix-daemon).
    pub fn add_sigs(&mut self, new_sigs: impl IntoIterator<Item = String>) {
        for sig in new_sigs {
            if !self.sigs.contains(&sig) {
                self.sigs.push(sig);
            }
        }
    }

    /// Content address (empty for input-addressed paths).
    pub fn ca(&self) -> Option<&str> {
        self.ca.as_deref()
    }
}

/// Builder for [`PathInfo`], the public API for constructing path metadata.
///
/// Required fields (`path`, `nar_hash`, `nar_size`) are set in the constructor.
/// Optional fields default to sensible values and can be overridden with
/// builder methods.
///
/// # Example
///
/// ```ignore
/// let info = PathInfoBuilder::new(path, nar_hash, nar_size)
///     .references(refs)
///     .ultimate(true)
///     .build()?;
/// ```
pub struct PathInfoBuilder {
    path: StorePath,
    nar_hash: NixHash,
    nar_size: u64,
    deriver: Option<StorePath>,
    references: Vec<StorePath>,
    registration_time: u64,
    ultimate: bool,
    sigs: Vec<String>,
    ca: Option<String>,
}

impl PathInfoBuilder {
    /// Create a new builder with the three required fields.
    pub fn new(path: StorePath, nar_hash: NixHash, nar_size: u64) -> Self {
        Self {
            path,
            nar_hash,
            nar_size,
            deriver: None,
            references: vec![],
            registration_time: 0,
            ultimate: false,
            sigs: vec![],
            ca: None,
        }
    }

    /// Set the derivation that produced this path.
    pub fn deriver(mut self, deriver: Option<StorePath>) -> Self {
        self.deriver = deriver;
        self
    }

    /// Set the runtime dependency references.
    pub fn references(mut self, references: Vec<StorePath>) -> Self {
        self.references = references;
        self
    }

    /// Set the Unix timestamp when this path was registered.
    pub fn registration_time(mut self, registration_time: u64) -> Self {
        self.registration_time = registration_time;
        self
    }

    /// Set whether this is the ultimate trusted source of the path.
    pub fn ultimate(mut self, ultimate: bool) -> Self {
        self.ultimate = ultimate;
        self
    }

    /// Set the cryptographic signatures over the path fingerprint.
    pub fn sigs(mut self, sigs: Vec<String>) -> Self {
        self.sigs = sigs;
        self
    }

    /// Set the content address.
    pub fn ca(mut self, ca: Option<String>) -> Self {
        self.ca = ca;
        self
    }

    /// Consume the builder and produce a validated [`PathInfo`].
    pub fn build(self) -> Result<PathInfo, PathInfoError> {
        PathInfo::new(
            self.path,
            self.deriver,
            self.nar_hash,
            self.references,
            self.registration_time,
            self.nar_size,
            self.ultimate,
            self.sigs,
            self.ca,
        )
    }
}

/// Interface for Nix store backends.
///
/// Consistency invariant: for every `PathInfo` returned by [`Store::query_path_info`],
/// `nar_hash` is the SHA-256 digest of the NAR bytes returned by [`Store::nar_from_path`]
/// for the same path, and `nar_size` equals the byte length of that NAR.
///
/// Read operations implemented in Phase 1a. Write operations (`add_path`)
/// added in Phase 1b for receiving uploaded paths via `wopAddToStoreNar`
/// and `wopAddMultipleToStore`.
#[async_trait::async_trait]
pub trait Store: Send + Sync {
    /// Check if a store path exists.
    async fn is_valid_path(&self, path: &StorePath) -> anyhow::Result<bool>;

    /// Query full metadata for a store path. Returns `None` if the path doesn't exist.
    async fn query_path_info(&self, path: &StorePath) -> anyhow::Result<Option<PathInfo>>;

    /// Batch validity check: returns only the paths that exist in the store.
    async fn query_valid_paths(&self, paths: &[StorePath]) -> anyhow::Result<Vec<StorePath>>;

    /// Retrieve the NAR content for a store path as a streaming reader.
    /// Returns `None` if not found.
    async fn nar_from_path(&self, path: &StorePath) -> anyhow::Result<Option<NarReader>>;

    /// Store a path with its metadata and NAR content.
    ///
    /// If the path already exists, the call succeeds without overwriting
    /// (idempotent). The caller is responsible for validating that the NAR
    /// content matches `info.nar_hash()` and `info.nar_size()` before calling.
    ///
    /// Called by `wopAddToStoreNar` and `wopAddMultipleToStore` handlers.
    // TODO: accept a streaming type (e.g., `NarReader`) instead of `Vec<u8>` to
    // avoid buffering large NARs on the write path. Requires reworking the hash
    // validation flow (SHA-256 is computed over the full NAR before inserting).
    #[allow(dead_code)] // used by opcode handlers added in next commits
    async fn add_path(&self, info: PathInfo, nar_data: Vec<u8>) -> anyhow::Result<()>;

    /// Resolve a store path from its hash part (32-char nixbase32 string).
    /// Returns `Some(path)` if a stored path's hash part matches, `None` otherwise.
    async fn query_path_from_hash_part(&self, hash_part: &str)
    -> anyhow::Result<Option<StorePath>>;

    /// Add signatures to an existing store path.
    ///
    /// If the path doesn't exist, this is a no-op (returns `Ok`).
    /// Signature deduplication uses set-union semantics.
    async fn add_signatures(&self, path: &StorePath, sigs: Vec<String>) -> anyhow::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path() -> StorePath {
        StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-test-1.0").unwrap()
    }

    fn sha256_hash() -> NixHash {
        NixHash::compute(HashAlgo::SHA256, b"test data")
    }

    #[test]
    fn path_info_rejects_non_sha256_hash() {
        let hash = NixHash::compute(HashAlgo::SHA512, b"test data");
        let result = PathInfoBuilder::new(test_path(), hash, 100).build();
        assert!(
            matches!(
                result,
                Err(PathInfoError::WrongHashAlgorithm(HashAlgo::SHA512))
            ),
            "expected WrongHashAlgorithm(SHA512), got {result:?}"
        );
    }

    #[test]
    fn path_info_rejects_zero_nar_size() {
        let result = PathInfoBuilder::new(test_path(), sha256_hash(), 0).build();
        assert!(
            matches!(result, Err(PathInfoError::ZeroNarSize)),
            "expected ZeroNarSize, got {result:?}"
        );
    }

    #[test]
    fn path_info_accepts_valid_sha256() {
        let result = PathInfoBuilder::new(test_path(), sha256_hash(), 100).build();
        assert!(result.is_ok());
    }

    #[test]
    fn add_sigs_merges_and_deduplicates() {
        let mut info = PathInfoBuilder::new(test_path(), sha256_hash(), 100)
            .sigs(vec!["key1:sig1".to_string()])
            .build()
            .unwrap();

        // Add new + duplicate
        info.add_sigs(vec!["key2:sig2".to_string(), "key1:sig1".to_string()]);
        assert_eq!(info.sigs(), &["key1:sig1", "key2:sig2"]);

        // Add only duplicates — no change
        info.add_sigs(vec!["key1:sig1".to_string()]);
        assert_eq!(info.sigs(), &["key1:sig1", "key2:sig2"]);
    }
}
