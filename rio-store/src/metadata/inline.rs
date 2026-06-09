//! Inline write-ahead: small NARs stored directly in `manifests.inline_blob`.
//!
//! Step 1 (`insert_manifest_uploading`) writes a placeholder with
//! `nar_size = 0` + `status = 'uploading'`. Step 3 (`complete_manifest_inline`)
//! fills real narinfo and stores the NAR blob atomically. On failure,
// r[impl store.inline.threshold]
//! `delete_manifest_uploading` reclaims the placeholder (guarded by
//! `nar_size = 0` so a concurrent successful upload is never touched).

use super::*;
use sqlx::PgPool;
use tracing::{debug, instrument};

/// Begin a new upload: insert placeholder narinfo + manifest rows.
///
/// The placeholder narinfo has `nar_hash = [0;32]` and `nar_size = 0`.
/// `nar_size = 0` is the placeholder marker: the minimum valid NAR is ~100
/// bytes, so 0 unambiguously means "not a real upload yet". This lets
/// `delete_manifest_uploading` identify placeholders without touching a
/// concurrent successful upload of the same path.
///
/// `references` is populated on the placeholder so the closure is protected
/// from GC at the instant this tx commits — no advisory lock needed
/// (I-192). Mark's CTE may or may not see this row depending on snapshot
/// timing; either way the references reach sweep:
///
/// - Placeholder commits BEFORE mark's CTE snapshot → seed (b) walks it.
/// - Placeholder commits AFTER mark's CTE snapshot → sweep's per-path
///   re-check (`narinfo."references" @> ARRAY[Q]`, fresh READ-COMMITTED
///   snapshot, scans `'uploading'` rows too) finds it and resurrects Q.
///
/// See `r[store.gc.sweep-recheck+2]` for the full race trace.
///
/// Returns `Some(claim_id)` if inserted (the caller now OWNS the
/// placeholder and uses `claim_id` for its cleanup paths — see
/// `r[store.put.placeholder-claim+2]`), `None` if another upload already
/// holds a placeholder (caller should re-check `check_manifest_complete`
/// — the race winner may have finished).
// r[impl store.put.placeholder-claim+2]
/// Test-only convenience wrapper (production callers go through
/// `ingest::claim_placeholder`, which threads `claimed_by`).
#[cfg(test)]
pub(crate) async fn insert_manifest_uploading(
    pool: &PgPool,
    store_path_hash: &[u8],
    store_path: &str,
    references: &[String],
) -> Result<Option<uuid::Uuid>> {
    insert_manifest_uploading_as(pool, store_path_hash, store_path, references, None).await
}

/// `insert_manifest_uploading` with owner attribution: `claimed_by`
/// (the substituting pod's name) is stamped on the placeholder row for
/// operator-side stall/takeover diagnosis. Substitution claims pass
/// `Some(pod)`; PutPath and every legacy caller pass `None` via the
/// plain wrapper (claimed_by stays NULL — the design sets it only on
/// substitution-claimed placeholders).
#[instrument(skip(pool, references), fields(store_path_hash = hex::encode(store_path_hash), refs = references.len()))]
pub(crate) async fn insert_manifest_uploading_as(
    pool: &PgPool,
    store_path_hash: &[u8],
    store_path: &str,
    references: &[String],
    claimed_by: Option<&str>,
) -> Result<Option<uuid::Uuid>> {
    let mut tx = pool.begin().await?;

    // narinfo placeholder first (manifests has FK to narinfo). ON CONFLICT
    // DO NOTHING: if another uploader already inserted, we don't clobber.
    // REFERENCES POPULATED HERE — this is what makes the placeholder itself
    // protect its closure (via mark seed (b) or sweep re-check) without an
    // advisory lock.
    // r[impl store.put.placeholder-refs]
    sqlx::query(
        r#"
        INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size,
                             "references")
        VALUES ($1, $2, $3, 0, $4)
        ON CONFLICT (store_path_hash) DO NOTHING
        "#,
    )
    .bind(store_path_hash)
    .bind(store_path)
    .bind(&[0u8; 32] as &[u8])
    .bind(references)
    .execute(&mut *tx)
    .await?;

    // manifests placeholder. ON CONFLICT DO NOTHING for the same reason.
    // rows_affected = 0 means another uploader owns this slot. claim_id
    // is the ownership token: every owner-side mutation (heartbeat,
    // completion, abort_placeholder, the drop-guard, put_chunked's
    // complete-failure rollback) filters on it so a late-firing op
    // cannot match a fresh re-upload at the same store_path_hash.
    let claim_id = uuid::Uuid::new_v4();
    let result = sqlx::query(
        r#"
        INSERT INTO manifests (store_path_hash, status, claim_id, claimed_by, claim_phase)
        VALUES ($1, 'uploading', $2, $3,
                CASE WHEN $3::text IS NOT NULL THEN 'downloading' END)
        ON CONFLICT (store_path_hash) DO NOTHING
        "#,
    )
    .bind(store_path_hash)
    .bind(claim_id)
    .bind(claimed_by)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok((result.rows_affected() > 0).then_some(claim_id))
}

