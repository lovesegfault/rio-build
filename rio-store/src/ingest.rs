//! Write-ahead NAR ingest core, shared by PutPath/PutPathBatch (gRPC)
//! and `Substituter` (upstream binary-cache fetch).
//!
//! Both flows walk the same state machine:
//!
//! 1. [`claim_placeholder`] — idempotency check, insert
//!    `status='uploading'` row, hot-path stale-reclaim
//! 2. caller acquires NAR bytes (gRPC stream / HTTP download)
//! 3. [`persist_nar`] — branch on size: inline (`manifests.inline_blob`)
//!    or chunked (`cas::put_chunked`)
//! 4. on any error after step 1: [`abort_placeholder`]
//!
//! Before this module existed, `Substituter::ingest` open-coded steps
//! 1/3/4 and had already drifted from `grpc/put_path/common.rs` once
//! (substitution lacked the `chunk_dedup_ratio` gauge). Factoring here
//! keeps the write-ahead invariants in one place; gRPC/substitute keep
//! their transport-specific bits (HMAC, sig_mode, error mapping) in
//! thin wrappers.

use std::sync::Arc;

use bytes::Bytes;
use sqlx::PgPool;
use tracing::{debug, warn};
use uuid::Uuid;

use rio_proto::validated::ValidatedPathInfo;

use crate::backend::ChunkBackend;
use crate::cas;
use crate::gc::orphan::ReapBy;
use crate::metadata::{self, MetadataError};
use crate::substitute::SUBSTITUTE_STALE_THRESHOLD;

/// Result of [`claim_placeholder`].
pub enum PlaceholderClaim {
    /// Path is already `status='complete'`. Caller returns
    /// `created=false` (gRPC) or fetches the existing row (substitute)
    /// without writing anything.
    AlreadyComplete,
    /// We inserted (or stale-reclaimed-then-inserted) the
    /// `status='uploading'` placeholder. Caller now OWNS it and MUST
    /// [`abort_placeholder`] on any error path. The carried [`Uuid`] is
    /// the `manifests.claim_id` ownership token (M_052) — every
    /// owner-side cleanup passes it to `reap_one(ReapBy::Claim(id))`
    /// so a late-firing cleanup cannot reap a fresh re-upload at the
    /// same `store_path_hash`.
    Owned(Uuid),
    /// Another uploader holds a live (heartbeating) placeholder. Caller
    /// returns `aborted` so the client retries.
    Concurrent,
}

/// Per-caller observability hooks. The two ingest entry points emit
/// different metric names for the same events (stale-reclaim on the
/// PutPath hot path vs the substitution hot path are tracked
/// separately because they indicate different upstream-health
/// problems).
/// The death-DELETE + re-insert arm of stale reclaim (dead owner).
pub const STALE_RECLAIM_HEARTBEAT: &str = "heartbeat";
/// The owner aborted its own wedged download and released in place.
pub const STALE_RECLAIM_STALL_ABORT: &str = "stall_abort";
/// A competing claimant took over a frozen mid-download claim in place.
pub const STALE_RECLAIM_STALL_RECLAIM: &str = "stall_reclaim";
/// The COMPLETE `reason` label alphabet for
/// [`IngestHooks::stale_reclaimed_metric`] (bug_265: the field doc
/// hand-enumerated two of the three shipped reasons — the alphabet now
/// lives here, every emit site references a member, and the parity
/// test pins this array against the canonical HELP text).
// Outside tests the array itself is doc-and-registry only (the emit
// sites use the named members above); the parity test is its consumer.
#[cfg_attr(not(test), allow(dead_code))]
pub const STALE_RECLAIM_REASONS: [&str; 3] = [
    STALE_RECLAIM_HEARTBEAT,
    STALE_RECLAIM_STALL_ABORT,
    STALE_RECLAIM_STALL_RECLAIM,
];

#[derive(Clone, Copy)]
pub struct IngestHooks {
    /// `metrics::counter!` name incremented when a stale `'uploading'`
    /// placeholder is reaped on the hot path, labeled by `reason` —
    /// the alphabet is [`STALE_RECLAIM_REASONS`], nothing else. e.g.
    /// `rio_store_putpath_stale_reclaimed_total`.
    pub stale_reclaimed_metric: &'static str,
    /// Prefix for `warn!`/`debug!` log lines (e.g. `"PutPath"`,
    /// `"substitute"`).
    pub ctx_label: &'static str,
}

/// Substitution-claim parameters for [`claim_placeholder`]'s
/// download-stalled takeover arm and owner attribution
/// (`r[store.substitute.stale-reclaim+3]`). PutPath/PutPathBatch pass
/// `None`: builder claims carry no narinfo-declared size and no
/// progress evidence, so the stall arm is structurally unreachable
/// for and from them.
pub struct SubstituteClaimParams<'a> {
    /// The verified narinfo's declared `NarSize` — the persist-phase
    /// exemption bound (`fetched_bytes < nar_size` scopes the stall
    /// predicate to mid-download claims).
    pub nar_size: u64,
    /// The download-stall window (`RIO_SUBSTITUTE_STALL_SECS`).
    pub stall_window: std::time::Duration,
    /// Owner attribution stamped on the row (the pod name).
    pub claimed_by: &'a str,
}

