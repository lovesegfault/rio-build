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

/// AD5 (P8): `terminationGracePeriodSeconds` for every executor pod —
/// THE single source for the pull-mode grace. The controller stamps it
/// on the pod spec (`PULL_MODE_TGPS_SECS` is a cast of this constant)
/// and the builder partitions the same number into its abort-drain +
/// reserved-report slices ([`crate::transport::GraceBudget`]); the
/// 45 s/45 s agreement between the two crates is a compile-time
/// identity, not prose. SIGTERM in pull mode is an abort, not a drain:
/// the grace covers cgroup-kill + drain-the-completion + one bounded
/// report attempt + slack, never the stream era's 2 h drain default.
pub const PULL_MODE_TERMINATION_GRACE_SECS: u64 = 45;

/// Floor for the scheduler's `establishment_report_slack_secs` config —
/// the single source for a cross-component timing contract: the
/// controller's wedge clustering observes deadline-expired attempts in
/// the open view for `WEDGE_DEADLINE_GRACE_SECS + 2 ticks` before the
/// establishment sweep may remove them. The controller const-asserts
/// its side (`grace + 2*TICK <= floor`); the scheduler validates its
/// side (`slack >= floor`) at config load. Raising the controller
/// numbers past the floor becomes a compile error; lowering the
/// scheduler slack past it becomes a load error — the comment-only
/// invariant is gone.
pub const MIN_ESTABLISHMENT_REPORT_SLACK_SECS: u64 = 60;

/// Default global NAR buffer budget for the store: `8 × MAX_NAR_SIZE`
/// (32 GiB) — lets 8 max-size ingests buffer in parallel before the
/// 9th parks. THE single source for the store binary's
/// `nar_buffer_budget_bytes` None-default AND the xtask deploy's
/// memory-limit derivation (D4: the deployed limit is DERIVED as
/// `budget + STORE_NON_NAR_RESERVE_BYTES`, never hand-picked) — the
/// two sides consume this one constant so the budget/limit law cannot
/// drift apart.
pub const DEFAULT_STORE_NAR_BUDGET_BYTES: u64 = 8 * MAX_NAR_SIZE;

/// Typed non-NAR memory reserve for the store pod (D4): the memory a
/// store replica needs ON TOP of its NAR buffer budget. The budget/
/// limit law — enforced at `rio-store` `Config::validate` (boot,
/// against the downward-API-injected limit) and satisfied by
/// construction at the xtask deploy set-site (limit := budget +
/// reserve) — is:
///
/// ```text
/// nar_buffer_budget_bytes + STORE_NON_NAR_RESERVE_BYTES <= limits.memory
/// ```
///
/// Derivation (each term cites its pinning source; the rio-store
/// test `non_nar_reserve_derivation` asserts the sum against the
/// cited defaults):
///
/// - 2 GiB — chunk read cache (`chunk_cache_capacity_bytes` default;
///   moka high-watermark, `ChunkCache::DEFAULT_CACHE_CAPACITY_BYTES`);
/// - 1 GiB — build-log ingest budget (`log_bytes_budget` default;
///   deliberately disjoint from the NAR budget so the two ingest
///   planes cannot starve each other);
/// - 1 GiB — runtime slack: GetPath prefetch windows (`K × CHUNK_MAX`
///   ≤ 16 MiB per stream), transient chunk-upload buffers
///   (`chunk_upload_max_concurrent` × chunk size), sqlx pools,
///   binary + allocator baseline.
///
/// VIOLABLE (R17): this is an engineering envelope, not a theorem —
/// the slack term is a priced estimate. Operators raising the cache
/// or log budgets must raise the deployed limit by the same delta
/// (validate() refuses the incoherent combination at boot).
pub const STORE_NON_NAR_RESERVE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    /// `nar_chunk_charge` floors at MIN_NAR_CHUNK_CHARGE for tiny chunks
    /// and is identity for chunks at/above the floor. This is the unit
    /// the per-handler MAX_NAR_SIZE cap MUST be tracked in — see
    /// `r[store.put.nar-bytes-budget]`.
    // r[verify store.put.nar-bytes-budget+6]
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

    /// The D4 budget/limit law's const face: the reserve is the sum of
    /// its three documented terms, and the default budget+reserve pair
    /// is exactly what the xtask deploy derivation renders (36 GiB).
    /// The per-term sources are rio-store defaults — the rio-store
    /// test `non_nar_reserve_derivation` binds those; this pins the
    /// arithmetic where the consts live.
    #[test]
    fn store_reserve_terms_sum_and_budget_relation() {
        const GIB: u64 = 1024 * 1024 * 1024;
        assert_eq!(STORE_NON_NAR_RESERVE_BYTES, 2 * GIB + GIB + GIB);
        assert_eq!(DEFAULT_STORE_NAR_BUDGET_BYTES, 8 * MAX_NAR_SIZE);
        // The derived deploy limit is a whole number of Gi (k8s
        // quantity-friendly) — 32 GiB budget + 4 GiB reserve = 36 Gi.
        assert_eq!(
            (DEFAULT_STORE_NAR_BUDGET_BYTES + STORE_NON_NAR_RESERVE_BYTES) % GIB,
            0
        );
        assert_eq!(
            (DEFAULT_STORE_NAR_BUDGET_BYTES + STORE_NON_NAR_RESERVE_BYTES) / GIB,
            36
        );
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
