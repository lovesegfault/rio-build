//! Orphan scanner: reap stale 'uploading' manifests.
//!
//! If a worker crashes mid-PutPath (between insert_manifest_
//! placeholder and complete_manifest), the manifest stays in
//! `status='uploading'` forever. It's a GC root (mark.rs seeds
//! uploading manifests) so its narinfo is never swept, and a stale
//! placeholder blocks every re-upload of its path with `Aborted`
//! until someone deletes it.
//!
//! This scanner runs periodically (15min default), finds
//! 'uploading' manifests older than `STALE_THRESHOLD`, and removes
//! them via `reap_one`: a pure path-row janitor — the abandoned
//! placeholder rows are deleted and nothing else is touched. Chunks
//! the reaped manifest leaves unreferenced are collected by the next
//! chunk collect cycle (gc::collect) — the reaper never reads
//! `chunk_list`, never soft-deletes, and never enqueues.

use std::time::Duration;

use sqlx::PgPool;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// How old an 'uploading' manifest must be before we reap it.
///
/// Was 2h (tuned for rare PutPath crash recovery — longer than any
/// legitimate upload). Dropped to 15min because substitution made
/// stale placeholders a hot-path blocker, not just a GC leak: an
/// interrupted try_substitute leaves the placeholder, and subsequent
/// attempts return miss until this scanner reclaims it.
/// [`Substituter::ingest`](crate::substitute::Substituter) does its
/// own 5-minute reclaim on the hot path (see
/// `r[store.substitute.stale-reclaim+3]`); this sweep is the safety
/// net for placeholders nobody re-requests.
///
/// Safe against reaping live uploads: uploaders heartbeat
/// `updated_at` every 30s/64 chunks (see
/// [`heartbeat_uploading`](crate::cas::heartbeat_uploading)), so
/// `updated_at` reflects "last progress" not "insert time". A 30s
/// heartbeat against a 15min threshold gives 30× safety margin — a
/// live upload is never stale. Without heartbeat, a 6GB NAR over
/// 50Mbps (~16min) would be reaped mid-flight.
#[cfg(not(test))]
const STALE_THRESHOLD: Duration = Duration::from_secs(15 * 60);
#[cfg(test)]
const STALE_THRESHOLD: Duration = Duration::from_millis(100);

/// Scan interval. 15min: stale uploads accumulate slowly (only on
/// worker crashes), no need to scan aggressively.
#[cfg(not(test))]
const SCAN_INTERVAL: Duration = Duration::from_secs(15 * 60);
#[cfg(test)]
const SCAN_INTERVAL: Duration = Duration::from_millis(200);

/// Max stale-placeholder rows fetched per outer SELECT in [`scan_once`].
/// The loop re-SELECTs until short. Bounds memory after a mass-crash
/// backlog (drain.rs uses the same LIMIT-then-loop pattern).
#[cfg(not(test))]
const SCAN_BATCH_SIZE: i64 = 1000;
#[cfg(test)]
const SCAN_BATCH_SIZE: i64 = 2;

/// Selector for [`reap_one`]. Replaces the old `Option<i64>` threshold —
/// `None` ("this is mine, no stale check") was not an ownership check:
/// `manifests` is keyed by `store_path_hash` alone, so a late-firing
/// cleanup matched any fresh `'uploading'` row at the same hash.
/// Ownership is now unrepresentable-as-absent; every caller supplies
/// either a staleness gate or its claim token (M_052).
#[derive(Debug, Clone, Copy)]
pub(crate) enum ReapBy {
    /// Reap iff `updated_at < now() - secs`. Used by [`scan_once`] and
    /// the hot-path stale-reclaim. Never matches a heartbeating uploader.
    Stale { secs: i64 },
    /// Reap iff `claim_id = id`. Used by owner-side cleanup
    /// (`abort_placeholder`, the PutPath drop-guard, `put_chunked`'s
    /// complete-failure rollback). Never matches another uploader's
    /// fresh placeholder.
    Claim(Uuid),
}