// r[impl store.put.idempotent]
// r[impl store.put.stale-reclaim]
/// Idempotency check + `status='uploading'` placeholder insert +
/// hot-path stale/stall reclaim. The shared step-1 of the write-ahead
/// flow.
///
/// Flow:
/// 1. `check_manifest_complete` → [`PlaceholderClaim::AlreadyComplete`]
/// 2. `insert_manifest_uploading` → if inserted: [`PlaceholderClaim::Owned`]
///    (fresh insert, `stall_count` 0)
/// 3. ON CONFLICT no-op: a **released-in-place** row (`claim_id` NULL —
///    what the owner-side stall abort leaves behind,
///    `r[store.substitute.stall-abort]`) is claimed immediately, no
///    threshold, `stall_count` preserved.
/// 4. Else try `reap_one` with the stale threshold — heartbeat death
///    DELETEs + re-inserts, resetting stall evidence (I-207 — a
///    fetcher that died mid-upload leaves a placeholder the orphan
///    scanner won't reap for 15min, but the scheduler retries within
///    seconds). Precedes the stall arm so a dead owner is reaped, not
///    striked, when both predicates hold.
/// 5. Else, substitution callers only (`stall` params present): a
///    **download-stalled** live claim (`fetched_bytes < nar_size` ∧
///    progress clock older than the stall window) is taken over in
///    place, `stall_count += 1`.
/// 6. Still not claimed → [`PlaceholderClaim::Concurrent`] (live
///    uploader's heartbeat keeps `updated_at` fresh and its progress
///    clock advancing, so every arm above protected it).
///
/// Per-caller metrics (`exists` / `concurrent_upload` on
/// `rio_store_put_path_total`) are NOT emitted here — that's a
/// PutPath-specific counter. Only the reason-labeled stale-reclaim
/// counter (whose name the caller supplies) is emitted.
pub async fn claim_placeholder(
    pool: &PgPool,
    store_path_hash: &[u8],
    store_path: &str,
    refs: &[String],
    hooks: IngestHooks,
    stall: Option<&SubstituteClaimParams<'_>>,
) -> Result<PlaceholderClaim, MetadataError> {
    if metadata::check_manifest_complete(pool, store_path_hash).await? {
        return Ok(PlaceholderClaim::AlreadyComplete);
    }

    let claimed_by = stall.map(|s| s.claimed_by);

    // STRUCTURAL: insert_manifest_uploading takes references and writes
    // them into the placeholder narinfo. Mark's CTE walks them from
    // commit → the closure is GC-protected without holding a session
    // lock for the full upload.
    let mut claim =
        metadata::insert_manifest_uploading_as(pool, store_path_hash, store_path, refs, claimed_by)
            .await?;

    // r[impl store.substitute.stale-reclaim+3]
    // The slot is occupied. Three takeover arms, in precedence order:
    //
    //   (1) released-in-place row (`claim_id` NULL — the owner-side
    //       stall abort's leavings): claimable IMMEDIATELY by any
    //       caller, no threshold, `stall_count` preserved;
    //   (2) heartbeat death (5 min, unchanged): DELETE + re-insert —
    //       benign churn (deploys, scale-in, crashes) resets stall
    //       evidence, never accrues strikes. Checked BEFORE the stall
    //       arm so a dead owner is reaped, not striked, when both
    //       predicates hold;
    //   (3) download-stalled live claim (substitution callers only:
    //       `fetched_bytes < nar_size` ∧ progress clock older than the
    //       stall window): in-place handoff, `stall_count += 1`.
    if claim.is_none() {
        claim = metadata::claim_released_placeholder(pool, store_path_hash, claimed_by).await?;
        if claim.is_some() {
            debug!(
                %store_path,
                "{}: released-in-place placeholder — claimed immediately", hooks.ctx_label,
            );
        }
    }

    if claim.is_none() {
        // The stale-reclaim is a path-row janitor (reap_one): it
        // deletes the abandoned placeholder rows so this re-upload can
        // proceed; chunks the dead manifest referenced are the collect
        // cycle's business.
        let threshold = SUBSTITUTE_STALE_THRESHOLD.as_secs() as i64;
        match crate::gc::orphan::reap_one(pool, store_path_hash, ReapBy::Stale { secs: threshold })
            .await
        {
            Ok(true) => {
                warn!(
                    %store_path,
                    threshold = ?SUBSTITUTE_STALE_THRESHOLD,
                    "{}: stale 'uploading' placeholder — reclaimed", hooks.ctx_label,
                );
                metrics::counter!(hooks.stale_reclaimed_metric, "reason" => STALE_RECLAIM_HEARTBEAT)
                    .increment(1);
                // Propagate (?) — after reap_one Ok(true) the
                // placeholder is gone; collapsing Err into the
                // Concurrent path here would silently swallow a DB
                // failure with no log (asymmetric with the first
                // insert above).
                claim = metadata::insert_manifest_uploading_as(
                    pool,
                    store_path_hash,
                    store_path,
                    refs,
                    claimed_by,
                )
                .await?;
            }
            Ok(false) => {} // not stale → live concurrent uploader
            Err(e) => warn!(error = %e,
                "{}: stale-reclaim failed (proceeding to concurrent-abort)", hooks.ctx_label),
        }
    }

    if claim.is_none()
        && let Some(params) = stall
    {
        claim = metadata::stall_takeover_placeholder(
            pool,
            store_path_hash,
            claimed_by,
            params.nar_size,
            params.stall_window,
        )
        .await?;
        if claim.is_some() {
            warn!(
                %store_path,
                window = ?params.stall_window,
                "{}: download-stalled placeholder — taken over in place (strike recorded)",
                hooks.ctx_label,
            );
            metrics::counter!(hooks.stale_reclaimed_metric, "reason" => STALE_RECLAIM_STALL_RECLAIM)
                .increment(1);
        }
    }

    match claim {
        Some(id) => Ok(PlaceholderClaim::Owned(id)),
        None => Ok(PlaceholderClaim::Concurrent),
    }
}

/// How [`persist_nar`] failed. The caller maps this to its own error
/// domain (`tonic::Status` for gRPC, `SubstituteError` for
/// substitution) and decides whether to [`abort_placeholder`]: the
/// chunked path already rolled back internally.
#[derive(Debug)]
pub enum PersistError {
    /// `cas::put_chunked` failed. Its internal rollback (the
    /// claim-gated `reap_one`) already ran; the placeholder is GONE
    /// (best-effort). Caller's `abort_placeholder` is a harmless
    /// no-op but not required.
    Chunked(anyhow::Error),
    /// `complete_manifest_inline` failed. Caller still OWNS the
    /// placeholder and MUST `abort_placeholder`.
    Inline(MetadataError),
}

/// Persist a validated, hash-verified NAR for ONE output. Branches on
/// `nar_data.len()` vs [`cas::INLINE_THRESHOLD`]: inline goes to
/// `manifests.inline_blob` in one tx; chunked goes through
/// [`cas::put_chunked`] (FastCDC + S3 + chunk rows, own write-ahead +
/// rollback).
///
/// Caller must hold a [`PlaceholderClaim::Owned`] for
/// `info.store_path_hash`. Emits `rio_store_chunk_dedup_ratio` on the
/// chunked branch.
pub async fn persist_nar(
    pool: &PgPool,
    chunk_backend: Option<&Arc<dyn ChunkBackend>>,
    info: &ValidatedPathInfo,
    claim: Uuid,
    nar_data: Vec<u8>,
    chunk_upload_max_concurrent: usize,
    hooks: IngestHooks,
) -> Result<(), PersistError> {
    if let Some(backend) = cas::should_chunk(chunk_backend, nar_data.len()) {
        let stats = cas::put_chunked(
            pool,
            backend,
            info,
            claim,
            &nar_data,
            chunk_upload_max_concurrent,
        )
        .await
        .map_err(PersistError::Chunked)?;
        debug!(
            store_path = %info.store_path.as_str(),
            total_chunks = stats.total_chunks,
            deduped = stats.deduped_chunks,
            ratio = stats.dedup_ratio(),
            "{}: chunked upload completed", hooks.ctx_label,
        );
        metrics::gauge!("rio_store_chunk_dedup_ratio").set(stats.dedup_ratio());
    } else {
        metadata::complete_manifest_inline(pool, info, claim, Bytes::from(nar_data))
            .await
            .map_err(PersistError::Inline)?;
        debug!(store_path = %info.store_path.as_str(), "{}: inline upload completed", hooks.ctx_label);
    }
    Ok(())
}

/// Heartbeat cadence for [`PlaceholderGuard`]. Matches
/// `cas::HEARTBEAT_TIME_INTERVAL` (the chunk-upload heartbeat) and is
/// ≪ `SUBSTITUTE_STALE_THRESHOLD` (300s), so a live owner survives ≥9
/// missed heartbeats before stale-reclaim takes it.
///
/// UN-cfg'd (merged_bug_082): this is the PRODUCTION granularity that
/// `Config::validate`'s `substitute_stall >= 2×` floor references — a
/// cfg(test) value here would let the validation test a floor
/// production never enforces. The test-only 50ms override lives in
/// [`PLACEHOLDER_HEARTBEAT_TICK`] and applies to the guard's TICKER
/// only.
pub(crate) const PLACEHOLDER_HEARTBEAT_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(30);

