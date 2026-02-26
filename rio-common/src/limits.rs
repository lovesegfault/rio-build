//! Shared size and resource limits across rio-build components.

/// Maximum NAR (Nix Archive) size accepted from any network peer: 4 GiB.
///
/// This bound prevents unbounded memory allocation from:
/// - A misbehaving store streaming an oversized NAR to gateway/worker clients
/// - A malicious client declaring `nar_size=u64::MAX` to trigger huge
///   `Vec::with_capacity` allocations on the store
///
/// 4 GiB is generous for real Nix store paths (most are under 1 GiB) while
/// providing a hard ceiling well within addressable memory on typical nodes.
pub const MAX_NAR_SIZE: u64 = 4 * 1024 * 1024 * 1024;