/// Run one scan iteration. Returns count of orphans reaped.
///
/// For each stale uploading manifest, `reap_one` re-checks staleness
/// inside its own transaction and deletes the abandoned path rows
/// (narinfo CASCADE → manifests/manifest_data) — nothing else.
///
/// Same transaction semantics as sweep::sweep (the two are
/// structurally similar — orphan is "sweep for uploading-status"
/// with different selection criteria).
pub async fn scan_once(
    pool: &PgPool,
    clearance: &mut crate::gc::hold::HoldClearance,
) -> Result<(u64, u64), sqlx::Error> {
    // Find stale uploading manifests. SELECT hash only — the
    // staleness predicate is re-checked INSIDE reap_one's tx (see the
    // TOCTOU handling there). I-148: covered by the partial index
    // from migration 031 (predicate matches `WHERE status =
    // 'uploading'` exactly); without it this was a ~4s seq-scan over
    // ~1.5M rows per replica per interval.
    //
    // LIMIT + loop-until-short: bounds memory after a mass-crash
    // backlog (the unbounded fetch_all would otherwise materialize
    // every stale row at once).
    let threshold_secs = STALE_THRESHOLD.as_secs() as i64;
    let mut reaped = 0u64;
    let mut failed = 0u64;
    'scan: loop {
        let stale: Vec<(Vec<u8>,)> = sqlx::query_as(
            r#"
            SELECT m.store_path_hash
              FROM manifests m
             WHERE m.status = 'uploading'
               AND m.updated_at < now() - make_interval(secs => $1)
             LIMIT $2
            "#,
        )
        .bind(threshold_secs)
        .bind(SCAN_BATCH_SIZE)
        .fetch_all(pool)
        .await?;

        let n = stale.len();
        if n == 0 {
            if reaped == 0 && failed == 0 {
                debug!("orphan scan: no stale uploading manifests");
            }
            break;
        }

        let mut progressed = 0u64;
        for (store_path_hash,) in stale {
            // Batch-boundary re-authorization (merged_bug_067; per-ROW
            // since bug_084 — each reap_one is its own committed
            // transaction, i.e. its own batch, and the sink demands
            // the token by value): a hold landing mid-scan (or a
            // drain-bound-aged clearance) stops the next reap instead
            // of riding the tick-start consult through the whole
            // backlog. The candidate SELECT above runs unguarded —
            // reads are not destructive. A consult error fails closed
            // through the `?`.
            let authority = match clearance.authorize_batch(pool).await? {
                crate::gc::hold::BatchAuthorize::Authorized(a) => a,
                crate::gc::hold::BatchAuthorize::Held(h) => {
                    info!(
                        hold_id = %h.hold_id,
                        reaped,
                        "orphan scan: global hold landed mid-scan; \
                         stopping at the batch boundary"
                    );
                    break 'scan;
                }
                crate::gc::hold::BatchAuthorize::Expired => {
                    warn!(
                        reaped,
                        "orphan scan: clearance aged past the drain bound; \
                         stopping at the batch boundary (next tick re-gates)"
                    );
                    break 'scan;
                }
            };
            // Per-row isolation: a poison row (a row that makes its
            // own delete transaction fail, e.g. an FK surprise from
            // manual surgery) must not wedge the scanner. Log + count;
            // next 15min tick re-finds it (and the metric makes it
            // operator-visible).
            match reap_one(
                pool,
                &store_path_hash,
                ReapBy::Stale {
                    secs: threshold_secs,
                },
                authority,
            )
            .await
            {
                Ok(true) => {
                    reaped += 1;
                    progressed += 1;
                }
                // Row left the predicate independently (reaped by
                // another replica / no longer stale).
                Ok(false) => progressed += 1,
                Err(e) => {
                    warn!(
                        store_path_hash = %hex::encode(&store_path_hash),
                        error = %e,
                        "orphan scan: reap_one failed (will retry next interval)",
                    );
                    metrics::counter!("rio_store_gc_orphan_reap_failed_total").increment(1);
                    failed += 1;
                }
            }
        }

        // Exit on short read OR a full batch with zero forward
        // progress. With ≥SCAN_BATCH_SIZE poison rows the predicate
        // never shrinks (failed rows roll back, re-match next SELECT)
        // → without the progress check the loop livelocks and the
        // "next 15min tick" never fires. spawn_periodic re-finds
        // poison rows next interval after the operator addresses the
        // metric.
        if (n as i64) < SCAN_BATCH_SIZE || progressed == 0 {
            break;
        }
    }

    if reaped > 0 {
        info!(
            count = reaped,
            "orphan scan: reaped stale uploading manifests"
        );
    }
    Ok((reaped, failed))
}