/// The guard task's actual tick period. Production: the heartbeat
/// interval. Test override (50ms): the progress-heartbeat tests assert
/// the guard task's periodic write actually lands; a 30s first tick
/// would make that untestable.
#[cfg(not(test))]
const PLACEHOLDER_HEARTBEAT_TICK: std::time::Duration = PLACEHOLDER_HEARTBEAT_INTERVAL;
#[cfg(test)]
const PLACEHOLDER_HEARTBEAT_TICK: std::time::Duration = std::time::Duration::from_millis(50);

/// The owner-side claim phase, mirrored durably to
/// `manifests.claim_phase` by every progress heartbeat (migration 092,
/// merged_bug_003). The stall-takeover predicate strikes ONLY
/// `downloading` claims: an owner parked on the local NAR-byte budget
/// or persisting a fully-fetched NAR is alive and exempt AS DATA — the
/// pre-092 predicate inferred "persisting" from `fetched_bytes ==
/// nar_size`, which was only correct when the competitor\'s expected
/// size matched the owner\'s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ClaimPhase {
    /// Reading body bytes from the upstream (the only strikeable phase).
    Downloading = 0,
    /// Blocked on the local NAR-bytes budget — backpressure, not a
    /// stall; never strikeable.
    BudgetParked = 1,
    /// Fully fetched, persisting to the store — never strikeable.
    Persisting = 2,
}

impl ClaimPhase {
    /// The `manifests.claim_phase` text value (092 CHECK alphabet).
    pub(crate) fn as_sql(self) -> &'static str {
        match self {
            ClaimPhase::Downloading => "downloading",
            ClaimPhase::BudgetParked => "budget_parked",
            ClaimPhase::Persisting => "persisting",
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => ClaimPhase::BudgetParked,
            2 => ClaimPhase::Persisting,
            _ => ClaimPhase::Downloading,
        }
    }
}

/// The owner\'s live progress: decompressed bytes read plus the claim
/// phase, both lock-free (the read loop and the heartbeat task share
/// it). One handle per owned placeholder.
#[derive(Debug, Default)]
pub struct ProgressHandle {
    bytes: std::sync::atomic::AtomicU64,
    phase: std::sync::atomic::AtomicU8,
}

impl ProgressHandle {
    /// Fresh handle: 0 bytes, `Downloading`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Overwrite the byte count (the read loop\'s running total).
    pub fn store_bytes(&self, n: u64) {
        self.bytes.store(n, std::sync::atomic::Ordering::Relaxed);
    }

    /// Current byte count.
    pub fn bytes(&self) -> u64 {
        self.bytes.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Stamp the phase (the next heartbeat carries it durably).
    pub fn set_phase(&self, p: ClaimPhase) {
        self.phase
            .store(p as u8, std::sync::atomic::Ordering::Relaxed);
    }

    /// Current phase.
    pub fn phase(&self) -> ClaimPhase {
        ClaimPhase::from_u8(self.phase.load(std::sync::atomic::Ordering::Relaxed))
    }
}

/// RAII owner of an `'uploading'` placeholder: heartbeats while held,
/// reaps on drop. See [`spawn_placeholder_guard`].
// r[impl store.put.drop-cleanup+2]
pub(crate) struct PlaceholderGuard {
    heartbeat: tokio::task::JoinHandle<()>,
    pool: PgPool,
    store_path_hash: Vec<u8>,
    /// `r[store.put.placeholder-claim+2]`: ownership token from
    /// [`PlaceholderClaim::Owned`]. The drop-path reap filters
    /// `claim_id = $claim` so it's a no-op if our row was already
    /// reaped (orphan scanner / `cas::put_chunked` rollback) and a
    /// fresh re-upload now holds the slot.
    claim: Uuid,
    defused: bool,
}

impl PlaceholderGuard {
    /// Stop heartbeating and skip the drop-path reap. Call after the
    /// placeholder has been flipped to `'complete'` (or explicitly
    /// `abort_upload`ed — the reap would be a no-op, but the spawn is
    /// wasted).
    pub(crate) fn defuse(mut self) {
        self.defused = true;
    }
}

impl Drop for PlaceholderGuard {
    fn drop(&mut self) {
        self.heartbeat.abort();
        // The RAII pair of spawn_placeholder_guard's increment. Before
        // the defused early-return: a defused (successful) upload is
        // just as over as an aborted one.
        metrics::gauge!("rio_store_placeholders_uploading").decrement(1.0);
        if self.defused {
            return;
        }
        let pool = self.pool.clone();
        let store_path_hash = std::mem::take(&mut self.store_path_hash);
        let claim = self.claim;
        rio_common::task::spawn_monitored("put-path-placeholder-reap", async move {
            if let Err(e) =
                crate::gc::orphan::reap_one(&pool, &store_path_hash, ReapBy::Claim(claim)).await
            {
                warn!(
                    store_path_hash = %hex::encode(&store_path_hash),
                    error = %e,
                    "drop-path placeholder cleanup failed; orphan scanner will reclaim",
                );
            }
        });
    }
}

// r[impl store.put.drop-cleanup+2]
/// Drop-safety + liveness for a [`PlaceholderClaim::Owned`] placeholder.
/// Returns a [`PlaceholderGuard`] that:
///
/// - **heartbeats** `manifests.updated_at` every 30s while held, so
///   `r[store.put.stale-reclaim]`'s `reap_one(SUBSTITUTE_STALE_
///   THRESHOLD)` never reaps a live owner during a long ingest
///   (6 GB/50 Mbps ≈ 16 min);
/// - **on Drop** (owning future dropped — tonic aborts on client
///   RST_STREAM; a `try_substitute` caller times out — without an
///   explicit [`abort_placeholder`] or `'complete'` flip), spawns
///   `reap_one`. `reap_one` filters `status='uploading'` so firing after
///   an explicit abort/complete is a harmless no-op.
///
/// Call [`PlaceholderGuard::defuse`] on success.
///
/// `progress` (`r[store.substitute.progress-heartbeat]`): when `Some`,
/// each heartbeat carries the handle's current value (the owner's
/// decompressed-byte count, advanced by `Substituter::fetch_nar`'s
/// read loop) via [`cas::heartbeat_uploading_with_progress`] — still
/// one UPDATE per tick. PutPath/PutPathBatch pass `None`, so builder
/// claims keep `fetched_bytes` NULL — structurally exempt from every
/// stall rule keyed on progress evidence.
///
/// Shared by `PutPath` and `Substituter::try_upstream`; both run inline
/// in a request handler future and so share the same drop hazard.
// r[impl store.substitute.progress-heartbeat]
pub fn spawn_placeholder_guard(
    pool: PgPool,
    store_path_hash: Vec<u8>,
    claim: Uuid,
    progress: Option<Arc<ProgressHandle>>,
) -> PlaceholderGuard {
    let heartbeat = {
        let pool = pool.clone();
        let hash = store_path_hash.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PLACEHOLDER_HEARTBEAT_TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // first tick fires immediately; skip it
            loop {
                tick.tick().await;
                match &progress {
                    Some(h) => {
                        cas::heartbeat_uploading_with_progress(
                            &pool,
                            &hash,
                            claim,
                            h.bytes(),
                            h.phase().as_sql(),
                        )
                        .await;
                    }
                    None => cas::heartbeat_uploading(&pool, &hash, claim).await,
                }
            }
        })
    };
    // RAII in-flight gauge: +1 here, −1 in Drop (defused or not — the
    // upload is over either way). Per-replica live owned placeholders;
    // sum() across replicas = cluster in-flight ingest.
    metrics::gauge!("rio_store_placeholders_uploading").increment(1.0);
    PlaceholderGuard {
        heartbeat,
        pool,
        store_path_hash,
        claim,
        defused: false,
    }
}

