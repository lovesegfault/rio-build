use camino::Utf8PathBuf;
use uuid::Uuid;

/// Unique identifier for an agent (UUID)
pub type AgentId = Uuid;

/// Derivation store path (serves as the job identifier)
///
/// This is the full Nix store path to a derivation, e.g.,
/// `/nix/store/abc123xyz-foo.drv`.
///
/// This is the primary identifier for builds in the cluster.
/// Multiple users submitting the same derivation will have the same path,
/// enabling automatic build deduplication.
///
/// We use the full path (not just the hash) because:
/// - It's already unique (guaranteed by Nix)
/// - It's more debuggable (includes the package name)
/// - We don't need to parse or extract anything
pub type DerivationPath = Utf8PathBuf;