/// Reap a single 'uploading' placeholder: delete the abandoned path
/// rows and nothing else.
///
/// `by`: [`ReapBy::Stale`] re-checks `updated_at < now() - secs` inside
/// the tx (TOCTOU guard against reaping a fresh re-upload).
/// [`ReapBy::Claim`] re-checks `claim_id = id` (owner-side cleanup —
/// see `r[store.put.placeholder-claim+2]`).
///
/// Returns `true` if reaped, `false` if no matching placeholder
/// (already gone, completed, fresher than threshold, or different
/// claim_id).
///
/// # History (I-040) and what reaping does today
///
/// Substitution's hot-path reclaim and its cleanup-on-failure both
/// previously called `crate::metadata::delete_manifest_uploading`
/// (the inline variant), which deletes `manifests` (CASCADE →
/// `manifest_data`) without touching chunk accounting; the counter
/// drift that left behind became a data-loss hazard while the upload
/// skip decision was inferred from the counter (the I-040/M_033
/// class). Two things have since removed that hazard at the root: the
/// upload skip decision is keyed on `uploaded_at` (never a liveness
/// signal), and chunk GC-eligibility is derived from the manifest
/// fold by the collect cycle. Reaping is therefore a pure path-row
/// janitor: it deletes the abandoned placeholder rows (FOR UPDATE +
/// EXISTS-guard, as before) — no `chunk_list` parse, no chunk-row
/// writes, no outbox enqueue. The chunks a reaped manifest leaves
/// unreferenced are collected by the next collect cycle. This is also
/// the claim-gated rollback `cas::put_chunked`/`cas::stage_chunked`
/// use on their failure paths.
///
/// Demands the reap's [`crate::gc::hold::BatchAuthority`] BY VALUE
/// (bug_084, R32): each reap is its own committed transaction — its
/// own batch — and this fn is the placeholder plane's DB-delete sink.
/// The wave-10 `_clearance: &HoldClearance` underscore-param was
/// advisory (a discarded proof, the merged_bug_006 shape one plane
/// over); the consumed token is not discardable.
// r[impl store.put.placeholder-claim+2]
// r[impl store.gc.hold-lanes+1]
// r[impl store.gc.batch-authority]
pub(crate) async fn reap_one(
    pool: &PgPool,
    store_path_hash: &[u8],
    by: ReapBy,
    authority: crate::gc::hold::BatchAuthority,
) -> Result<bool, sqlx::Error> {
    // The token is spent: one authority, one reap, this sink.
    authority.spend();
    let mut tx = pool.begin().await?;

    // Re-check the predicate INSIDE the tx with FOR UPDATE. Two races
    // this guards (preserved from the pre-extraction loop body):
    //
    // (1) Outer-SELECT vs inner-DELETE: scan_once's outer SELECT is
    // OUTSIDE any tx. Re-checking INSIDE the tx (with FOR UPDATE
    // locking the manifest row) guarantees the row we delete is the
    // row that matched.
    //
    // (2) Reap-then-reupload (ReapBy::Stale): store-0 + store-1 both
    // outer-SELECT the same stale hash; store-0 reaps; worker
    // re-uploads (NEW row, same hash, status='uploading',
    // updated_at=now()); store-1's FOR UPDATE would match the NEW row
    // (status is 'uploading' ✓) and reap a FRESH upload. Re-checking
    // the stale threshold inside the tx catches this — fresh
    // re-uploads have updated_at=now() → don't match. ReapBy::Claim
    // covers the analogous owner-side race (A's drop-guard fires
    // after B's fresh insert) via claim_id mismatch.
    //
    // The FOR UPDATE blocks any concurrent re-upload until this tx
    // commits — same pattern as sweep.rs.
    //
    // Two query strings (not a runtime-built one) so sqlx can prepare
    // both at compile time. The EXISTS-guard DELETE below mirrors the
    // same shape.
    let matched: Option<i32> = match by {
        ReapBy::Stale { secs } => {
            sqlx::query_scalar(
                r#"
            SELECT 1
              FROM manifests m
             WHERE m.store_path_hash = $1
               AND m.status = 'uploading'
               AND m.updated_at < now() - make_interval(secs => $2)
               FOR UPDATE OF m
            "#,
            )
            .bind(store_path_hash)
            .bind(secs)
            .fetch_optional(&mut *tx)
            .await?
        }
        ReapBy::Claim(id) => {
            sqlx::query_scalar(
                r#"
            SELECT 1
              FROM manifests m
             WHERE m.store_path_hash = $1
               AND m.status = 'uploading'
               AND m.claim_id = $2
               FOR UPDATE OF m
            "#,
            )
            .bind(store_path_hash)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
        }
    };
    if matched.is_none() {
        // Gone, completed, fresher than the threshold, or a different
        // claim — not ours to touch.
        tx.rollback().await?;
        return Ok(false);
    }

    // DELETE narinfo → CASCADE to manifests/manifest_data.
    //
    // Status (+ stale or claim) guards in EXISTS: atomic re-check at
    // DELETE time. rows_affected()==0 catches: (a) another replica
    // already reaped (gone), (b) upload completed since FOR UPDATE
    // (status='complete' → EXISTS false), (c) reap-then-reupload: a
    // FRESH 'uploading' row exists (updated_at recent / claim_id
    // different → EXISTS false).
    //
    // The FOR UPDATE above already re-checked status (+ stale/claim)
    // and locked the row — the EXISTS guard is defense-in-depth
    // against the predicate changing between the two statements.
    let deleted = match by {
        ReapBy::Stale { secs } => {
            sqlx::query(
                r#"
            DELETE FROM narinfo n
             WHERE n.store_path_hash = $1
               AND EXISTS (
                   SELECT 1 FROM manifests m
                    WHERE m.store_path_hash = $1
                      AND m.status = 'uploading'
                      AND m.updated_at < now() - make_interval(secs => $2)
               )
            "#,
            )
            .bind(store_path_hash)
            .bind(secs)
            .execute(&mut *tx)
            .await?
        }
        ReapBy::Claim(id) => {
            sqlx::query(
                r#"
            DELETE FROM narinfo n
             WHERE n.store_path_hash = $1
               AND EXISTS (
                   SELECT 1 FROM manifests m
                    WHERE m.store_path_hash = $1
                      AND m.status = 'uploading'
                      AND m.claim_id = $2
               )
            "#,
            )
            .bind(store_path_hash)
            .bind(id)
            .execute(&mut *tx)
            .await?
        }
    };
    if deleted.rows_affected() == 0 {
        // Gone or completed — either way, not an orphan anymore.
        // Rollback (no-op, nothing changed yet).
        tx.rollback().await?;
        return Ok(false);
    }

    // live_055(b): announce the reap to raced waiters — this is the
    // ONE delete chokepoint (abort_placeholder, the drop-guard, the
    // orphan scanner, and the hot-path stale reclaim all funnel
    // here). Inside the tx: PG delivers at COMMIT, so a woken
    // waiter's re-check sees the freed slot.
    crate::metadata::notify_placeholder_event(&mut *tx, store_path_hash).await?;

    tx.commit().await?;
    Ok(true)
}

