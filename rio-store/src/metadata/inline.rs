//! Inline write-ahead: small NARs stored directly in `manifests.inline_blob`.
//!
//! Step 1 (`insert_manifest_uploading`) writes a placeholder with
//! `nar_size = 0` + `status = 'uploading'`. Step 3 (`complete_manifest_inline`)
//! fills real narinfo and stores the NAR blob atomically. On failure,
// r[impl store.inline.threshold]
//! `delete_manifest_uploading` reclaims the placeholder (guarded by
//! `nar_size = 0` so a concurrent successful upload is never touched).

use super::*;
use sqlx::{PgPool, Postgres, Transaction};
use tracing::{debug, instrument};

/// Bounded retry budget for the shared `GC_MARK_LOCK_ID` acquisition in
/// [`insert_manifest_uploading`]. With [`MARK_LOCK_JITTER_MS`] this gives
/// a ~2–5 s window — sized to outlast `compute_unreachable` on a
/// tens-of-thousands-row narinfo table (I-168).
const MARK_LOCK_ATTEMPTS: u32 = 10;

/// Per-attempt sleep range (ms) for [`acquire_mark_lock_shared`].
/// 200–500 ms: long enough that 1024 concurrent waiters (32 sessions ×
/// 32-deep pipeline) don't hammer PG with try-lock queries every 50 ms;
/// short enough that the median waiter sees ≤2 attempts once mark
/// releases.
const MARK_LOCK_JITTER_MS: (u64, u64) = (200, 500);

/// Begin a transaction holding `GC_MARK_LOCK_ID` shared (txn-scoped).
///
/// `try` variant in a bounded loop instead of the blocking
/// `pg_advisory_xact_lock_shared`: the blocking form holds a pool
/// connection for the full wait, and PG `lock_timeout` does NOT bound
/// advisory-lock waits — `statement_timeout` would, but that holds the
/// conn for the duration too. With 1024 concurrent waiters × 50-conn
/// pool that's a `PoolTimedOut` cascade. The loop drops the txn
/// (rollback → conn returned to pool) BEFORE sleeping, so waiters
/// occupy zero connections between attempts.
///
/// Returns the open transaction on success (caller commits/rolls back;
/// the xact-scoped lock releases at txn end). Returns
/// [`MetadataError::GcMarkBusy`] on exhaustion.
// r[impl store.put.gc-mark-retry]
async fn acquire_mark_lock_shared(pool: &PgPool) -> Result<Transaction<'_, Postgres>> {
    for attempt in 1..=MARK_LOCK_ATTEMPTS {
        let mut tx = pool.begin().await?;
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock_shared($1)")
            .bind(crate::gc::GC_MARK_LOCK_ID)
            .fetch_one(&mut *tx)
            .await?;
        if locked {
            return Ok(tx);
        }
        // Roll back NOW so the conn goes back to the pool before the
        // sleep. Dropping the tx is sufficient (sqlx auto-rolls back on
        // drop), but the explicit drop makes the ordering load-bearing.
        drop(tx);
        if attempt < MARK_LOCK_ATTEMPTS {
            debug!(
                attempt,
                max = MARK_LOCK_ATTEMPTS,
                "GC_MARK_LOCK_ID held exclusive; retrying shared acquire after jitter"
            );
            let (lo, hi) = MARK_LOCK_JITTER_MS;
            tokio::time::sleep(super::jitter_range(lo, hi)).await;
        }
    }
    Err(MetadataError::GcMarkBusy)
}

