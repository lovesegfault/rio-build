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

/// Maximum size of a `.narinfo` body fetched from an upstream binary cache.
///
/// Typical narinfos are under 2 KiB, but `References:` is unbounded by the
/// format: a buildEnv-style path at the [`MAX_REFERENCES`] cap (10 000 refs ×
/// ~60 B basename) is ~600 KiB. 1 MiB gives comfortable headroom for that
/// legitimate ceiling while still bounding the `.text()` allocation against a
/// hostile tenant-configured upstream that streams gigabytes.
pub const MAX_NARINFO_BYTES: u64 = 1024 * 1024;

/// Maximum size of a `/nix-cache-info` body fetched from an upstream cache.
///
/// The real document is three short `Key: Value` lines (~60 bytes).
pub const MAX_CACHE_INFO_BYTES: u64 = 4 * 1024;

/// Maximum number of references in a single PathInfo.
///
/// Bounds unbounded repeated fields from untrusted proto input. A malicious
/// client could otherwise send millions of references in a single message
/// (within the 32 MiB gRPC frame limit, that's ~150k+ short store paths)
/// which would all be persisted to the database without validation.
pub const MAX_REFERENCES: usize = 10_000;

/// Maximum number of signatures in a single PathInfo.
pub const MAX_SIGNATURES: usize = 100;

/// Maximum store paths a single `try_substitute` will recursively fetch
/// via `ensure_references`. Bounds the closure walk against a hostile
/// upstream serving an infinite reference chain. Real closures (full
/// nixpkgs stdenv ~5k, full system ~20k) are well under this.
pub const MAX_SUBSTITUTE_CLOSURE: usize = 50_000;

/// Minimum NAR-budget charge per `NarChunk` message, in bytes.
///
/// `accumulate_chunk` charges `chunk.len().max(MIN_NAR_CHUNK_CHARGE)`
/// against the global `nar_bytes_budget` semaphore. Without a floor, a
/// 1-byte chunk acquires 1 permit but pushes a `SemaphorePermit`
/// (~16 B) plus a `Vec` slot into `held_permits` — a 1-byte stream
/// amplifies tracking overhead unbounded by the byte budget. 256 covers
/// the permit struct + Vec growth amortization with headroom; legit
/// clients chunk at ≥4 KiB so the floor never applies on the hot path.
pub const MIN_NAR_CHUNK_CHARGE: u32 = 256;

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