// r[impl store.gc.hold-lanes+1]
/// The DEMAND-DRIVEN face of [`reap_one`] (merged_bug_050): the
/// non-periodic callers — `abort_placeholder`, the PutPath
/// drop-guard, the hot-path stale-reclaim, and the cas rollback
/// paths — consult the hold gate at call time and SKIP the reap
/// while a global hold is active (or the consult fails — fail
/// closed). `Ok(false)` is the honest "not reaped": the placeholder
/// ages and the (also held-suspended) scanner reaps it after the
/// hold releases — the recorded fallback. During an incident freeze
/// a placeholder row is evidence of an attempted upload; demand
/// callers must not destroy it any more than the periodic lanes.
pub(crate) async fn reap_one_consulted(
    pool: &PgPool,
    store_path_hash: &[u8],
    by: ReapBy,
) -> Result<bool, sqlx::Error> {
    match crate::gc::hold::gate(pool).await {
        Ok(crate::gc::hold::HoldGate::Clear(mut clearance)) => {
            // The demand-driven reap is one batch: mint its token at
            // the fresh consult (bug_084 — the sink demands it by
            // value). Held/Expired here mirror the gate arms below;
            // Expired on a just-minted clearance is unreachable but
            // refuses honestly rather than panicking (closed alphabet,
            // no wildcard).
            match clearance.authorize_batch(pool).await? {
                crate::gc::hold::BatchAuthorize::Authorized(authority) => {
                    reap_one(pool, store_path_hash, by, authority).await
                }
                crate::gc::hold::BatchAuthorize::Held(h) => {
                    debug!(
                        store_path_hash = %hex::encode(store_path_hash),
                        hold_id = %h.hold_id,
                        "placeholder reap skipped: global hold landed at the \
                         batch boundary (scanner reaps after release)"
                    );
                    metrics::counter!(
                        "rio_store_gc_hold_lane_skips_total",
                        "lane" => "claim-reap", "cause" => "held"
                    )
                    .increment(1);
                    Ok(false)
                }
                crate::gc::hold::BatchAuthorize::Expired => {
                    warn!(
                        store_path_hash = %hex::encode(store_path_hash),
                        "placeholder reap skipped: clearance expired at mint \
                         (cannot happen on a fresh consult; refusing closed)"
                    );
                    metrics::counter!(
                        "rio_store_gc_hold_lane_skips_total",
                        "lane" => "claim-reap", "cause" => "consult_error"
                    )
                    .increment(1);
                    Ok(false)
                }
            }
        }
        Ok(crate::gc::hold::HoldGate::Held(h)) => {
            debug!(
                store_path_hash = %hex::encode(store_path_hash),
                hold_id = %h.hold_id,
                "placeholder reap skipped: active global gc hold \
                 (scanner reaps after release)"
            );
            metrics::counter!(
                "rio_store_gc_hold_lane_skips_total",
                "lane" => "claim-reap", "cause" => "held"
            )
            .increment(1);
            Ok(false)
        }
        Err(e) => {
            warn!(
                store_path_hash = %hex::encode(store_path_hash),
                error = %e,
                "placeholder reap: hold consult failed; skipping (fail closed)"
            );
            metrics::counter!(
                "rio_store_gc_hold_lane_skips_total",
                "lane" => "claim-reap", "cause" => "consult_error"
            )
            .increment(1);
            Ok(false)
        }
    }
}