/// Best-effort placeholder cleanup after a failed ingest: a claim-gated
/// path-row delete (`reap_one`). `claim` is the ownership token from
/// [`PlaceholderClaim::Owned`] — `reap_one` filters `claim_id = $claim`
/// so this is a no-op if the row was already reaped (orphan scanner /
/// `cas::put_chunked` rollback) AND a fresh re-upload now holds the
/// slot. Chunks the aborted upload staged are left for the collect
/// cycle.
pub async fn abort_placeholder(pool: &PgPool, store_path_hash: &[u8], claim: Uuid) {
    if let Err(e) = crate::gc::orphan::reap_one(pool, store_path_hash, ReapBy::Claim(claim)).await {
        warn!(
            store_path_hash = %hex::encode(store_path_hash),
            error = %e,
            "abort_placeholder: cleanup failed; orphan scanner will reclaim",
        );
    }
}

// ---------------------------------------------------------------------------
// Placeholder progress/stall battery (work item S)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// bug_265 parity: the const alphabet IS the label alphabet — every
    /// member appears in the canonical HELP text of BOTH metric
    /// families (lib.rs describe_counter, which docs/gen/metrics.json
    /// is generated from), and the array is duplicate-free. The emit
    /// sites reference the named members, so reachable-reason
    /// membership is structural. merged_bug_189: the putpath window is
    /// anchored too — the hooks indirection parametrizes the metric
    /// NAME, so every reason is reachable under either family and both
    /// HELPs must document the full alphabet (pre-fix, putpath carried
    /// reason=heartbeat its HELP never mentioned).
    #[test]
    fn stale_reclaim_reason_alphabet_matches_help() {
        let lib = include_str!("lib.rs");
        for family in [
            "rio_store_substitute_stale_reclaimed_total",
            "rio_store_putpath_stale_reclaimed_total",
        ] {
            let start = lib
                .find(family)
                .unwrap_or_else(|| panic!("{family} HELP present"));
            let help = &lib[start..start + 700];
            for reason in STALE_RECLAIM_REASONS {
                assert!(
                    help.contains(reason),
                    "{family} HELP must name reason '{reason}'"
                );
            }
        }
        let mut sorted = STALE_RECLAIM_REASONS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), STALE_RECLAIM_REASONS.len(), "no duplicates");
    }
    use std::time::Duration;

    use rio_test_support::TestDb;

    const TEST_HOOKS: IngestHooks = IngestHooks {
        stale_reclaimed_metric: "rio_store_substitute_stale_reclaimed_total",
        ctx_label: "ingest-test",
    };

    /// Progress/stall column probe for one manifests row.
    /// `(status, claim_id, fetched_bytes, last_progress_at_epoch,
    ///   stall_count, claimed_by, updated_at_epoch)`.
    type RowState = (
        String,
        Option<Uuid>,
        Option<i64>,
        Option<f64>,
        i16,
        Option<String>,
        f64,
    );

    async fn row_state(pool: &PgPool, hash: &[u8]) -> Option<RowState> {
        sqlx::query_as(
            "SELECT status, claim_id, fetched_bytes, \
                    EXTRACT(EPOCH FROM last_progress_at)::float8, \
                    stall_count, claimed_by, \
                    EXTRACT(EPOCH FROM updated_at)::float8 \
               FROM manifests WHERE store_path_hash = $1",
        )
        .bind(hash)
        .fetch_optional(pool)
        .await
        .expect("row_state query")
    }

    /// Claim a fresh placeholder, panicking on any non-Owned outcome.
    async fn claim_owned(pool: &PgPool, hash: &[u8], path: &str) -> Uuid {
        match claim_placeholder(pool, hash, path, &[], TEST_HOOKS, None)
            .await
            .expect("claim_placeholder")
        {
            PlaceholderClaim::Owned(c) => c,
            PlaceholderClaim::AlreadyComplete => panic!("unexpected AlreadyComplete"),
            PlaceholderClaim::Concurrent => panic!("unexpected Concurrent"),
        }
    }

    /// The substitution-shaped claim: stall params present (nar_size
    /// 1000, window 60 s, pod "claimant-pod").
    async fn claim_substitute(pool: &PgPool, hash: &[u8], path: &str) -> PlaceholderClaim {
        claim_placeholder(
            pool,
            hash,
            path,
            &[],
            TEST_HOOKS,
            Some(&SubstituteClaimParams {
                nar_size: 1000,
                stall_window: Duration::from_secs(60),
                claimed_by: "claimant-pod",
            }),
        )
        .await
        .expect("claim_placeholder")
    }

    /// Backdate the stall-relevant clocks on a row, keeping liveness
    /// (`updated_at`) fresh unless `dead_heartbeat`.
    async fn age_row(pool: &PgPool, hash: &[u8], progress_age_secs: i64, dead_heartbeat: bool) {
        let hb_age = if dead_heartbeat { 600 } else { 0 };
        sqlx::query(
            "UPDATE manifests SET \
                 last_progress_at = now() - make_interval(secs => $2), \
                 updated_at = now() - make_interval(secs => $3) \
             WHERE store_path_hash = $1",
        )
        .bind(hash)
        .bind(progress_age_secs)
        .bind(hb_age)
        .execute(pool)
        .await
        .expect("age_row");
    }

    fn test_hash(tag: u8) -> Vec<u8> {
        vec![tag; 32]
    }

    /// `manifests.claim_phase` of a row (092 battery helper).
    async fn row_phase(pool: &PgPool, hash: &[u8]) -> Option<String> {
        sqlx::query_scalar("SELECT claim_phase FROM manifests WHERE store_path_hash = $1")
            .bind(hash)
            .fetch_one(pool)
            .await
            .expect("row_phase")
    }

    /// Stamp a phase directly (the production writer is the heartbeat;
    /// the battery pins the SQL rule, not the task timing).
    async fn set_phase(pool: &PgPool, hash: &[u8], phase: Option<&str>) {
        sqlx::query("UPDATE manifests SET claim_phase = $2 WHERE store_path_hash = $1")
            .bind(hash)
            .bind(phase)
            .execute(pool)
            .await
            .expect("set_phase");
    }

    /// Stamp progress evidence directly.
    async fn set_progress(pool: &PgPool, hash: &[u8], fetched: i64) {
        sqlx::query(
            "UPDATE manifests SET fetched_bytes = $2, last_progress_at = now() \
             WHERE store_path_hash = $1",
        )
        .bind(hash)
        .bind(fetched)
        .execute(pool)
        .await
        .expect("set_progress");
    }

    // r[verify store.substitute.stale-reclaim+3]
    /// merged_bug_003 (a): an ALIVE owner parked on the local byte
    /// budget — progress frozen past the stall window, liveness fresh,
    /// `claim_phase = 'budget_parked'` — is EXEMPT from takeover as
    /// data. Pre-092 RED: the predicate had no phase/liveness conjunct
    /// and deposed the live backpressured owner.
    #[tokio::test]
    async fn takeover_exempts_alive_budget_parked_owner() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash = test_hash(0xb1);
        let _claim = claim_owned(&db.pool, &hash, "/nix/store/b1-parked").await;
        set_progress(&db.pool, &hash, 100).await;
        set_phase(&db.pool, &hash, Some("budget_parked")).await;
        age_row(&db.pool, &hash, 120, false).await; // progress stale, liveness fresh

        let took = crate::metadata::stall_takeover_placeholder(
            &db.pool,
            &hash,
            Some("competitor-pod"),
            1000,
            Duration::from_secs(60),
        )
        .await
        .expect("takeover query");
        assert!(took.is_none(), "an alive parked owner is never deposed");
        let (_, _, _, _, strikes, _, _) = row_state(&db.pool, &hash).await.expect("row");
        assert_eq!(strikes, 0, "no strike accrued");
    }

    // r[verify store.substitute.stale-reclaim+3]
    /// merged_bug_003 (b): a DEAD owner (no heartbeat 200s) is NOT
    /// striked by the takeover (liveness conjunct fails) and not yet
    /// reaped (300s threshold) — the competitor sees Concurrent. Past
    /// 300s the heartbeat-death reap arm wins and the fresh claim
    /// starts at stall_count 0: reaped, never striked. Pre-092 RED:
    /// the 180-300s window striked the corpse.
    #[tokio::test]
    async fn dead_owner_routes_to_reap_not_strike() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash = test_hash(0xb2);
        let _claim = claim_owned(&db.pool, &hash, "/nix/store/b2-dead").await;
        set_progress(&db.pool, &hash, 100).await;
        set_phase(&db.pool, &hash, Some("downloading")).await;
        // Progress stale AND liveness stale (200s) — dead, pre-reap.
        sqlx::query(
            "UPDATE manifests SET last_progress_at = now() - make_interval(secs => 200), \
             updated_at = now() - make_interval(secs => 200) WHERE store_path_hash = $1",
        )
        .bind(&hash)
        .execute(&db.pool)
        .await
        .unwrap();

        let took = crate::metadata::stall_takeover_placeholder(
            &db.pool,
            &hash,
            Some("competitor-pod"),
            1000,
            Duration::from_secs(60),
        )
        .await
        .expect("takeover query");
        assert!(took.is_none(), "a dead owner is not striked");
        match claim_substitute(&db.pool, &hash, "/nix/store/b2-dead").await {
            PlaceholderClaim::Concurrent => {}
            _ => panic!("pre-reap dead owner must read Concurrent"),
        }

        // Past the 300s reap threshold: the death arm reaps and the
        // fresh claim carries ZERO strikes.
        sqlx::query(
            "UPDATE manifests SET updated_at = now() - make_interval(secs => 400) \
             WHERE store_path_hash = $1",
        )
        .bind(&hash)
        .execute(&db.pool)
        .await
        .unwrap();
        match claim_substitute(&db.pool, &hash, "/nix/store/b2-dead").await {
            PlaceholderClaim::Owned(_) => {}
            _ => panic!("post-threshold dead owner must be reaped"),
        }
        let (_, _, _, _, strikes, _, _) = row_state(&db.pool, &hash).await.expect("row");
        assert_eq!(
            strikes, 0,
            "reaped-not-striked: the fresh claim starts clean"
        );
    }

    // r[verify store.substitute.stale-reclaim+3]
    /// merged_bug_003 (c): a PERSISTING owner is exempt even when the
    /// competitor expects a BIGGER NAR. Pre-092 RED: the persist
    /// exemption was the inference `fetched_bytes == nar_size`, which
    /// only held when the competitor\'s expected size equalled the
    /// owner\'s — a bigger expectation re-opened the strike.
    #[tokio::test]
    async fn takeover_exempts_persisting_owner_regardless_of_competitor_size() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash = test_hash(0xb3);
        let _claim = claim_owned(&db.pool, &hash, "/nix/store/b3-persist").await;
        set_progress(&db.pool, &hash, 1000).await; // fully fetched (owner\'s size)
        set_phase(&db.pool, &hash, Some("persisting")).await;
        age_row(&db.pool, &hash, 120, false).await;

        let took = crate::metadata::stall_takeover_placeholder(
            &db.pool,
            &hash,
            Some("competitor-pod"),
            2000, // competitor expects MORE than the owner fetched
            Duration::from_secs(60),
        )
        .await
        .expect("takeover query");
        assert!(
            took.is_none(),
            "persisting is exempt AS DATA, not by size equality"
        );
    }

    // r[verify store.substitute.stale-reclaim+3]
    /// merged_bug_003 (d): the intended victim — an ALIVE owner whose
    /// DOWNLOAD is wedged (phase downloading, progress frozen past the
    /// window, liveness fresh) — IS taken over in place, with the
    /// strike recorded (non-vacuity of the whole battery).
    #[tokio::test]
    async fn takeover_strikes_wedged_downloading_owner() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash = test_hash(0xb4);
        let claim = claim_owned(&db.pool, &hash, "/nix/store/b4-wedged").await;
        set_progress(&db.pool, &hash, 500).await;
        set_phase(&db.pool, &hash, Some("downloading")).await;
        age_row(&db.pool, &hash, 120, false).await;

        let took = crate::metadata::stall_takeover_placeholder(
            &db.pool,
            &hash,
            Some("competitor-pod"),
            1000,
            Duration::from_secs(60),
        )
        .await
        .expect("takeover query");
        let new_claim = took.expect("wedged downloading owner is deposed");
        assert_ne!(new_claim, claim, "in-place handoff mints a new claim token");
        let (_, _, fetched, _, strikes, by, _) = row_state(&db.pool, &hash).await.expect("row");
        assert_eq!(strikes, 1, "one stall, one strike");
        assert_eq!(fetched, None, "progress reset for the new owner");
        assert_eq!(by.as_deref(), Some("competitor-pod"));
        assert_eq!(
            row_phase(&db.pool, &hash).await.as_deref(),
            Some("downloading"),
            "the new owner starts in the downloading phase"
        );
    }

    // r[verify store.substitute.progress-heartbeat]
    /// The guard\'s heartbeat carries the handle\'s PHASE durably (092_manifests_claim_phase):
    /// stamp BudgetParked on the handle and the next tick mirrors it
    /// to `manifests.claim_phase`.
    #[tokio::test]
    async fn guard_heartbeat_carries_claim_phase() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash = test_hash(0xb5);
        let claim = claim_owned(&db.pool, &hash, "/nix/store/b5-phase").await;
        let handle = Arc::new(ProgressHandle::new());
        let guard = spawn_placeholder_guard(
            db.pool.clone(),
            hash.clone(),
            claim,
            Some(Arc::clone(&handle)),
        );
        handle.store_bytes(64);
        handle.set_phase(ClaimPhase::BudgetParked);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            row_phase(&db.pool, &hash).await.as_deref(),
            Some("budget_parked"),
            "the heartbeat mirrors the handle phase"
        );
        handle.set_phase(ClaimPhase::Persisting);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            row_phase(&db.pool, &hash).await.as_deref(),
            Some("persisting")
        );
        guard.defuse();
    }

    // r[verify store.substitute.progress-heartbeat]
    /// The with-progress heartbeat is one claim-guarded UPDATE that
    /// writes `fetched_bytes` and advances `last_progress_at` ONLY
    /// when the byte count changed — the stuck≠slow discriminator.
    /// A wrong claim (stale owner) writes nothing.
    #[tokio::test]
    async fn progress_heartbeat_advances_only_on_change() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash = test_hash(0xa1);
        let claim = claim_owned(&db.pool, &hash, "/nix/store/aa-progress-hb").await;

        crate::cas::heartbeat_uploading_with_progress(&db.pool, &hash, claim, 100, "downloading")
            .await;
        let (_, _, fetched1, lp1, _, _, _) = row_state(&db.pool, &hash).await.expect("row");
        assert_eq!(fetched1, Some(100), "first heartbeat lands fetched_bytes");
        let lp1 = lp1.expect("first progress write sets last_progress_at");

        // Same value → liveness bumps, progress clock does NOT.
        tokio::time::sleep(Duration::from_millis(30)).await;
        crate::cas::heartbeat_uploading_with_progress(&db.pool, &hash, claim, 100, "downloading")
            .await;
        let (_, _, fetched2, lp2, _, _, _) = row_state(&db.pool, &hash).await.expect("row");
        assert_eq!(fetched2, Some(100));
        assert_eq!(
            lp2,
            Some(lp1),
            "unchanged byte count must NOT advance last_progress_at (a wedged \
             owner's heartbeat keeps liveness fresh while the progress clock freezes)"
        );

        // Larger value → progress clock advances.
        tokio::time::sleep(Duration::from_millis(30)).await;
        crate::cas::heartbeat_uploading_with_progress(&db.pool, &hash, claim, 200, "downloading")
            .await;
        let (_, _, fetched3, lp3, _, _, _) = row_state(&db.pool, &hash).await.expect("row");
        assert_eq!(fetched3, Some(200));
        assert!(
            lp3.expect("set") > lp1,
            "advancing byte count must advance last_progress_at"
        );

        // Claim guard: a stale owner's heartbeat is a no-op.
        crate::cas::heartbeat_uploading_with_progress(
            &db.pool,
            &hash,
            Uuid::new_v4(),
            999,
            "downloading",
        )
        .await;
        let (_, _, fetched4, _, _, _, _) = row_state(&db.pool, &hash).await.expect("row");
        assert_eq!(
            fetched4,
            Some(200),
            "a heartbeat under a foreign claim_id must not write progress"
        );
    }

    // r[verify store.substitute.progress-heartbeat]
    /// The guard plumbing end-to-end: a guard spawned WITH a progress
    /// handle lands the handle's value in `fetched_bytes` on its
    /// periodic tick; a guard WITHOUT one (the PutPath shape) keeps
    /// `fetched_bytes` NULL forever — the structural exemption every
    /// stall rule keys on.
    #[tokio::test]
    async fn guard_progress_handle_lands_putpath_stays_null() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Substitution-shaped guard: progress handle wired.
        let hash_sub = test_hash(0xa2);
        let claim_sub = claim_owned(&db.pool, &hash_sub, "/nix/store/ab-guard-sub").await;
        let handle = Arc::new(ProgressHandle::new());
        let guard_sub = spawn_placeholder_guard(
            db.pool.clone(),
            hash_sub.clone(),
            claim_sub,
            Some(Arc::clone(&handle)),
        );
        handle.store_bytes(4096);

        // PutPath-shaped guard: no handle.
        let hash_put = test_hash(0xa3);
        let claim_put = claim_owned(&db.pool, &hash_put, "/nix/store/ac-guard-put").await;
        let guard_put = spawn_placeholder_guard(db.pool.clone(), hash_put.clone(), claim_put, None);

        // ≥3 test-cadence ticks (50ms each).
        tokio::time::sleep(Duration::from_millis(300)).await;

        let (_, _, fetched_sub, lp_sub, _, _, _) =
            row_state(&db.pool, &hash_sub).await.expect("sub row");
        assert_eq!(
            fetched_sub,
            Some(4096),
            "the guard's periodic heartbeat must carry the progress handle's value"
        );
        assert!(lp_sub.is_some(), "progress write must set last_progress_at");

        let (_, _, fetched_put, lp_put, _, _, _) =
            row_state(&db.pool, &hash_put).await.expect("put row");
        assert_eq!(
            fetched_put, None,
            "a PutPath claim (no progress handle) must keep fetched_bytes NULL"
        );
        assert_eq!(lp_put, None, "...and never set last_progress_at");

        guard_sub.defuse();
        guard_put.defuse();
    }

    // r[verify store.substitute.progress-heartbeat]
    /// `rio_store_placeholders_uploading` tracks live owned
    /// placeholders RAII-style: +1 per guard spawn, −1 per guard drop
    /// (defused or not — the upload is over either way). The gauge is
    /// the per-replica in-flight ingest signal the dashboards read.
    ///
    /// metrics-util's debugging `snapshot()` is DESTRUCTIVE (`swap(0)`
    /// per read), so each read below observes the DELTA since the
    /// previous read — +1/+1/−1/−1 pins the inc/dec pairing exactly.
    #[tokio::test]
    async fn placeholders_uploading_gauge_tracks_guard_lifecycle() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let rec = DebuggingRecorder::new();
        let snap = rec.snapshotter();
        let _guard = metrics::set_default_local_recorder(&rec);

        let gauge_delta = |snap: &metrics_util::debugging::Snapshotter| -> Option<f64> {
            snap.snapshot()
                .into_vec()
                .into_iter()
                .find_map(|(ck, _, _, v)| {
                    (ck.key().name() == "rio_store_placeholders_uploading").then(|| match v {
                        DebugValue::Gauge(g) => g.into_inner(),
                        _ => f64::NAN,
                    })
                })
        };

        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash_a = test_hash(0xa4);
        let claim_a = claim_owned(&db.pool, &hash_a, "/nix/store/ad-gauge-a").await;
        let g_a = spawn_placeholder_guard(db.pool.clone(), hash_a, claim_a, None);
        assert_eq!(gauge_delta(&snap), Some(1.0), "guard spawn increments");

        let hash_b = test_hash(0xa5);
        let claim_b = claim_owned(&db.pool, &hash_b, "/nix/store/ae-gauge-b").await;
        let g_b = spawn_placeholder_guard(db.pool.clone(), hash_b, claim_b, None);
        assert_eq!(
            gauge_delta(&snap),
            Some(1.0),
            "second spawn increments again"
        );

        drop(g_a); // un-defused drop (the abort path)
        assert_eq!(
            gauge_delta(&snap),
            Some(-1.0),
            "dropped guard decrements (abort path)"
        );

        g_b.defuse(); // defused consume (the success path)
        assert_eq!(
            gauge_delta(&snap),
            Some(-1.0),
            "defused guard decrements too (success path)"
        );
    }

    // ── The four-case stall battery (review-findings watch-item 1) ──

    // r[verify store.substitute.stale-reclaim+3]
    /// Case 1: a frozen mid-download claim (progress evidence present,
    /// `fetched_bytes < nar_size`, progress clock older than the stall
    /// window, heartbeat ALIVE) is taken over IN PLACE: new
    /// claim_id/claimed_by, progress reset, `stall_count += 1` — the
    /// download-stalled reclaim arm.
    #[tokio::test]
    async fn stall_reclaim_takes_over_frozen_mid_download() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash = test_hash(0xb1);
        let owner = claim_owned(&db.pool, &hash, "/nix/store/ba-frozen").await;

        // Owner reported progress (100 < 1000), then froze 120s ago
        // (window 60s); heartbeat stays fresh (alive-but-wedged).
        crate::cas::heartbeat_uploading_with_progress(&db.pool, &hash, owner, 100, "downloading")
            .await;
        age_row(&db.pool, &hash, 120, false).await;

        let claim = claim_substitute(&db.pool, &hash, "/nix/store/ba-frozen").await;
        let new_claim = match claim {
            PlaceholderClaim::Owned(c) => c,
            other => panic!(
                "a frozen mid-download claim must be stall-reclaimed, got {}",
                match other {
                    PlaceholderClaim::AlreadyComplete => "AlreadyComplete",
                    PlaceholderClaim::Concurrent => "Concurrent",
                    PlaceholderClaim::Owned(_) => unreachable!(),
                }
            ),
        };
        assert_ne!(new_claim, owner, "in-place handoff mints a NEW claim");

        let (status, claim_id, fetched, lp, stalls, claimed_by, _) =
            row_state(&db.pool, &hash).await.expect("row survives");
        assert_eq!(status, "uploading", "in-place: the row was NOT deleted");
        assert_eq!(claim_id, Some(new_claim));
        assert_eq!(fetched, None, "progress evidence reset for the new owner");
        assert_eq!(lp, None);
        assert_eq!(stalls, 1, "the stall event accrued exactly one strike");
        assert_eq!(claimed_by.as_deref(), Some("claimant-pod"));
    }

    // r[verify store.substitute.stale-reclaim+3]
    /// Case 2: an ADVANCING download (progress clock fresh) is never
    /// stall-reclaimed — slow ≠ stuck.
    #[tokio::test]
    async fn stall_reclaim_skips_advancing_download() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash = test_hash(0xb2);
        let owner = claim_owned(&db.pool, &hash, "/nix/store/bb-advancing").await;
        crate::cas::heartbeat_uploading_with_progress(&db.pool, &hash, owner, 100, "downloading")
            .await;
        // Progress clock fresh (just heartbeated); no aging.

        let claim = claim_substitute(&db.pool, &hash, "/nix/store/bb-advancing").await;
        assert!(
            matches!(claim, PlaceholderClaim::Concurrent),
            "an advancing owner must keep its claim"
        );
        let (_, claim_id, _, _, stalls, _, _) = row_state(&db.pool, &hash).await.expect("row");
        assert_eq!(claim_id, Some(owner), "ownership unchanged");
        assert_eq!(stalls, 0, "no strike for a healthy transfer");
    }

    // r[verify store.substitute.stale-reclaim+3]
    /// Case 3: a PutPath claim (`fetched_bytes` NULL — no progress
    /// handle) is structurally exempt from the stall arm no matter how
    /// old its progress-free state is, as long as it heartbeats.
    #[tokio::test]
    async fn stall_reclaim_skips_putpath_claim() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash = test_hash(0xb3);
        let owner = claim_owned(&db.pool, &hash, "/nix/store/bc-putpath").await;
        // Liveness fresh; fetched_bytes NULL (never heartbeated with
        // progress). last_progress_at NULL → predicate can't match.

        let claim = claim_substitute(&db.pool, &hash, "/nix/store/bc-putpath").await;
        assert!(
            matches!(claim, PlaceholderClaim::Concurrent),
            "a PutPath claim must never be stall-reclaimed"
        );
        let (_, claim_id, fetched, _, stalls, _, _) =
            row_state(&db.pool, &hash).await.expect("row");
        assert_eq!(claim_id, Some(owner));
        assert_eq!(fetched, None);
        assert_eq!(stalls, 0);
    }

    // r[verify store.substitute.stale-reclaim+3]
    /// Case 4: a download-complete (persist-phase) claim
    /// (`fetched_bytes == nar_size`) is NEVER stall-reclaimable — only
    /// the 5-minute heartbeat-death rule applies there. Stealing
    /// mid-persist would discard a finished multi-GiB download.
    #[tokio::test]
    async fn stall_reclaim_skips_persist_phase() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash = test_hash(0xb4);
        let owner = claim_owned(&db.pool, &hash, "/nix/store/bd-persist").await;
        // Download complete: fetched == nar_size (1000); progress clock
        // long stale (the persist phase legitimately writes no
        // progress); heartbeat alive (chunk-upload loop keeps it fresh).
        crate::cas::heartbeat_uploading_with_progress(&db.pool, &hash, owner, 1000, "downloading")
            .await;
        age_row(&db.pool, &hash, 600, false).await;

        let claim = claim_substitute(&db.pool, &hash, "/nix/store/bd-persist").await;
        assert!(
            matches!(claim, PlaceholderClaim::Concurrent),
            "a persist-phase claim must never be stall-stolen"
        );
        let (_, claim_id, fetched, _, stalls, _, _) =
            row_state(&db.pool, &hash).await.expect("row");
        assert_eq!(claim_id, Some(owner));
        assert_eq!(fetched, Some(1000));
        assert_eq!(stalls, 0);
    }

    // ── Release-in-place / strike-once / reset ──

    // r[verify store.substitute.stale-reclaim+3]
    // r[verify store.substitute.stall-abort+2]
    /// A released-in-place row (the owner-side stall abort's leavings:
    /// `claim_id` NULL, `stall_count` recorded) is claimable
    /// IMMEDIATELY by any caller — no staleness threshold — with the
    /// stall evidence preserved.
    #[tokio::test]
    async fn released_row_immediately_claimable_stall_count_preserved() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash = test_hash(0xb5);
        let owner = claim_owned(&db.pool, &hash, "/nix/store/be-released").await;
        crate::cas::heartbeat_uploading_with_progress(&db.pool, &hash, owner, 100, "downloading")
            .await;

        // The owner-side abort releases in place.
        let released = metadata::release_placeholder_in_place(&db.pool, &hash, owner)
            .await
            .expect("release");
        assert!(released, "the owning claim releases its own row");
        let (status, claim_id, fetched, lp, stalls, claimed_by, _) = row_state(&db.pool, &hash)
            .await
            .expect("row survives release");
        assert_eq!(status, "uploading");
        assert_eq!(claim_id, None, "claim relinquished");
        assert_eq!(fetched, None, "progress NULLed");
        assert_eq!(lp, None);
        assert_eq!(stalls, 1, "the abort recorded its strike");
        assert_eq!(claimed_by, None);

        // IMMEDIATELY claimable — fresh row (updated_at = now()), no
        // threshold — even by a PutPath-shaped caller (params: None).
        let claim = claim_owned(&db.pool, &hash, "/nix/store/be-released").await;
        let (_, claim_id, _, _, stalls, _, _) = row_state(&db.pool, &hash).await.expect("row");
        assert_eq!(claim_id, Some(claim), "released row claimed in place");
        assert_eq!(stalls, 1, "stall evidence survives the handoff");
    }

    // r[verify store.substitute.stale-reclaim+3]
    // r[verify store.substitute.stall-abort+2]
    /// Claim-guarded strike-once, both interleavings:
    /// (a) a competing stall-reclaim lands first → the owner's late
    ///     release matches zero rows (claim changed) → stall_count
    ///     stays 1;
    /// (b) the owner's release lands first → a competing stall-
    ///     takeover matches zero rows (`fetched_bytes` NULL) and the
    ///     competitor claims via the released arm instead → stall_count
    ///     stays 1.
    #[tokio::test]
    async fn abort_vs_reclaim_race_strikes_exactly_once() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // (a) reclaim first, release late.
        let hash = test_hash(0xb6);
        let owner = claim_owned(&db.pool, &hash, "/nix/store/bf-race-a").await;
        crate::cas::heartbeat_uploading_with_progress(&db.pool, &hash, owner, 100, "downloading")
            .await;
        age_row(&db.pool, &hash, 120, false).await;
        let taken = metadata::stall_takeover_placeholder(
            &db.pool,
            &hash,
            Some("competitor"),
            1000,
            Duration::from_secs(60),
        )
        .await
        .expect("takeover");
        assert!(taken.is_some(), "the competing reclaim wins the row");
        let released = metadata::release_placeholder_in_place(&db.pool, &hash, owner)
            .await
            .expect("late release");
        assert!(
            !released,
            "the deposed owner's release is claim-guarded out"
        );
        let (_, _, _, _, stalls, _, _) = row_state(&db.pool, &hash).await.expect("row");
        assert_eq!(stalls, 1, "one stall event, one strike (reclaim-first)");

        // (b) release first, takeover late.
        let hash = test_hash(0xb7);
        let owner = claim_owned(&db.pool, &hash, "/nix/store/bg-race-b").await;
        crate::cas::heartbeat_uploading_with_progress(&db.pool, &hash, owner, 100, "downloading")
            .await;
        age_row(&db.pool, &hash, 120, false).await;
        let released = metadata::release_placeholder_in_place(&db.pool, &hash, owner)
            .await
            .expect("release");
        assert!(released);
        let taken = metadata::stall_takeover_placeholder(
            &db.pool,
            &hash,
            Some("competitor"),
            1000,
            Duration::from_secs(60),
        )
        .await
        .expect("late takeover");
        assert!(
            taken.is_none(),
            "the takeover arm must not match a released row (fetched_bytes NULL)"
        );
        // The competitor's full claim path picks it up via the
        // released arm instead — without another strike.
        let claim = claim_substitute(&db.pool, &hash, "/nix/store/bg-race-b").await;
        assert!(matches!(claim, PlaceholderClaim::Owned(_)));
        let (_, _, _, _, stalls, _, _) = row_state(&db.pool, &hash).await.expect("row");
        assert_eq!(stalls, 1, "one stall event, one strike (release-first)");
    }

    // r[verify store.substitute.stale-reclaim+3]
    /// Heartbeat-death keeps DELETING (the unchanged 5-minute rule),
    /// which RESETS stall evidence: benign churn (deploys, scale-in,
    /// crashes) never accrues strikes. The dead-owner arm wins over
    /// the stall arm when both hold.
    #[tokio::test]
    async fn heartbeat_death_reap_resets_stall_count() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash = test_hash(0xb8);
        let owner = claim_owned(&db.pool, &hash, "/nix/store/bh-dead").await;
        crate::cas::heartbeat_uploading_with_progress(&db.pool, &hash, owner, 100, "downloading")
            .await;
        // Two prior strikes on the row...
        sqlx::query("UPDATE manifests SET stall_count = 2 WHERE store_path_hash = $1")
            .bind(hash.as_slice())
            .execute(&db.pool)
            .await
            .unwrap();
        // ...then the owner dies outright: progress AND heartbeat stale
        // (a dead owner stops both clocks). The frozen-download
        // predicate would also match — the DELETE arm must win.
        age_row(&db.pool, &hash, 600, true).await;

        let claim = claim_substitute(&db.pool, &hash, "/nix/store/bh-dead").await;
        assert!(matches!(claim, PlaceholderClaim::Owned(_)));
        let (_, _, fetched, _, stalls, _, _) = row_state(&db.pool, &hash).await.expect("row");
        assert_eq!(
            stalls, 0,
            "heartbeat death deletes the row — strikes reset, never inherited"
        );
        assert_eq!(fetched, None);
    }

    // r[verify store.substitute.stale-reclaim+3]
    /// The reclaim counter is reason-labeled: `heartbeat` for the
    /// death-DELETE arm, `stall_reclaim` for the in-place takeover.
    /// (`stall_abort` is emitted at the owner-side abort site in
    /// substitute.rs.) Single destructive snapshot at the end.
    #[tokio::test]
    async fn reclaim_counter_carries_reasons() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let rec = DebuggingRecorder::new();
        let snap = rec.snapshotter();
        let _guard = metrics::set_default_local_recorder(&rec);

        let db = TestDb::new(&crate::MIGRATOR).await;

        // heartbeat-death reclaim.
        let hash = test_hash(0xb9);
        claim_owned(&db.pool, &hash, "/nix/store/bi-hb-dead").await;
        age_row(&db.pool, &hash, 600, true).await;
        claim_owned(&db.pool, &hash, "/nix/store/bi-hb-dead").await;

        // stall reclaim.
        let hash2 = test_hash(0xba);
        let owner = claim_owned(&db.pool, &hash2, "/nix/store/bj-stalled").await;
        crate::cas::heartbeat_uploading_with_progress(&db.pool, &hash2, owner, 100, "downloading")
            .await;
        age_row(&db.pool, &hash2, 120, false).await;
        assert!(matches!(
            claim_substitute(&db.pool, &hash2, "/nix/store/bj-stalled").await,
            PlaceholderClaim::Owned(_)
        ));

        let mut by_reason: std::collections::BTreeMap<String, u64> = Default::default();
        for (ck, _, _, v) in snap.snapshot().into_vec() {
            let DebugValue::Counter(c) = v else { continue };
            if ck.key().name() == "rio_store_substitute_stale_reclaimed_total" {
                let reason = ck
                    .key()
                    .labels()
                    .find(|l| l.key() == "reason")
                    .map(|l| l.value().to_owned())
                    .unwrap_or_else(|| "<unlabeled>".into());
                *by_reason.entry(reason).or_default() += c;
            }
        }
        assert_eq!(
            by_reason.get("heartbeat").copied(),
            Some(1),
            "death-DELETE arm counts reason=heartbeat; got {by_reason:?}"
        );
        assert_eq!(
            by_reason.get("stall_reclaim").copied(),
            Some(1),
            "in-place takeover counts reason=stall_reclaim; got {by_reason:?}"
        );
    }
}
