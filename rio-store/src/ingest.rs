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
#[derive(Clone, Copy)]
pub struct IngestHooks {
    /// `metrics::counter!` name incremented when a stale `'uploading'`
    /// placeholder is reaped on the hot path. e.g.
    /// `rio_store_putpath_stale_reclaimed_total`.
    pub stale_reclaimed_metric: &'static str,
    /// Prefix for `warn!`/`debug!` log lines (e.g. `"PutPath"`,
    /// `"substitute"`).
    pub ctx_label: &'static str,
}

// r[impl store.put.idempotent]
// r[impl store.put.stale-reclaim]
/// Idempotency check + `status='uploading'` placeholder insert +
/// hot-path stale-reclaim. The shared step-1 of the write-ahead flow.
///
/// Flow:
/// 1. `check_manifest_complete` → [`PlaceholderClaim::AlreadyComplete`]
/// 2. `insert_manifest_uploading` → if inserted: [`PlaceholderClaim::Owned`]
/// 3. ON CONFLICT no-op: try `reap_one` with the stale threshold
///    (I-207 — a fetcher that died mid-upload leaves a placeholder
///    the orphan scanner won't reap for 15min, but the scheduler
///    retries within seconds). If reap succeeded, re-insert.
/// 4. Still not inserted → [`PlaceholderClaim::Concurrent`] (live
///    uploader's heartbeat keeps `updated_at` fresh, so reap_one's
///    threshold check protected it).
///
/// Per-caller metrics (`exists` / `concurrent_upload` on
/// `rio_store_put_path_total`) are NOT emitted here — that's a
/// PutPath-specific counter. Only the stale-reclaim counter (whose
/// name the caller supplies) is emitted.
pub async fn claim_placeholder(
    pool: &PgPool,
    store_path_hash: &[u8],
    store_path: &str,
    refs: &[String],
    hooks: IngestHooks,
) -> Result<PlaceholderClaim, MetadataError> {
    if metadata::check_manifest_complete(pool, store_path_hash).await? {
        return Ok(PlaceholderClaim::AlreadyComplete);
    }

    // STRUCTURAL: insert_manifest_uploading takes references and writes
    // them into the placeholder narinfo. Mark's CTE walks them from
    // commit → the closure is GC-protected without holding a session
    // lock for the full upload.
    let mut claim =
        metadata::insert_manifest_uploading(pool, store_path_hash, store_path, refs).await?;

    if claim.is_none() {
        // r[impl store.substitute.stale-reclaim]
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
                metrics::counter!(hooks.stale_reclaimed_metric).increment(1);
                // Propagate (?) — after reap_one Ok(true) the
                // placeholder is gone; collapsing Err into the
                // Concurrent path here would silently swallow a DB
                // failure with no log (asymmetric with line 101).
                claim =
                    metadata::insert_manifest_uploading(pool, store_path_hash, store_path, refs)
                        .await?;
            }
            Ok(false) => {} // not stale → live concurrent uploader
            Err(e) => warn!(error = %e,
                "{}: stale-reclaim failed (proceeding to concurrent-abort)", hooks.ctx_label),
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
/// Test override (50ms): the progress-heartbeat tests assert the guard
/// task's periodic write actually lands; a 30s first tick would make
/// that untestable. Production cadence is unchanged.
#[cfg(not(test))]
const PLACEHOLDER_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
#[cfg(test)]
const PLACEHOLDER_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

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
    progress: Option<Arc<std::sync::atomic::AtomicU64>>,
) -> PlaceholderGuard {
    let heartbeat = {
        let pool = pool.clone();
        let hash = store_path_hash.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PLACEHOLDER_HEARTBEAT_INTERVAL);
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
                            h.load(std::sync::atomic::Ordering::Relaxed),
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
    use std::sync::atomic::{AtomicU64, Ordering};
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
        match claim_placeholder(pool, hash, path, &[], TEST_HOOKS)
            .await
            .expect("claim_placeholder")
        {
            PlaceholderClaim::Owned(c) => c,
            PlaceholderClaim::AlreadyComplete => panic!("unexpected AlreadyComplete"),
            PlaceholderClaim::Concurrent => panic!("unexpected Concurrent"),
        }
    }

    fn test_hash(tag: u8) -> Vec<u8> {
        vec![tag; 32]
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

        crate::cas::heartbeat_uploading_with_progress(&db.pool, &hash, claim, 100).await;
        let (_, _, fetched1, lp1, _, _, _) = row_state(&db.pool, &hash).await.expect("row");
        assert_eq!(fetched1, Some(100), "first heartbeat lands fetched_bytes");
        let lp1 = lp1.expect("first progress write sets last_progress_at");

        // Same value → liveness bumps, progress clock does NOT.
        tokio::time::sleep(Duration::from_millis(30)).await;
        crate::cas::heartbeat_uploading_with_progress(&db.pool, &hash, claim, 100).await;
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
        crate::cas::heartbeat_uploading_with_progress(&db.pool, &hash, claim, 200).await;
        let (_, _, fetched3, lp3, _, _, _) = row_state(&db.pool, &hash).await.expect("row");
        assert_eq!(fetched3, Some(200));
        assert!(
            lp3.expect("set") > lp1,
            "advancing byte count must advance last_progress_at"
        );

        // Claim guard: a stale owner's heartbeat is a no-op.
        crate::cas::heartbeat_uploading_with_progress(&db.pool, &hash, Uuid::new_v4(), 999).await;
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
        let handle = Arc::new(AtomicU64::new(0));
        let guard_sub = spawn_placeholder_guard(
            db.pool.clone(),
            hash_sub.clone(),
            claim_sub,
            Some(Arc::clone(&handle)),
        );
        handle.store(4096, Ordering::Relaxed);

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
}