/// Begin a new upload: insert placeholder narinfo + manifest rows.
///
/// The placeholder narinfo has `nar_hash = [0;32]` and `nar_size = 0`.
/// `nar_size = 0` is the placeholder marker: the minimum valid NAR is ~100
/// bytes, so 0 unambiguously means "not a real upload yet". This lets
/// `delete_manifest_uploading` identify placeholders without touching a
/// concurrent successful upload of the same path.
///
/// `references` is populated on the placeholder so GC mark's CTE walks
/// them from the instant this tx commits — the closure is protected
/// WITHOUT holding a session lock for the full upload duration. This is
/// the structural fix for pool exhaustion: previously PutPath held a
/// dedicated pool connection (shared `GC_MARK_LOCK_ID`) from placeholder
/// insert through complete (~40s for 4 GiB @ 100 MB/s); 21st concurrent
/// upload → PoolTimedOut. Now the lock is transaction-scoped (~ms).
///
/// Returns `true` if inserted, `false` if another upload already holds a
/// placeholder (caller should re-check `check_manifest_complete` — the race
/// winner may have finished). Returns `MetadataError::GcMarkBusy` if GC
/// mark holds `GC_MARK_LOCK_ID` exclusive for longer than
/// [`MARK_LOCK_ATTEMPTS`]×[`MARK_LOCK_JITTER_MS`] (retriable; maps to
/// ABORTED).
#[instrument(skip(pool, references), fields(store_path_hash = hex::encode(store_path_hash), refs = references.len()))]
pub async fn insert_manifest_uploading(
    pool: &PgPool,
    store_path_hash: &[u8],
    store_path: &str,
    references: &[String],
) -> Result<bool> {
    let mut tx = acquire_mark_lock_shared(pool).await?;

    // narinfo placeholder first (manifests has FK to narinfo). ON CONFLICT
    // DO NOTHING: if another uploader already inserted, we don't clobber.
    // REFERENCES POPULATED HERE — mark's CTE walks them from the instant
    // this tx commits. This is what lets the lock be tx-scoped instead of
    // held for the full upload: the placeholder itself protects its refs.
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
    // rows_affected = 0 means another uploader owns this slot.
    let result = sqlx::query(
        r#"
        INSERT INTO manifests (store_path_hash, status)
        VALUES ($1, 'uploading')
        ON CONFLICT (store_path_hash) DO NOTHING
        "#,
    )
    .bind(store_path_hash)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(result.rows_affected() > 0)
}

/// Finalize an inline upload: fill real narinfo + store the NAR in
/// `manifests.inline_blob` + flip status to 'complete'.
///
/// Single transaction: either the path becomes fully visible to
/// `query_path_info` or it stays a placeholder. No partial-complete state.
#[instrument(skip(pool, info, nar_data), fields(store_path = %info.store_path.as_str(), nar_size = nar_data.len()))]
pub async fn complete_manifest_inline(
    pool: &PgPool,
    info: &ValidatedPathInfo,
    nar_data: Bytes,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    complete_manifest_inline_in_tx(&mut tx, info, nar_data).await?;
    tx.commit().await?;
    debug!(store_path = %info.store_path.as_str(), "inline upload completed");
    Ok(())
}

