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

/// Maximum number of outputs in a single PutPathBatch request.
///
/// Nix multi-output derivations typically have 2-5 outputs (out, dev, lib,
/// doc, man). 16 gives generous headroom without allowing a client to open
/// an unbounded number of per-output accumulation buffers on the server.
pub const MAX_BATCH_OUTPUTS: usize = 16;

/// Maximum number of DAG nodes in a single SubmitBuild request.
///
/// Protects the scheduler from unbounded DAG merges. Matches
/// `rio_nix::protocol::wire::MAX_COLLECTION_COUNT` (1M) — the gateway wire
/// layer is the trust boundary; tighter caps here only reject DAGs the
/// wire admitted (I-137: 100k→1M after hello-deep-1024x at 153,821
/// nodes). Memory: ~1 GB scheduler-side at the cap (DerivationState +
/// cycle-check color map + iterative DFS stack).
pub const MAX_DAG_NODES: usize = 1_048_576;

/// Maximum number of DAG edges in a single SubmitBuild request.
///
/// Realistic derivation DAGs have average out-degree 1-5; nixpkgs full
/// is ~200k edges for ~60k nodes. 5M maintains the 5× node ratio while
/// bounding the O(edges) merge loop against a fully-connected
/// pathological submission (1M nodes = 10^12 edges).
pub const MAX_DAG_EDGES: usize = 5_242_880;

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