/// Claim a **released-in-place** `'uploading'` placeholder: a row the
/// owner-side stall abort left behind (`claim_id IS NULL`,
/// `r[store.substitute.stall-abort]`). Claimable immediately by ANY
/// caller — no staleness threshold — with `stall_count` PRESERVED
/// (the whole point of releasing in place instead of deleting: stall
/// evidence survives the handoff). Returns the new claim token, or
/// `None` if no released row exists (live claim, completed, or gone).
// r[impl store.substitute.stale-reclaim+3]
#[instrument(skip(pool), fields(store_path_hash = hex::encode(store_path_hash)))]
pub(crate) async fn claim_released_placeholder(
    pool: &PgPool,
    store_path_hash: &[u8],
    claimed_by: Option<&str>,
) -> Result<Option<uuid::Uuid>> {
    let claim_id = uuid::Uuid::new_v4();
    // fetched_bytes/last_progress_at defensively re-NULLed (the release
    // already cleared them); stall_count deliberately untouched.
    let result = sqlx::query(
        r#"
        UPDATE manifests
           SET claim_id = $2, claimed_by = $3,
               claim_phase = CASE WHEN $3::text IS NOT NULL THEN 'downloading' END,
               fetched_bytes = NULL, last_progress_at = NULL,
               updated_at = now()
         WHERE store_path_hash = $1
           AND status = 'uploading'
           AND claim_id IS NULL
        "#,
    )
    .bind(store_path_hash)
    .bind(claim_id)
    .bind(claimed_by)
    .execute(pool)
    .await?;
    Ok((result.rows_affected() > 0).then_some(claim_id))
}

/// Take over a **download-stalled** live claim in place: the
/// download-scoped stall-reclaim arm of
/// `r[store.substitute.stale-reclaim+3]`. The predicate is
/// [`STALL_TAKEOVER_PREDICATE`] (single-sourced beside this fn) — the
/// two-clock, phase-keyed rule (092, merged_bug_003):
///
///   `claim_phase = 'downloading'` — parked/persisting owners are
///   exempt AS DATA (the pre-092 `fetched_bytes < nar_size` inference
///   made the persist exemption conditional on the COMPETITOR\'s
///   expected size equalling the owner\'s, and the budget-park froze
///   progress with liveness fresh for >180s, deposing live
///   backpressured owners);
///   `fetched_bytes < $nar_size ∧ last_progress_at stale` — progress
///   froze;
///   `updated_at fresh` — the owner is ALIVE (a dead owner falls to
///   the 300s heartbeat-death reap arm and is reaped, not striked —
///   pre-092 a crashed owner in the 180–300s window collected a
///   bogus strike).
///
/// PutPath claims (`fetched_bytes` NULL, `claim_phase` NULL) stay
/// structurally outside every conjunct, and an
/// advancing owner refreshes `last_progress_at` every heartbeat. The
/// handoff is in place (new `claim_id`/`claimed_by`, progress reset,
/// `stall_count += 1`) so stall evidence accumulates across owners.
/// Claim-guarded against the owner-side release racing the same stall
/// event: whichever lands first wins, the loser matches zero rows —
/// one stall, one strike.
///
/// `nar_size` is the caller's verified-narinfo `NarSize` (every
/// substitution claimant parses the narinfo before claiming); the
/// row itself stores no expected size while uploading.
// r[impl store.substitute.stale-reclaim+3]
#[instrument(skip(pool), fields(store_path_hash = hex::encode(store_path_hash)))]
pub(crate) async fn stall_takeover_placeholder(
    pool: &PgPool,
    store_path_hash: &[u8],
    claimed_by: Option<&str>,
    nar_size: u64,
    stall_window: std::time::Duration,
) -> Result<Option<uuid::Uuid>> {
    let claim_id = uuid::Uuid::new_v4();
    let result = sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"
        UPDATE manifests
           SET claim_id = $2, claimed_by = $3, claim_phase = 'downloading',
               fetched_bytes = NULL, last_progress_at = NULL,
               stall_count = stall_count + 1,
               updated_at = now()
         WHERE store_path_hash = $1
           AND status = 'uploading'
           AND claim_id IS NOT NULL
           {STALL_TAKEOVER_PREDICATE}
        "#
    )))
    .bind(store_path_hash)
    .bind(claim_id)
    .bind(claimed_by)
    .bind(i64::try_from(nar_size).unwrap_or(i64::MAX))
    .bind(stall_window.as_secs_f64())
    .execute(pool)
    .await?;
    Ok((result.rows_affected() > 0).then_some(claim_id))
}

