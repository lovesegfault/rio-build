//! Store trait defining the interface for Nix store backends.

use rio_nix::hash::NixHash;
use rio_nix::store_path::StorePath;

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
    /// Used by integration tests and [`super::memory::import_from_nix_store`].
    #[allow(clippy::too_many_arguments)]
    #[allow(dead_code)] // used by integration tests (separate crate)
    pub fn new(
        path: StorePath,
        deriver: Option<StorePath>,
        nar_hash: NixHash,
        references: Vec<StorePath>,
        registration_time: u64,
        nar_size: u64,
        ultimate: bool,
        sigs: Vec<String>,
        ca: Option<String>,
    ) -> Self {
        Self {
            path,
            deriver,
            nar_hash,
            references,
            registration_time,
            nar_size,
            ultimate,
            sigs,
            ca,
        }
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

    /// Content address (empty for input-addressed paths).
    pub fn ca(&self) -> Option<&str> {
        self.ca.as_deref()
    }
}

/// Interface for Nix store backends.
///
/// Consistency invariant: for every `PathInfo` returned by [`Store::query_path_info`],
/// `nar_hash` is the SHA-256 digest of the NAR bytes returned by [`Store::nar_from_path`]
/// for the same path, and `nar_size` equals the byte length of that NAR.
///
/// Phase 1a implements only read operations + temp roots.
/// Phase 1b adds write operations (add_to_store_nar, etc).
#[async_trait::async_trait]
pub trait Store: Send + Sync {
    /// Check if a store path exists.
    async fn is_valid_path(&self, path: &StorePath) -> anyhow::Result<bool>;

    /// Query full metadata for a store path. Returns `None` if the path doesn't exist.
    async fn query_path_info(&self, path: &StorePath) -> anyhow::Result<Option<PathInfo>>;

    /// Batch validity check: returns only the paths that exist in the store.
    async fn query_valid_paths(&self, paths: &[StorePath]) -> anyhow::Result<Vec<StorePath>>;

    /// Retrieve the NAR content for a store path. Returns `None` if not found.
    ///
    // TODO: return a streaming type instead of `Vec<u8>` to avoid buffering
    // large NARs in memory. Phase 1b should introduce an `AsyncRead`-based API.
    async fn nar_from_path(&self, path: &StorePath) -> anyhow::Result<Option<Vec<u8>>>;
}
