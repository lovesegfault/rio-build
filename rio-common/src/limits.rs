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

/// Maximum number of references in a single PathInfo.
///
/// Bounds unbounded repeated fields from untrusted proto input. A malicious
/// client could otherwise send millions of references in a single message
/// (within the 32 MiB gRPC frame limit, that's ~150k+ short store paths)
/// which would all be persisted to the database without validation.
pub const MAX_REFERENCES: usize = 10_000;

/// Maximum number of signatures in a single PathInfo.
pub const MAX_SIGNATURES: usize = 100;

/// Maximum number of DAG nodes in a single SubmitBuild request.
///
/// Protects the scheduler from unbounded DAG merges. Large monorepo builds
/// can legitimately have tens of thousands of derivations; 100k gives
/// headroom without allowing runaway memory.
pub const MAX_DAG_NODES: usize = 100_000;

/// Maximum number of DAG edges in a single SubmitBuild request.
///
/// Realistic derivation DAGs have average out-degree 1-5; nixpkgs full
/// is ~200k edges for ~60k nodes. 500k gives headroom for dense DAGs
/// while bounding the O(edges) merge loop against a fully-connected
/// pathological submission (100k nodes = 10^10 edges).
pub const MAX_DAG_EDGES: usize = 500_000;

/// Worker heartbeat interval. The worker sends a HeartbeatRequest to the
/// scheduler at this cadence; the scheduler's staleness check uses the
/// derived timeout below. Changing this one constant moves both sides
/// in lockstep.
pub const HEARTBEAT_INTERVAL_SECS: u64 = 10;

/// How many missed heartbeats before a worker is considered dead.
pub const MAX_MISSED_HEARTBEATS: u32 = 3;

/// Heartbeat timeout. Derived as `interval × max_missed` so the coupling
/// is explicit: a worker is declared dead after missing 3 heartbeats, not
/// after an arbitrary 30s. If you tune the interval, the timeout moves
/// with it automatically.
pub const HEARTBEAT_TIMEOUT_SECS: u64 = MAX_MISSED_HEARTBEATS as u64 * HEARTBEAT_INTERVAL_SECS;