/// Transaction-body variant of [`complete_manifest_inline`]: runs the
/// narinfo UPDATE + manifests UPDATE inside a caller-owned transaction.
///
/// `PutPathBatch` calls this N times inside ONE `pool.begin()` to achieve
/// cross-output atomicity (all outputs flip to 'complete' or none do).
/// [`complete_manifest_inline`] is a thin wrapper that begins/commits
/// around one call.
///
/// Takes `&mut PgConnection` (not `&mut Transaction`) to match
/// [`update_narinfo_complete`]'s signature — `&mut *tx` deref-coerces
/// a `Transaction` to `PgConnection`.
pub async fn complete_manifest_inline_in_tx(
    conn: &mut sqlx::PgConnection,
    info: &ValidatedPathInfo,
    nar_data: Bytes,
) -> Result<()> {
    if update_narinfo_complete(conn, info).await? == 0 {
        // insert_manifest_uploading MUST have run first. If rows_affected
        // is 0, delete_manifest_uploading raced us and won. The caller's
        // placeholder is gone; bailing here prevents a half-complete write.
        return Err(MetadataError::PlaceholderMissing {
            store_path: info.store_path.to_string(),
        });
    }

    // Store NAR + flip status. nar_data is Bytes (Arc-refcounted); sqlx binds
    // &[u8], so .as_ref() — no copy.
    let manifest_result = sqlx::query(
        r#"
        UPDATE manifests SET
            status      = 'complete',
            inline_blob = $2,
            updated_at  = now()
        WHERE store_path_hash = $1
        "#,
    )
    .bind(&info.store_path_hash)
    .bind(nar_data.as_ref())
    .execute(&mut *conn)
    .await?;

    if manifest_result.rows_affected() == 0 {
        return Err(MetadataError::PlaceholderMissing {
            store_path: info.store_path.to_string(),
        });
    }

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
pub async fn manifest_uploading_age(
    pool: &PgPool,
    store_path_hash: &[u8],
) -> Result<Option<std::time::Duration>> {
    // PG INTERVAL → sqlx PgInterval. Microsecond precision; we floor to
    // whole seconds (stale-threshold comparison is in the minutes range,
    // sub-second precision irrelevant). `updated_at` not `created_at`:
    // manifests only has updated_at (002_store.sql:62); same column the
    // orphan scanner checks.
    let interval: Option<sqlx::postgres::types::PgInterval> = sqlx::query_scalar(
        r#"
        SELECT now() - updated_at
          FROM manifests
         WHERE store_path_hash = $1 AND status = 'uploading'
        "#,
    )
    .bind(store_path_hash)
    .fetch_optional(pool)
    .await?;

    Ok(interval.map(|iv| {
        // PgInterval = months*30d + days*24h + microseconds. For
        // now()-updated_at on a recent placeholder, months=days=0 in
        // practice. Include them anyway for correctness if a placeholder
        // somehow survives that long. saturating_add: negative age
        // (clock skew, manual row tweak) clamps to zero → treated as
        // young → not reclaimed, which is the safe direction.
        let secs = (iv.months as i64 * 30 * 86400)
            .saturating_add(iv.days as i64 * 86400)
            .saturating_add(iv.microseconds / 1_000_000)
            .max(0) as u64;
        std::time::Duration::from_secs(secs)
    }))
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
#[instrument(skip(pool), fields(store_path_hash = hex::encode(store_path_hash)))]
pub async fn delete_manifest_uploading(pool: &PgPool, store_path_hash: &[u8]) -> Result<()> {
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
pub async fn check_manifest_complete(pool: &PgPool, store_path_hash: &[u8]) -> Result<bool> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;

    // r[verify store.put.gc-mark-retry]
    /// I-168 reproduction: hold `GC_MARK_LOCK_ID` exclusive on a
    /// session-level connection, spawn `insert_manifest_uploading`
    /// concurrently, release the exclusive at t≈1 s. The insert MUST
    /// succeed (proves the bounded-retry loop waits and re-acquires)
    /// instead of returning the old single-shot `Serialization` error.
    ///
    /// Uses session-level `pg_advisory_lock` (NOT xact-level) so the
    /// holder doesn't need an open transaction across the sleep — the
    /// lock survives until explicit `pg_advisory_unlock` or connection
    /// close. Mirrors `gc/mod.rs:run_gc`, which uses the session-level
    /// form.
    #[tokio::test]
    async fn insert_retries_across_gc_mark_window() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Dedicated holder connection. Session-level exclusive lock.
        let mut holder = db.pool.acquire().await.unwrap();
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(crate::gc::GC_MARK_LOCK_ID)
            .execute(&mut *holder)
            .await
            .unwrap();

        // Release after ~1 s — within the 10×200–500 ms retry window
        // but well past a single attempt, so the test fails on the old
        // single-shot behavior.
        let release = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            sqlx::query("SELECT pg_advisory_unlock($1)")
                .bind(crate::gc::GC_MARK_LOCK_ID)
                .execute(&mut *holder)
                .await
                .unwrap();
            // holder dropped here → conn returned to pool.
        });

        let store_path_hash = vec![0x68u8; 32];
        let path = rio_test_support::fixtures::test_store_path("gc-mark-retry");
        let inserted = insert_manifest_uploading(&db.pool, &store_path_hash, &path, &[])
            .await
            .expect("insert should succeed after mark releases within retry window");
        assert!(inserted, "fresh path → placeholder inserted");

        release.await.unwrap();
    }

    // r[verify store.put.gc-mark-retry]
    /// Negative: if mark holds exclusive for the ENTIRE retry window,
    /// the insert returns `GcMarkBusy` (not `Serialization`, not a
    /// hang). Proves the loop is bounded and the new variant surfaces.
    #[tokio::test]
    async fn insert_exhausts_retry_returns_gc_mark_busy() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let mut holder = db.pool.acquire().await.unwrap();
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(crate::gc::GC_MARK_LOCK_ID)
            .execute(&mut *holder)
            .await
            .unwrap();

        // Bound the whole call so a regression to the blocking
        // `pg_advisory_xact_lock_shared` form would fail the test
        // instead of hanging it. 10×500 ms = 5 s max; 8 s ceiling.
        let store_path_hash = vec![0x69u8; 32];
        let path = rio_test_support::fixtures::test_store_path("gc-mark-busy");
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            insert_manifest_uploading(&db.pool, &store_path_hash, &path, &[]),
        )
        .await
        .expect("retry loop must be bounded (no blocking-lock regression)")
        .expect_err("exclusive held for full window → GcMarkBusy");
        assert!(
            matches!(err, MetadataError::GcMarkBusy),
            "got {err:?}, expected GcMarkBusy"
        );

        // Release so the holder conn drops cleanly.
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(crate::gc::GC_MARK_LOCK_ID)
            .execute(&mut *holder)
            .await
            .unwrap();
    }
}