/// Spawn the periodic orphan scanner. Runs `scan_once` every
/// SCAN_INTERVAL. Errors logged; next iteration retries. Exits
/// cleanly when `shutdown` is cancelled.
///
/// `spawn_periodic` sets `MissedTickBehavior::Skip`: if one scan is
/// slow (large orphan backlog), don't fire twice immediately.
/// Interval drifts; fine for a 15min background task.
pub fn spawn_scanner(
    pool: PgPool,
    shutdown: rio_common::signal::Token,
) -> tokio::task::JoinHandle<()> {
    // r[impl store.gc.hold-lanes+1]
    // Registered through DestructiveLane (merged_bug_050): every
    // tick consults the gc-hold gate fail-closed before reaping —
    // reaping during an incident freeze destroys evidence of
    // attempted uploads. Hold-suspension preserves the scanner's
    // eventual-fallback role OUTSIDE holds.
    let lane_pool = pool.clone();
    crate::gc::lane::DestructiveLane::spawn_periodic(
        "gc-orphan-scanner",
        SCAN_INTERVAL,
        pool,
        shutdown,
        Box::new(move |clearance| {
            let pool = lane_pool.clone();
            Box::pin(async move {
                if let Err(e) = scan_once(&pool, clearance).await {
                    warn!(error = %e, "orphan scan failed (will retry next interval)");
                }
            })
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Manifest, ManifestEntry};
    use rio_test_support::TestDb;

    /// Seed an 'uploading' placeholder, upgrade to chunked (chunk row
    /// and manifest_data written), backdate. Simulates: prior
    /// `cas::put_chunked` crashed AFTER `upgrade_manifest_to_chunked`.
    ///
    /// Returns (chunk_hash, claim_id) so callers can check the chunk
    /// row state and exercise `ReapBy::Claim`.
    async fn seed_stale_chunked(pool: &PgPool, hash: &[u8], path: &str) -> ([u8; 32], Uuid) {
        let claim = crate::metadata::insert_manifest_uploading(pool, hash, path, &[])
            .await
            .unwrap()
            .expect("fresh path → placeholder inserted");
        // One-chunk manifest, built via the real serializer.
        let chunk_hash = [hash[0]; 32]; // distinct per test via the path-hash byte
        let chunk_list = Manifest {
            entries: vec![ManifestEntry {
                hash: chunk_hash,
                size: 100,
            }],
        }
        .serialize();
        let _ = crate::metadata::upgrade_manifest_to_chunked(
            pool,
            hash,
            &chunk_list,
            &[chunk_hash.to_vec()],
            &[100i64],
        )
        .await
        .unwrap();
        sqlx::query(
            "UPDATE manifests SET updated_at = now() - interval '1 hour' \
             WHERE store_path_hash = $1",
        )
        .bind(hash)
        .execute(pool)
        .await
        .unwrap();
        (chunk_hash, claim)
    }

    // r[verify store.substitute.stale-reclaim+3]
    /// `reap_one` on a CHUNKED placeholder is a pure path-row janitor:
    /// the placeholder rows go (narinfo CASCADE → manifests /
    /// manifest_data) and the chunk rows are left exactly as they
    /// were — no soft-delete, no outbox row. The chunks the reaped
    /// manifest leaves unreferenced are the next collect cycle's
    /// ordinary victims (covered by the gc::collect live-cycle tests).
    #[tokio::test]
    async fn reap_one_chunked_is_path_row_janitor() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let hash = vec![0x40u8; 32];
        let path = rio_test_support::fixtures::test_store_path("i040-reap-chunked");
        let (chunk_hash, claim) = seed_stale_chunked(&db.pool, &hash, &path).await;

        // Verify setup: chunk row + manifest_data exist.
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE blake3_hash = $1")
            .bind(chunk_hash.as_slice())
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(n, 1, "setup: chunk row exists");

        // reap_one (owner-side — by claim).
        let reaped = reap_one(
            &db.pool,
            &hash,
            ReapBy::Claim(claim),
            crate::test_helpers::gc_batch_authority(&db.pool).await,
        )
        .await
        .unwrap();
        assert!(reaped, "chunked placeholder reaped");

        // Placeholder gone (narinfo CASCADE → manifests/manifest_data).
        let n: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM manifest_data WHERE store_path_hash = $1")
                .bind(&hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(n.0, 0, "manifest_data gone via CASCADE");

        // Chunk row untouched: present, not soft-deleted, no outbox row.
        let (del, outbox): (bool, i64) = sqlx::query_as(
            "SELECT c.deleted, \
                    (SELECT COUNT(*) FROM pending_s3_deletes p WHERE p.blake3_hash = c.blake3_hash) \
             FROM chunks c WHERE c.blake3_hash = $1",
        )
        .bind(chunk_hash.as_slice())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(!del, "reap never soft-deletes chunk rows");
        assert_eq!(outbox, 0, "reap never enqueues outbox rows");
    }

    /// `scan_once`'s loop delegates to `reap_one` — chunked
    /// placeholders found by the periodic scanner are also handled as
    /// path rows only.
    #[tokio::test]
    async fn scan_once_chunked_reaps_path_rows_only() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let hash = vec![0x41u8; 32];
        let path = rio_test_support::fixtures::test_store_path("i040-scan-chunked");
        let (chunk_hash, _) = seed_stale_chunked(&db.pool, &hash, &path).await;

        let (reaped, failed) = scan_once(
            &db.pool,
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
        )
        .await
        .unwrap();
        assert_eq!(reaped, 1);
        assert_eq!(failed, 0);

        let (del, n_md): (bool, i64) = sqlx::query_as(
            "SELECT c.deleted, \
                    (SELECT COUNT(*) FROM manifest_data WHERE store_path_hash = $1) \
             FROM chunks c WHERE c.blake3_hash = $2",
        )
        .bind(&hash)
        .bind(chunk_hash.as_slice())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(!del, "scanner reap leaves the chunk row untouched");
        assert_eq!(n_md, 0, "scanner reap deleted the path rows");
    }

    /// `reap_one(ReapBy::Stale, <batch authority>)` skips a fresh
    /// placeholder. Same guard scan_once relied on; pinned here so a
    /// future direct caller (substitute) gets the same protection.
    #[tokio::test]
    async fn reap_one_thresholded_skips_fresh_chunked() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let hash = vec![0x42u8; 32];
        let path = rio_test_support::fixtures::test_store_path("i040-fresh-chunked");
        let (chunk_hash, _) = seed_stale_chunked(&db.pool, &hash, &path).await;
        // Re-freshen: undo the backdate from the seed helper.
        sqlx::query(
            "UPDATE manifests SET updated_at = now() + interval '10 seconds' \
             WHERE store_path_hash = $1",
        )
        .bind(&hash)
        .execute(&db.pool)
        .await
        .unwrap();

        // 5min threshold → fresh placeholder NOT reaped.
        let reaped = reap_one(
            &db.pool,
            &hash,
            ReapBy::Stale { secs: 300 },
            crate::test_helpers::gc_batch_authority(&db.pool).await,
        )
        .await
        .unwrap();
        assert!(!reaped, "fresh placeholder skipped under threshold");

        // The fresh upload's rows are all still there.
        let (n_md, n_chunk): (i64, i64) = sqlx::query_as(
            "SELECT \
               (SELECT COUNT(*) FROM manifest_data WHERE store_path_hash = $1), \
               (SELECT COUNT(*) FROM chunks WHERE blake3_hash = $2)",
        )
        .bind(&hash)
        .bind(chunk_hash.as_slice())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            n_md, 1,
            "fresh chunked placeholder's manifest_data untouched"
        );
        assert_eq!(
            n_chunk, 1,
            "fresh chunked placeholder's chunk row untouched"
        );
    }

    // r[verify store.put.placeholder-claim+2]
    /// bug_235: `ReapBy::Claim(a)` MUST NOT match a fresh placeholder
    /// inserted with claim_b at the same hash. Before M_052 the
    /// equivalent (a threshold-less reap) filtered
    /// `status='uploading'` only — A's late drop-guard reaped B's
    /// in-flight placeholder (and with it the manifest_data that keeps
    /// B's chunks out of the collect cycle's eligible set).
    #[tokio::test]
    async fn reap_one_claim_mismatch_is_noop() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let hash = vec![0x43u8; 32];
        let path = rio_test_support::fixtures::test_store_path("claim-mismatch");

        // A inserts + is reaped by the orphan scanner (its row is GONE).
        let (_, claim_a) = seed_stale_chunked(&db.pool, &hash, &path).await;
        let (reaped, _) = scan_once(
            &db.pool,
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
        )
        .await
        .unwrap();
        assert_eq!(reaped, 1, "scanner reaped A's stale placeholder");

        // B inserts a FRESH placeholder at the same hash.
        let (_, claim_b) = seed_stale_chunked(&db.pool, &hash, &path).await;
        assert_ne!(claim_a, claim_b);

        // A's late drop-guard fires with claim_a → MUST NOT match B.
        let reaped = reap_one(
            &db.pool,
            &hash,
            ReapBy::Claim(claim_a),
            crate::test_helpers::gc_batch_authority(&db.pool).await,
        )
        .await
        .unwrap();
        assert!(!reaped, "claim_a mismatch → no-op (B's row protected)");

        // B's narinfo + manifests + manifest_data intact.
        let (n_m, n_md): (i64, i64) = sqlx::query_as(
            "SELECT \
               (SELECT COUNT(*) FROM manifests WHERE store_path_hash = $1), \
               (SELECT COUNT(*) FROM manifest_data WHERE store_path_hash = $1)",
        )
        .bind(&hash)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(n_m, 1, "B's manifests row survives");
        assert_eq!(n_md, 1, "B's manifest_data row survives");

        // B's own claim DOES match.
        let reaped = reap_one(
            &db.pool,
            &hash,
            ReapBy::Claim(claim_b),
            crate::test_helpers::gc_batch_authority(&db.pool).await,
        )
        .await
        .unwrap();
        assert!(reaped, "claim_b matches → B's placeholder reaped");
    }

    /// The reap is independent of `chunk_list` contents: a corrupt
    /// blob on a stale placeholder neither blocks the reap nor causes
    /// any chunk-row write (the reap never parses it — corrupt-input
    /// handling lives entirely with the collect cycle's fail-closed
    /// mark, which aborts on the same blob instead).
    #[tokio::test]
    async fn reap_one_ignores_corrupt_chunk_list() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let hash = vec![0x47u8; 32];
        let path = rio_test_support::fixtures::test_store_path("corrupt-reap");
        let (chunk_hash, claim) = seed_stale_chunked(&db.pool, &hash, &path).await;
        // Corrupt the chunk_list AFTER the upgrade wrote it.
        sqlx::query("UPDATE manifest_data SET chunk_list = $2 WHERE store_path_hash = $1")
            .bind(&hash)
            .bind(vec![0xFFu8, 0x00, 0x01])
            .execute(&db.pool)
            .await
            .unwrap();

        let reaped = reap_one(
            &db.pool,
            &hash,
            ReapBy::Claim(claim),
            crate::test_helpers::gc_batch_authority(&db.pool).await,
        )
        .await
        .unwrap();
        assert!(reaped, "corrupt chunk_list does not block the reap");

        // The placeholder rows are gone; the chunk row is untouched.
        let manifest_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM manifest_data WHERE store_path_hash = $1")
                .bind(&hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(manifest_rows, 0);
        let deleted: bool = sqlx::query_scalar("SELECT deleted FROM chunks WHERE blake3_hash = $1")
            .bind(chunk_hash.as_slice())
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert!(!deleted, "the reap path never soft-deletes");
    }

    /// Helper: insert an 'uploading' placeholder AND backdate
    /// updated_at so the stale-threshold check deterministically
    /// matches (test STALE_THRESHOLD.as_secs()==0 means the query
    /// needs updated_at < now(), which is fragile if set to now()
    /// in the same statement — backdating avoids the race).
    async fn seed_stale_uploading(pool: &PgPool, hash: &[u8], path: &str) -> uuid::Uuid {
        let claim = crate::metadata::insert_manifest_uploading(pool, hash, path, &[])
            .await
            .unwrap()
            .unwrap();
        sqlx::query(
            "UPDATE manifests SET updated_at = now() - interval '1 hour' \
             WHERE store_path_hash = $1",
        )
        .bind(hash)
        .execute(pool)
        .await
        .unwrap();
        claim
    }

    #[tokio::test]
    async fn orphan_reaps_stale_uploading() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let hash = vec![0x01u8; 32];
        let path = rio_test_support::fixtures::test_store_path("orphan-stale");
        seed_stale_uploading(&db.pool, &hash, &path).await;

        let (reaped, _) = scan_once(
            &db.pool,
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
        )
        .await
        .unwrap();
        assert_eq!(reaped, 1, "stale uploading manifest reaped");

        // narinfo gone (CASCADE took manifests too).
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM narinfo WHERE store_path_hash = $1")
                .bind(&hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(count.0, 0, "narinfo deleted");
    }

    /// TOCTOU regression: upload completes between scan's SELECT
    /// and DELETE. The status guard in the DELETE's WHERE must
    /// catch this and SKIP the delete.
    #[tokio::test]
    async fn orphan_skips_completed_upload_toctou() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed stale uploading placeholder — it WOULD be reaped.
        let hash = vec![0x02u8; 32];
        let path = rio_test_support::fixtures::test_store_path("orphan-raced");
        seed_stale_uploading(&db.pool, &hash, &path).await;

        // Simulate upload completing BETWEEN the outer SELECT and
        // the per-path DELETE. In the real race, this happens while
        // scan_once is iterating. Here we flip status directly
        // before calling scan_once — the DELETE's WHERE EXISTS
        // (status='uploading') should see status='complete' → no
        // match → rows_affected()==0 → skipped.
        //
        // Also set nar_size>0 so it's clearly a real completed path
        // (not that the DELETE checks this — the status guard is
        // what matters).
        sqlx::query("UPDATE manifests SET status = 'complete' WHERE store_path_hash = $1")
            .bind(&hash)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE narinfo SET nar_size = 42 WHERE store_path_hash = $1")
            .bind(&hash)
            .execute(&db.pool)
            .await
            .unwrap();

        // Key point: the outer SELECT in scan_once filters on
        // status='uploading' + updated_at. Since we already flipped
        // status, scan_once's SELECT won't even find this hash. To
        // test the DELETE guard SPECIFICALLY (not the SELECT), we
        // need the SELECT to find it but the DELETE to skip it.
        //
        // We can't easily interleave with scan_once's internal loop
        // from a unit test. Instead, we assert the INVARIANT
        // directly: run the same DELETE query with status+stale
        // guard, verify rows_affected==0.
        let deleted = sqlx::query(
            r#"
            DELETE FROM narinfo n
             WHERE n.store_path_hash = $1
               AND EXISTS (
                   SELECT 1 FROM manifests m
                    WHERE m.store_path_hash = $1
                      AND m.status = 'uploading'
                      AND m.updated_at < now() - make_interval(secs => $2)
               )
            "#,
        )
        .bind(&hash)
        .bind(0i64)
        .execute(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            deleted.rows_affected(),
            0,
            "status guard prevented delete of completed upload"
        );

        // narinfo still present.
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM narinfo WHERE store_path_hash = $1")
                .bind(&hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(count.0, 1, "completed upload NOT deleted by status guard");

        // And scan_once itself finds nothing (status already
        // complete → SELECT filters it out).
        let (reaped, _) = scan_once(
            &db.pool,
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
        )
        .await
        .unwrap();
        assert_eq!(reaped, 0, "scan_once found nothing (status=complete)");
    }

    #[tokio::test]
    async fn orphan_skips_fresh_uploading() {
        // Fresh (not stale) upload in progress → NOT reaped.
        let db = TestDb::new(&crate::MIGRATOR).await;

        let hash = vec![0x03u8; 32];
        let path = rio_test_support::fixtures::test_store_path("orphan-fresh");
        // Insert WITHOUT backdating — updated_at = now(). With test
        // STALE_THRESHOLD.as_secs()==0, query is `updated_at < now()`.
        // Set updated_at slightly in the future to guarantee NOT stale.
        crate::metadata::insert_manifest_uploading(&db.pool, &hash, &path, &[])
            .await
            .unwrap();
        sqlx::query(
            "UPDATE manifests SET updated_at = now() + interval '10 seconds' \
             WHERE store_path_hash = $1",
        )
        .bind(&hash)
        .execute(&db.pool)
        .await
        .unwrap();

        let (reaped, _) = scan_once(
            &db.pool,
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
        )
        .await
        .unwrap();
        assert_eq!(reaped, 0, "fresh upload not reaped");

        // narinfo still present.
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM narinfo WHERE store_path_hash = $1")
                .bind(&hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(count.0, 1, "fresh upload narinfo preserved");
    }

    /// Reap-then-reupload race: store-0 + store-1 both outer-SELECT
    /// the same stale hash. store-0 reaps it. Worker re-uploads (NEW
    /// row, same hash, status='uploading', updated_at fresh). store-1's
    /// inner FOR UPDATE + DELETE must NOT reap the fresh re-upload.
    ///
    /// We can't race two scan_once calls; instead we simulate
    /// store-1's inner-loop state: we already HAVE the hash from
    /// a stale outer SELECT, but the DB now has a FRESH re-upload
    /// at that hash. Running the inner queries directly (FOR UPDATE
    /// + DELETE with the stale-threshold re-check) must return 0.
    #[tokio::test]
    async fn orphan_skips_fresh_reupload_after_another_replicas_reap() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash = vec![0x04u8; 32];
        let path = rio_test_support::fixtures::test_store_path("reap-reupload-race");

        // --- store-0's turn: seed stale + reap ---
        seed_stale_uploading(&db.pool, &hash, &path).await;
        let (reaped, _) = scan_once(
            &db.pool,
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
        )
        .await
        .unwrap();
        assert_eq!(reaped, 1, "store-0 reaped the stale upload");

        // --- Worker re-uploads same path (FRESH placeholder) ---
        // updated_at = now() + 10s guarantees NOT stale under the
        // test threshold (0s → `updated_at < now()`).
        crate::metadata::insert_manifest_uploading(&db.pool, &hash, &path, &[])
            .await
            .unwrap();
        sqlx::query(
            "UPDATE manifests SET updated_at = now() + interval '10 seconds' \
             WHERE store_path_hash = $1",
        )
        .bind(&hash)
        .execute(&db.pool)
        .await
        .unwrap();

        // --- store-1's inner loop: has the hash from its OWN outer
        // SELECT (which ran BEFORE store-0's reap, saw stale). ---
        //
        // Run the FOR UPDATE query directly. With the stale-threshold
        // re-check, it should return None (fresh re-upload has
        // updated_at > now()-threshold → doesn't match). The query
        // mirrors production reap_one's ownership re-check shape.
        let mut tx = db.pool.begin().await.unwrap();
        let matched: Option<Vec<u8>> = sqlx::query_scalar(
            r#"
            SELECT m.store_path_hash
              FROM manifests m
             WHERE m.store_path_hash = $1
               AND m.status = 'uploading'
               AND m.updated_at < now() - make_interval(secs => $2)
               FOR UPDATE OF m
            "#,
        )
        .bind(&hash)
        .bind(0i64)
        .fetch_optional(&mut *tx)
        .await
        .unwrap();
        assert!(
            matched.is_none(),
            "FOR UPDATE must NOT match fresh re-upload (stale threshold re-check)"
        );

        // The DELETE should also skip (EXISTS with stale clause
        // → false for fresh row).
        let deleted = sqlx::query(
            r#"
            DELETE FROM narinfo n
             WHERE n.store_path_hash = $1
               AND EXISTS (
                   SELECT 1 FROM manifests m
                    WHERE m.store_path_hash = $1
                      AND m.status = 'uploading'
                      AND m.updated_at < now() - make_interval(secs => $2)
               )
            "#,
        )
        .bind(&hash)
        .bind(0i64)
        .execute(&mut *tx)
        .await
        .unwrap();
        assert_eq!(
            deleted.rows_affected(),
            0,
            "DELETE must NOT reap fresh re-upload"
        );
        tx.rollback().await.unwrap();

        // Fresh re-upload's narinfo still present.
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM narinfo WHERE store_path_hash = $1")
                .bind(&hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(count.0, 1, "fresh re-upload narinfo survived");
    }

    /// Heartbeat bumps updated_at so the scanner skips a live upload
    /// even when its original insert time is stale.
    ///
    /// Positive: stale placeholder + heartbeat → scan_once reaps 0.
    /// Negative: stale placeholder + NO heartbeat → scan_once reaps 1.
    // r[verify store.gc.orphan-heartbeat]
    #[tokio::test]
    async fn orphan_heartbeat_protects_live_upload() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // --- Positive: heartbeat rescues a stale placeholder ---
        let live_hash = vec![0x05u8; 32];
        let live_path = rio_test_support::fixtures::test_store_path("orphan-heartbeat-live");
        let claim = seed_stale_uploading(&db.pool, &live_hash, &live_path).await;

        // Before heartbeat: updated_at is 1h in the past (from
        // seed_stale_uploading's backdate).
        let before: (sqlx::postgres::types::PgInterval,) =
            sqlx::query_as("SELECT now() - updated_at FROM manifests WHERE store_path_hash = $1")
                .bind(&live_hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(
            before.0.microseconds > 60 * 1_000_000,
            "pre-heartbeat updated_at should be >1min old (seeded 1h back)"
        );

        // r[verify store.put.placeholder-claim+2]
        // Foreign-claim heartbeat is a no-op: a stale uploader whose
        // row was reaped must NOT refresh a fresh re-uploader's
        // updated_at. Fire with a wrong claim first; assert age
        // unchanged.
        crate::cas::heartbeat_uploading(&db.pool, &live_hash, uuid::Uuid::new_v4()).await;
        let after_foreign: (sqlx::postgres::types::PgInterval,) =
            sqlx::query_as("SELECT now() - updated_at FROM manifests WHERE store_path_hash = $1")
                .bind(&live_hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(
            after_foreign.0.microseconds > 60 * 1_000_000,
            "foreign-claim heartbeat must NOT refresh updated_at; got {}µs",
            after_foreign.0.microseconds
        );

        // Heartbeat with the real claim.
        crate::cas::heartbeat_uploading(&db.pool, &live_hash, claim).await;

        // After heartbeat: updated_at is fresh (< 1min old).
        let after: (sqlx::postgres::types::PgInterval,) =
            sqlx::query_as("SELECT now() - updated_at FROM manifests WHERE store_path_hash = $1")
                .bind(&live_hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(
            after.0.microseconds < 60 * 1_000_000,
            "post-heartbeat updated_at should be <1min old, got {}µs",
            after.0.microseconds
        );

        // Bump slightly into the future to defeat test STALE_THRESHOLD
        // (0s → query is `updated_at < now()` which a just-heartbeated
        // row would still match under sub-second clock jitter). This
        // mirrors what orphan_skips_fresh_uploading does.
        sqlx::query(
            "UPDATE manifests SET updated_at = now() + interval '10 seconds' \
             WHERE store_path_hash = $1",
        )
        .bind(&live_hash)
        .execute(&db.pool)
        .await
        .unwrap();

        // --- Negative: stale placeholder WITHOUT heartbeat ---
        let dead_hash = vec![0x06u8; 32];
        let dead_path = rio_test_support::fixtures::test_store_path("orphan-heartbeat-dead");
        seed_stale_uploading(&db.pool, &dead_hash, &dead_path).await;
        // No heartbeat.

        // Scan: dead reaped, live skipped.
        let (reaped, _) = scan_once(
            &db.pool,
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
        )
        .await
        .unwrap();
        assert_eq!(reaped, 1, "exactly the non-heartbeated placeholder reaped");

        // live still present; dead gone.
        let live_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM narinfo WHERE store_path_hash = $1")
                .bind(&live_hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(live_count.0, 1, "heartbeated upload survived scan");

        let dead_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM narinfo WHERE store_path_hash = $1")
                .bind(&dead_hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(dead_count.0, 0, "non-heartbeated upload reaped");
    }

    /// I-148 dev-only sanity: the `scan_once` SELECT must use the
    /// partial index from migration 031, not seq-scan manifests.
    ///
    /// `#[ignore]` because EXPLAIN output depends on PG's cost model;
    /// in practice the partial index is tiny enough (predicate matches
    /// <100 rows at steady state) that PG picks it even on a small
    /// test DB, but this isn't a CI gate. Run locally with
    /// `cargo test -p rio-store -- --ignored scan_query_uses`.
    #[ignore = "EXPLAIN plan depends on PG cost model; dev-only sanity"]
    #[tokio::test]
    async fn scan_query_uses_uploading_partial_idx() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed enough rows that PG's cost model wouldn't pick the
        // index for a generic scan — proves the PARTIAL predicate is
        // what makes it cheap. All 'complete' (don't match the index
        // predicate) bar a handful 'uploading'. narinfo first to
        // satisfy the manifests FK.
        sqlx::query(
            "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size)
             SELECT sha256(i::text::bytea),
                    '/nix/store/' || lpad(to_hex(i), 32, '0') || '-seed',
                    sha256(i::text::bytea), 0
             FROM generate_series(1, 2000) AS i",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO manifests (store_path_hash, status, updated_at)
             SELECT sha256(i::text::bytea),
                    CASE WHEN i <= 5 THEN 'uploading' ELSE 'complete' END,
                    now() - i * interval '1 second'
             FROM generate_series(1, 2000) AS i",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE manifests")
            .execute(&db.pool)
            .await
            .unwrap();

        // Mirror scan_once's query exactly (predicate must match the
        // partial index's WHERE for PG to use it).
        let plan: Vec<(String,)> = sqlx::query_as(
            "EXPLAIN (FORMAT TEXT)
             SELECT m.store_path_hash
               FROM manifests m
              WHERE m.status = 'uploading'
                AND m.updated_at < now() - make_interval(secs => $1)",
        )
        .bind(STALE_THRESHOLD.as_secs() as i64)
        .fetch_all(&db.pool)
        .await
        .unwrap();

        let joined: String = plan
            .into_iter()
            .map(|(l,)| l)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("idx_manifests_uploading_updated_at"),
            "EXPLAIN plan should reference idx_manifests_uploading_updated_at; got:\n{joined}"
        );
        assert!(
            !joined.contains("Seq Scan on manifests"),
            "EXPLAIN plan should NOT seq-scan manifests; got:\n{joined}"
        );
    }
}