/// The single-sourced stall-takeover rule (092, merged_bug_003;
/// r[store.substitute.stale-reclaim+3]). `$4` = the competitor\'s
/// verified NarSize, `$5` = the stall window in seconds. Two clocks,
/// one phase:
///
/// - `claim_phase = 'downloading'`: parked/persisting owners exempt
///   AS DATA;
/// - progress stale: `fetched_bytes` short of the size and
///   `last_progress_at` older than the window;
/// - liveness FRESH: `updated_at` within the window — dead owners
///   route to the 300s reap arm (reaped, not striked).
///
/// Both time conjuncts evaluate on PG `now()` — no cross-replica
/// clock skew; durability lag of the phase is ≤ one heartbeat
/// (30s ≪ the validated ≥60s stall floor).
pub(crate) const STALL_TAKEOVER_PREDICATE: &str = "AND claim_phase = 'downloading'
           AND fetched_bytes IS NOT NULL AND fetched_bytes < $4
           AND last_progress_at < now() - make_interval(secs => $5)
           AND updated_at      >= now() - make_interval(secs => $5)";

/// Owner-side **release-in-place** after a stall abort
/// (`r[store.substitute.stall-abort]`): relinquish the claim
/// (`claim_id`/`claimed_by` cleared), NULL the progress evidence, and
/// record the strike (`stall_count += 1`) — WITHOUT deleting the row,
/// so the next attempt re-claims immediately
/// ([`claim_released_placeholder`]) and the stall evidence survives.
///
/// Claim-guarded on the aborting owner's `claim_id`: if a competing
/// stall-reclaim already took the row over, this matches zero rows and
/// the stall event still increments `stall_count` exactly once.
/// Returns whether the release applied.
// r[impl store.substitute.stall-abort+2]
#[instrument(skip(pool), fields(store_path_hash = hex::encode(store_path_hash)))]
pub(crate) async fn release_placeholder_in_place(
    pool: &PgPool,
    store_path_hash: &[u8],
    claim: uuid::Uuid,
) -> Result<bool> {
    let result = sqlx::query(
        r#"
        UPDATE manifests
           SET claim_id = NULL, claimed_by = NULL, claim_phase = NULL,
               fetched_bytes = NULL, last_progress_at = NULL,
               stall_count = stall_count + 1,
               updated_at = now()
         WHERE store_path_hash = $1
           AND status = 'uploading'
           AND claim_id = $2
        "#,
    )
    .bind(store_path_hash)
    .bind(claim)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Finalize an inline upload: fill real narinfo + store the NAR in
/// `manifests.inline_blob` + flip status to 'complete'.
///
/// Single transaction: either the path becomes fully visible to
/// `query_path_info` or it stays a placeholder. No partial-complete state.
#[instrument(skip(pool, info, nar_data), fields(store_path = %info.store_path.as_str(), nar_size = nar_data.len()))]
pub(crate) async fn complete_manifest_inline(
    pool: &PgPool,
    info: &ValidatedPathInfo,
    claim: uuid::Uuid,
    nar_data: Bytes,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    super::complete_manifest_in_conn(&mut tx, info, claim, Some(nar_data.as_ref())).await?;
    tx.commit().await?;
    debug!(store_path = %info.store_path.as_str(), "inline upload completed");
    Ok(())
}

/// Age of an existing `'uploading'` placeholder, or `None` if no such
/// placeholder exists (already completed, already cleaned up, or never
/// inserted).
///
/// Test-only since I-040: `Substituter::ingest`'s reclaim now uses
/// [`crate::gc::orphan::reap_one`], which does the stale check
/// in-SQL. This survives as a test helper for asserting "placeholder
/// still present" after a non-reclaiming flow.
#[cfg(test)]
#[instrument(skip(pool), fields(store_path_hash = hex::encode(store_path_hash)))]
pub(crate) async fn manifest_uploading_age(
    pool: &PgPool,
    store_path_hash: &[u8],
) -> Result<Option<std::time::Duration>> {
    // EXTRACT(EPOCH FROM interval) → float8 seconds. Avoids client-side
    // PgInterval arithmetic (months*30d is calendar-incorrect; PG knows the
    // real wall-clock delta). `updated_at` not `created_at`: manifests only
    // has updated_at (002_store.sql:62); same column the orphan scanner
    // checks. GREATEST(..., 0): negative age (clock skew, manual row tweak)
    // clamps to zero → treated as young → not reclaimed, the safe direction.
    let secs: Option<f64> = sqlx::query_scalar(
        r#"
        SELECT GREATEST(EXTRACT(EPOCH FROM (now() - updated_at)), 0)::float8
          FROM manifests
         WHERE store_path_hash = $1 AND status = 'uploading'
        "#,
    )
    .bind(store_path_hash)
    .fetch_optional(pool)
    .await?;

    // merged_bug_262: PG-derived seconds through the total
    // constructor (NaN/neg -> 0, +inf -> 1yr) instead of the panicking
    // raw call.
    Ok(secs.map(rio_common::clamped::clamped_duration_secs))
}

/// Reclaim placeholder rows from a failed upload.
///
/// Only deletes rows where `narinfo.nar_size = 0` AND
/// `manifests.status = 'uploading'`. Both conditions together: if a
/// concurrent upload succeeded, its nar_size is >0 and status is 'complete',
/// so we don't touch it. Safe to call even if no placeholder exists (no-op).
///
/// manifests deleted first (FK dependency: manifests → narinfo). ON DELETE
/// CASCADE on the FK would also work but explicit ordering makes intent
/// clear and doesn't depend on schema details.
///
/// Production callers use [`crate::gc::orphan::reap_one`] (the
/// claim/stale-gated path-row janitor). This inline-only delete is
/// kept for the defense-in-depth test that asserts an abandoned chunk
/// row no longer causes upload-skip.
#[cfg(test)]
#[instrument(skip(pool), fields(store_path_hash = hex::encode(store_path_hash)))]
pub(crate) async fn delete_manifest_uploading(pool: &PgPool, store_path_hash: &[u8]) -> Result<()> {
    let mut tx = pool.begin().await?;

    sqlx::query(
        r#"
        DELETE FROM manifests
        WHERE store_path_hash = $1 AND status = 'uploading'
        "#,
    )
    .bind(store_path_hash)
    .execute(&mut *tx)
    .await?;

    // nar_size = 0 is the placeholder marker. A successful upload ALWAYS
    // has nar_size > 0 (min valid NAR is ~100 bytes).
    sqlx::query(
        r#"
        DELETE FROM narinfo
        WHERE store_path_hash = $1 AND nar_size = 0
        "#,
    )
    .bind(store_path_hash)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// Check if a store path already has a completed upload.
///
/// Idempotency pre-check for PutPath: if `true`, the path exists and the
/// caller should return `created: false` without touching anything.
#[instrument(skip(pool), fields(store_path_hash = hex::encode(store_path_hash)))]
pub(crate) async fn check_manifest_complete(pool: &PgPool, store_path_hash: &[u8]) -> Result<bool> {
    // EXISTS returns a PG bool, which sqlx decodes cleanly. `SELECT 1` would
    // return int4 and need i32 (not i64) — a type-width footgun that turns
    // into an opaque "ColumnDecode" runtime error. EXISTS sidesteps it
    // entirely and is also the idiomatic existence-check query.
    let exists: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM manifests
            WHERE store_path_hash = $1 AND status = 'complete'
        )
        "#,
    )
    .bind(store_path_hash)
    .fetch_one(pool)
    .await?;

    Ok(exists)
}
