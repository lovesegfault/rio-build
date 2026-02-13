//! Store trait defining the interface for Nix store backends.

use rio_nix::hash::NixHash;
use rio_nix::store_path::StorePath;

/// Metadata about a store path, corresponding to narinfo fields.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PathInfo {
    /// The store path this info describes.
    pub path: StorePath,
    /// The derivation that produced this path, if known.
    pub deriver: Option<StorePath>,
    /// SHA-256 hash of the NAR serialization.
    pub nar_hash: NixHash,
    /// Store paths referenced by this path (runtime dependencies).
    pub references: Vec<StorePath>,
    /// Unix timestamp when this path was registered.
    pub registration_time: u64,
    /// Size of the NAR serialization in bytes.
    pub nar_size: u64,
    /// Whether this is the ultimate trusted source of the path.
    pub ultimate: bool,
    /// Cryptographic signatures over the path fingerprint.
    pub sigs: Vec<String>,
    /// Content address (empty for input-addressed paths).
    pub ca: Option<String>,
}

/// Interface for Nix store backends.
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
    async fn nar_from_path(&self, path: &StorePath) -> anyhow::Result<Option<Vec<u8>>>;
}
