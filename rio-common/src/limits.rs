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

/// Maximum store paths the scheduler's `spawn_substitute_fetches` BFS
/// will visit per derivation. Bounds the closure walk against a hostile
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

/// Semaphore permits charged for one `NarChunk` against `nar_bytes_budget`.
///
/// Floored at [`MIN_NAR_CHUNK_CHARGE`]. Exposed so PutPath/PutPathBatch
/// callers track their cumulative charge in the SAME unit the semaphore
/// debits — `r[store.put.nar-bytes-budget]`'s "single batch can never
/// self-deadlock on permits it holds" only holds when the per-handler
/// `MAX_NAR_SIZE` cap is enforced in charged-permit units, not raw wire
/// bytes (a 1-byte chunk charges 256, so a raw-byte cap undercounts 256×).
pub const fn nar_chunk_charge(len: usize) -> u64 {
    let len = len as u64;
    if len < MIN_NAR_CHUNK_CHARGE as u64 {
        MIN_NAR_CHUNK_CHARGE as u64
    } else {
        len
    }
}

/// Maximum length of a `hw_class` string accepted by `AppendHwPerfSample`.
///
/// Real values are controller-stamped via downward-API as
/// `"{manufacturer}-{generation}-{storage}-{band}"` (e.g. `"aws-7-ebs-mid"`;
/// longest realistic `"unknown-unknown-unknown-unknown"` = 31 chars). The
/// schema column is unbounded `TEXT` and the unique key is composite
/// `(hw_class, pod_id)`, so without a length cap a compromised builder
/// holding a legitimate token could loop with distinct multi-MB strings and
/// fill `hw_perf_samples` indefinitely. 64 gives 2× headroom over the
/// longest legitimate value.
pub const MAX_HW_CLASS_LEN: usize = 64;

/// `hw_class` charset + length predicate: `[a-z0-9-]{1,MAX_HW_CLASS_LEN}`.
///
/// Single source of truth for the constraint enforced at every
/// `hw_class` sink (`AppendHwPerfSample`, `AppendInterruptSample`,
/// `SlaConfig::validate`, controller node-informer). The predicate lives
/// next to the limit constant so a future charset change (e.g. allowing
/// `_`) is one edit, not N inline `bytes().all(...)` copies (bug_038).
pub fn is_hw_class_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_HW_CLASS_LEN
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

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

/// Maximum size of a single `DerivationNode.drv_content` payload accepted
/// at SubmitBuild ingress, and equally the gateway's cap on the serialized
/// inline derivation for the content-bound single-node hook fallback.
///
/// Two producers fill `drv_content`:
/// - the inline-`.drv` *optimization* (`filter_and_inline_drv`), which
///   inlines cache-resident derivations of ≤64 KiB each under a 16 MiB
///   per-submission total budget, and
/// - the content-bound *hook fallback* (`build_fallback_node`), which may
///   carry a single derivation up to this constant because the `.drv`
///   exists in no store for the worker to fetch.
///
/// The scheduler validates the same bound defensively at SubmitBuild
/// ingress (workers and direct submitters are untrusted). Because the
/// gateway's fallback cap *is* this constant, anything the gateway
/// accepts is never size-rejected downstream — keep both sides pointed
/// at this single definition so the producer and consumer bounds cannot
/// drift apart again.
pub const MAX_DRV_CONTENT_BYTES: usize = 1024 * 1024;

/// Maximum NAR size for a `.drv` (derivation text) transfer: enforced
/// at the store's PutPath admission — chunk accumulation and the
/// trailer's declared `nar_size` — so an oversized "derivation" blob
/// never gets buffered, hashed, or stored; and passed as the collect
/// cap at every derivation-text fetch site (worker glue-table fetches,
/// gateway BFS `.drv` resolution), where the leading `Info.nar_size`
/// pre-check turns it into an immediate, byte-free rejection.
///
/// Sizing (no-knobs: const with rationale, not config): derivations
/// are ATerm text. The largest legitimate `.drv`s observed at
/// nixpkgs-scale are ~10 MiB (huge env blocks, `exportReferencesGraph`
/// users), so 16 MiB gives ~60% headroom and equals the gateway's
/// long-standing write-side `DRV_NAR_BUFFER_LIMIT`, which now aliases
/// this const. The general [`MAX_NAR_SIZE`] (4 GiB) is 256x too
/// generous for this class: combined with 16-way worker and 32-way
/// gateway fan-out it let one tenant-controlled `.drv` name stream
/// tens of GiB into trusted-plane buffers (round-16 bug_095).
pub const MAX_DRV_NAR_BYTES: u64 = 16 * 1024 * 1024;

/// The NAR transfer cap for a path CLASS, as a sealed type: derivation
/// texts get [`MAX_DRV_NAR_BYTES`], everything else [`MAX_NAR_SIZE`].
///
/// The field is PRIVATE and the only constructors are the two class
/// constructors — there is no `From<u64>` and no arithmetic: a fetch
/// site cannot mint a private or divergent bound, by construction
/// (round-17 bug_030: the round-16 cap consolidation missed the
/// scheduler's dispatch fetch precisely because the bound was an
/// untyped `u64` any site could shadow with a local const; the typed
/// seal turns the next missed sibling into a compile error). The NAR
/// fetch primitives (`get_path_nar` / `get_path_nar_to_file` in
/// rio-proto) take this type — never a raw `u64` — and the
/// `drv-cap-conformance` CI check pins both that signature and the
/// constructor call-site registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NarSizeCap(u64);

impl NarSizeCap {
    /// The derivation-text class cap ([`MAX_DRV_NAR_BYTES`], 16 MiB).
    /// For every fetch whose target is a `.drv` (claims verification,
    /// CA resolve, glue-table fetches, BFS `.drv` resolution).
    pub const fn derivation() -> Self {
        Self(MAX_DRV_NAR_BYTES)
    }

    /// The general path-class cap ([`MAX_NAR_SIZE`], 4 GiB). For
    /// fetches of arbitrary store paths (FUSE input materialization,
    /// client downloads).
    pub const fn general() -> Self {
        Self(MAX_NAR_SIZE)
    }

    /// The class cap for a path, derived from whether it is a
    /// derivation. Single source for the split so the store's
    /// admission bound and the worker/gateway collection bounds cannot
    /// drift apart.
    pub const fn for_path_class(is_derivation: bool) -> Self {
        if is_derivation {
            Self::derivation()
        } else {
            Self::general()
        }
    }

    /// The cap in bytes — for comparisons and error text only. This
    /// is deliberately a one-way door: bytes come OUT of a class cap;
    /// a cap never comes from bytes.
    pub const fn bytes(self) -> u64 {
        self.0
    }
}

/// The NAR byte cap appropriate for a path class. Prefer
/// [`NarSizeCap::for_path_class`]; this remains for arithmetic-only
/// consumers (store admission accumulation) and returns the same
/// sealed value's byte count.
pub const fn nar_size_cap(is_derivation: bool) -> u64 {
    NarSizeCap::for_path_class(is_derivation).bytes()
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// `nar_chunk_charge` floors at MIN_NAR_CHUNK_CHARGE for tiny chunks
    /// and is identity for chunks at/above the floor. This is the unit
    /// the per-handler MAX_NAR_SIZE cap MUST be tracked in — see
    /// `r[store.put.nar-bytes-budget]`.
    // r[verify store.put.nar-bytes-budget+3]
    #[test]
    fn nar_chunk_charge_floors_tiny_chunks() {
        assert_eq!(nar_chunk_charge(0), MIN_NAR_CHUNK_CHARGE as u64);
        assert_eq!(nar_chunk_charge(1), MIN_NAR_CHUNK_CHARGE as u64);
        assert_eq!(
            nar_chunk_charge(MIN_NAR_CHUNK_CHARGE as usize - 1),
            MIN_NAR_CHUNK_CHARGE as u64
        );
        assert_eq!(
            nar_chunk_charge(MIN_NAR_CHUNK_CHARGE as usize),
            MIN_NAR_CHUNK_CHARGE as u64
        );
        assert_eq!(nar_chunk_charge(4096), 4096);
        // Invariant the self-deadlock fix relies on: a handler that
        // sends N tiny chunks is charged ≥ N × MIN_NAR_CHUNK_CHARGE,
        // so the per-handler MAX_NAR_SIZE cap (in charged units) fires
        // at ≤ MAX_NAR_SIZE / MIN_NAR_CHUNK_CHARGE chunks — never more
        // than MAX_NAR_SIZE permits held → with budget ≥ MAX_NAR_SIZE
        // (production: 8×), no self-deadlock possible.
        let n = 1000u64;
        let charged: u64 = (0..n).map(|_| nar_chunk_charge(1)).sum();
        assert_eq!(charged, n * MIN_NAR_CHUNK_CHARGE as u64);
    }

    #[test]
    fn is_hw_class_name_charset_and_len() {
        assert!(is_hw_class_name("aws-7-ebs-mid"));
        assert!(is_hw_class_name("a"));
        assert!(is_hw_class_name("0-0"));
        assert!(is_hw_class_name(&"a".repeat(MAX_HW_CLASS_LEN)));
        // Rejects: empty, over-length, dot, underscore, uppercase,
        // non-ASCII. The dot case is the bug_038 trigger
        // (`c7a.xlarge` boots cleanly under the old config-validate,
        // every sample silently rejected at the gRPC sink).
        assert!(!is_hw_class_name(""));
        assert!(!is_hw_class_name(&"a".repeat(MAX_HW_CLASS_LEN + 1)));
        assert!(!is_hw_class_name("c7a.xlarge"));
        assert!(!is_hw_class_name("aws_7"));
        assert!(!is_hw_class_name("AWS-7"));
        assert!(!is_hw_class_name("aws-7-ébs"));
    }
}
