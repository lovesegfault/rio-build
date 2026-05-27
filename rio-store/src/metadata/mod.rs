//! Store path metadata persistence in PostgreSQL.
//!
//! CRUD operations for the `narinfo` and `manifests` tables defined in
//! `migrations/002_store.sql`.
//!
//! # Storage model
//!
//! NAR content is FastCDC-chunked: chunk bodies live in the chunk
//! backend (S3/filesystem), `manifest_data.chunk_list` holds the
//! ordered (blake3, size) list. Every complete manifest has a
//! `manifest_data` row. (The pre-P0583 `manifests.inline_blob` fast
//! path for small NARs is gone — migration 065 dropped the column.)
//!
//! # Write-ahead pattern
//!
//! 1. `insert_manifest_uploading()` — writes placeholder narinfo + manifest
//!    with `status='uploading'`. Protects the upload from concurrent GC.
//! 2. Caller uploads chunks.
//! 3. `complete_manifest_chunked()` — fills real narinfo metadata + flips
//!    `status='complete'` atomically.
//!
//! On failure between 1 and 3, `gc::orphan::reap_one()` reclaims the
//! placeholder. It only touches rows owned by the caller's `claim_id`,
//! so it's safe even if a concurrent upload already succeeded.
//!
//! `query_path_info()` and `find_missing_paths()` filter on
//! `manifests.status = 'complete'`, so placeholders are never exposed.

use std::time::Duration;

use rio_proto::validated::ValidatedPathInfo;
use tracing::{instrument, warn};

mod chunked;
mod cluster_key_history;
mod queries;
pub(crate) mod tenant_keys;
pub(crate) mod upstreams;

// Public API — explicit re-exports so all external callers in grpc/,
// cas.rs keep their `metadata::foo` paths. Kept explicit (not
// `pub use chunked::*` etc.) so dead items in submodules
// surface as `unused` instead of being silently exported.
pub(crate) use chunked::{
    PlaceholderToken, complete_manifest_chunked, delete_manifest_chunked_uploading,
    heartbeat_pending_chunks, mark_chunks_durable, mark_chunks_uploaded, register_pending_chunks,
    upgrade_manifest_to_chunked,
};
pub(crate) use cluster_key_history::load_cluster_key_history;
pub(crate) use queries::{
    append_signatures, bump_compat_attempt, bump_nar_index_retry, count_compat_pending,
    find_missing_paths, get_manifest, get_manifest_batch, get_manifest_for_index, get_nar_index,
    list_compat_pending, list_nar_index_pending, path_by_nar_hash, query_by_hash_part,
    query_path_info, query_path_info_batch, set_compat_file_hash, set_nar_index,
};
pub(crate) use tenant_keys::get_active_signer;
pub(crate) use upstreams::{SigMode, Upstream};

// Error type lives in `crate::error` so the `schema` feature can
// compile it without pulling `bytes`/`rio_proto`. Re-exported here so
// every existing `metadata::MetadataError` / `metadata::Result` path
// keeps working unchanged.
pub(crate) use crate::error::{MetadataError, Result};

/// PG 40P01-retry backoff: ~50–150 ms (`100ms ± 50%`). One-shot — no
/// exponential growth (mult=1, single attempt). Just enough to
/// desynchronize two retrying txns so they don't re-collide in
/// lockstep.
const PG_DEADLOCK_BACKOFF: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
    base: Duration::from_millis(100),
    mult: 1.0,
    cap: Duration::from_millis(100),
    jitter: rio_common::backoff::Jitter::Proportional(0.5),
};

/// 50–150ms jitter; see [`PG_DEADLOCK_BACKOFF`].
pub(crate) fn jitter() -> Duration {
    PG_DEADLOCK_BACKOFF.duration(0)
}

/// Execute a batch `UPDATE ... WHERE <key> = ANY($1)` with deadlock-safe
/// lock ordering. Sorts the input before binding so all callers acquire
/// PG row locks in the same deterministic order (prevents circular wait
/// → SQLSTATE 40P01). Wraps in a single retry-on-40P01: the sort SHOULD
/// prevent deadlock, but PG can still hit it on index-page splits under
/// extreme contention; one retry is cheap, unbounded retry masks real
/// problems.
///
/// The `body` closure receives the SORTED keys (owned, cloned once per
/// attempt) and must perform the full transaction (begin→UPDATE→commit).
/// On 40P01, the closure is re-invoked after jitter — PG aborts the
/// whole txn on deadlock, not just the failing statement.
///
/// Owned `Vec<Vec<u8>>` (not `&[Vec<u8>]`): the closure returns a
/// `Future` that must own its captures across `.await` points; a slice
/// borrow into the helper's stack would need higher-ranked trait bounds.
/// The one-clone cost (~KB for typical chunk batches) is negligible
/// versus PG roundtrips.
// r[impl store.chunk.lock-order]
pub(crate) async fn with_sorted_retry<T, F, Fut>(mut keys: Vec<Vec<u8>>, body: F) -> Result<T>
where
    F: Fn(Vec<Vec<u8>>) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    keys.sort_unstable();
    match body(keys.clone()).await {
        Err(MetadataError::Deadlock(e)) => {
            warn!(error = %e, "40P01 on batch UPDATE; retrying once after jitter");
            tokio::time::sleep(jitter()).await;
            body(keys).await
        }
        r => r,
    }
}

/// A path's chunk manifest: the ordered `(blake3_digest, size)` list a
/// NAR reassembles from. Returned by [`get_manifest`].
///
/// Decoded from `manifest_data.chunk_list` in exactly one place
/// (`queries::decode_chunk_list`); callers never query `manifest_data`
/// directly.
#[derive(Debug)]
pub(crate) struct ChunkManifest(pub(crate) Vec<([u8; 32], u32)>);

impl ChunkManifest {
    /// Total NAR size in bytes this manifest will reassemble to.
    ///
    /// Sum of chunk sizes (u64 — see
    /// [`crate::manifest::Manifest::total_size`] for the u32-overflow
    /// rationale). GetPath checks this against `narinfo.nar_size`
    /// before streaming so manifest/narinfo drift fails fast with
    /// DATA_LOSS instead of delivering garbage.
    pub(crate) fn total_size(&self) -> u64 {
        self.0.iter().map(|(_, size)| *size as u64).sum()
    }
}

// ---------------------------------------------------------------------------
// Shared helpers — NarinfoRow column list + validation epilogue + UPDATE SQL
//
// These are used by query_path_info and query_by_hash_part. Extracting
// them means adding a column to NarinfoRow requires editing ONE macro,
// not N SELECT strings.
// ---------------------------------------------------------------------------

/// Expands to the 10-column SELECT list for `NarinfoRow`, aliased `n.*`.
/// A macro (not a const) so `concat!` can embed it in query literals —
/// `concat!` only accepts literal tokens, not const expressions, and we
/// want compile-time strings (no per-query `format!` alloc).
#[macro_export]
#[doc(hidden)]
macro_rules! narinfo_cols {
    () => {
        r#"n.store_path, n.store_path_hash, n.deriver, n.nar_hash, n.nar_size,
           n."references", n.signatures, n.ca, n.registration_time, n.ultimate"#
    };
}

#[derive(sqlx::FromRow)]
pub(crate) struct NarinfoRow {
    store_path: String,
    store_path_hash: Vec<u8>,
    deriver: Option<String>,
    nar_hash: Vec<u8>,
    nar_size: i64,
    references: Vec<String>,
    signatures: Vec<String>,
    ca: Option<String>,
    registration_time: i64,
    ultimate: bool,
}

impl NarinfoRow {
    pub(crate) fn try_into_validated(self) -> Result<ValidatedPathInfo> {
        use rio_proto::types::PathInfo;
        // i64 → u64: PG stores nar_size and registration_time as bigint
        // (signed). Both are non-negative by construction (nar_size from a
        // Vec::len(); registration_time from epoch seconds). A negative
        // value is row-level corruption — surface it as InvariantViolation
        // rather than `as u64`-wrapping to a huge value that masquerades
        // as valid downstream.
        let nar_size = u64::try_from(self.nar_size).map_err(|_| {
            MetadataError::InvariantViolation(format!(
                "narinfo.nar_size for {} is negative ({})",
                self.store_path, self.nar_size
            ))
        })?;
        let registration_time = u64::try_from(self.registration_time).map_err(|_| {
            MetadataError::InvariantViolation(format!(
                "narinfo.registration_time for {} is negative ({})",
                self.store_path, self.registration_time
            ))
        })?;
        // Build raw PathInfo then delegate to the centralized TryFrom —
        // keeps validation logic in one place (rio-proto::validated), not
        // duplicated here.
        ValidatedPathInfo::try_from(PathInfo {
            store_path: self.store_path,
            store_path_hash: self.store_path_hash,
            deriver: self.deriver.unwrap_or_default(),
            nar_hash: self.nar_hash,
            nar_size,
            references: self.references,
            registration_time,
            ultimate: self.ultimate,
            signatures: self.signatures,
            content_address: self.ca.unwrap_or_default(),
        })
        .map_err(MetadataError::MalformedRow)
    }
}

/// Convert `Option<NarinfoRow>` → `Result<Option<ValidatedPathInfo>>`.
///
/// Shared epilogue for the three fetch_optional → validate queries.
/// DB-egress validation: a malformed row (garbage store_path, wrong-length
/// nar_hash) would otherwise propagate silently. Caught here at the trust
/// boundary — PG doesn't enforce these as CHECK constraints.
pub(crate) fn validate_row(row: Option<NarinfoRow>) -> Result<Option<ValidatedPathInfo>> {
    row.map(NarinfoRow::try_into_validated).transpose()
}

/// Fill the real narinfo fields (replacing placeholder zeros).
///
/// Runs inside [`complete_manifest_in_conn`] after the claim-gated
/// manifests UPDATE proves ownership. Returns rows_affected so callers
/// can check for the placeholder-raced-away case.
pub(super) async fn update_narinfo_complete(
    tx: &mut sqlx::PgConnection,
    info: &ValidatedPathInfo,
) -> std::result::Result<u64, sqlx::Error> {
    let deriver_str = info.deriver.as_ref().map(|d| d.to_string());
    let refs_str: Vec<String> = info.references.iter().map(|r| r.to_string()).collect();
    let ca_str = info.content_address.as_deref();

    sqlx::query(
        r#"
        UPDATE narinfo SET
            deriver           = $2,
            nar_hash          = $3,
            nar_size          = $4,
            "references"      = $5,
            signatures        = $6,
            ca                = $7,
            registration_time = $8,
            ultimate          = $9
        WHERE store_path_hash = $1
        "#,
    )
    .bind(&info.store_path_hash)
    .bind(deriver_str)
    .bind(info.nar_hash.as_slice())
    .bind(info.nar_size as i64)
    .bind(&refs_str)
    .bind(&info.signatures)
    .bind(ca_str)
    .bind(info.registration_time as i64)
    .bind(info.ultimate)
    .execute(&mut *tx)
    .await
    .map(|r| r.rows_affected())
}

/// Finalize an upload inside a caller-owned tx/connection: flip
/// `manifests.status = 'complete'`, then narinfo UPDATE. `PutPathBatch`
/// calls this N times inside one `pool.begin()` for cross-output
/// atomicity; the pool-wrapping [`complete_manifest_chunked`] wraps a
/// single call.
///
/// `claim` is the ownership token from [`insert_manifest_uploading`].
/// The manifests UPDATE filters on it so a stale uploader whose row
/// was reaped (and replaced by a fresh re-upload at the same
/// `store_path_hash`) gets `rows_affected==0 → PlaceholderMissing`
/// instead of overwriting the re-uploader's `signatures`/`deriver`/
/// `registration_time` (`r[store.put.placeholder-claim+2]`). The
/// manifests UPDATE runs FIRST so a foreign-claim call touches zero
/// rows in any table; `update_narinfo_complete` (no `claim_id` column
/// on narinfo) only runs once ownership is proven.
// r[impl store.put.placeholder-claim+2]
pub(crate) async fn complete_manifest_in_conn(
    conn: &mut sqlx::PgConnection,
    info: &ValidatedPathInfo,
    claim: uuid::Uuid,
) -> Result<()> {
    let manifest_result = sqlx::query(
        r#"
        UPDATE manifests SET
            status     = 'complete',
            updated_at = now()
        WHERE store_path_hash = $1 AND claim_id = $2
        "#,
    )
    .bind(&info.store_path_hash)
    .bind(claim)
    .execute(&mut *conn)
    .await?;
    if manifest_result.rows_affected() == 0 {
        // insert_manifest_uploading MUST have run first. If rows_affected
        // is 0, either delete_manifest_uploading raced us and won, OR
        // our row was stale-reaped and a fresh re-upload (different
        // claim_id) now holds the slot. Either way the caller's
        // placeholder is gone; bailing here prevents a half-complete /
        // foreign-clobbering write.
        return Err(MetadataError::PlaceholderMissing {
            store_path: info.store_path.to_string(),
        });
    }
    if update_narinfo_complete(conn, info).await? == 0 {
        return Err(MetadataError::PlaceholderMissing {
            store_path: info.store_path.to_string(),
        });
    }
    Ok(())
}

// r[impl store.put.tenant-attribution+2]
/// Attribute one store path to one tenant inside the caller's
/// transaction: `INSERT INTO path_tenants … ON CONFLICT DO NOTHING`,
/// FK-guarded via `SELECT … FROM tenants` so a tenant deleted mid-upload
/// degrades to "no row" instead of failing the commit (same insert shape
/// as `PutPathChunked`'s commit).
///
/// `path_tenants` is the read-time tenancy join for the castore RPCs
/// (`r[store.castore.tenant-scope+2]`) and the narinfo visibility gate, so
/// callers run this in the same transaction that flips the manifest to
/// `'complete'` — a committed upload is never visible-but-unattributed,
/// and a rolled-back commit never leaves a stray attribution row.
pub(crate) async fn upsert_path_tenant_in_conn(
    conn: &mut sqlx::PgConnection,
    store_path_hash: &[u8],
    tenant_id: uuid::Uuid,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO path_tenants (store_path_hash, tenant_id) \
         SELECT $1, t.tenant_id FROM tenants t WHERE t.tenant_id = $2 \
         ON CONFLICT DO NOTHING",
    )
    .bind(store_path_hash)
    .bind(tenant_id)
    .execute(conn)
    .await
    .map(|_| ())
}

/// Begin a new upload: insert placeholder narinfo + manifest rows.
///
/// The placeholder narinfo has `nar_hash = [0;32]` and `nar_size = 0`.
/// `nar_size = 0` is the placeholder marker: the minimum valid NAR is ~100
/// bytes, so 0 unambiguously means "not a real upload yet".
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
// r[impl store.put.placeholder-refs]
#[instrument(skip(pool, references), fields(store_path_hash = hex::encode(store_path_hash), refs = references.len()))]
pub(crate) async fn insert_manifest_uploading(
    pool: &sqlx::PgPool,
    store_path_hash: &[u8],
    store_path: &str,
    references: &[String],
) -> Result<Option<uuid::Uuid>> {
    let mut tx = pool.begin().await?;

    // narinfo placeholder first (manifests has FK to narinfo). ON CONFLICT
    // DO NOTHING: if another uploader already inserted, we don't clobber.
    // REFERENCES POPULATED HERE — this is what makes the placeholder itself
    // protect its closure (via mark seed (b) or sweep re-check) without an
    // advisory lock.
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
        INSERT INTO manifests (store_path_hash, status, claim_id)
        VALUES ($1, 'uploading', $2)
        ON CONFLICT (store_path_hash) DO NOTHING
        "#,
    )
    .bind(store_path_hash)
    .bind(claim_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok((result.rows_affected() > 0).then_some(claim_id))
}

/// Check if a store path already has a completed upload.
///
/// Idempotency pre-check for PutPath: if `true`, the path exists and the
/// caller should return `created: false` without touching anything.
#[instrument(skip(pool), fields(store_path_hash = hex::encode(store_path_hash)))]
pub(crate) async fn check_manifest_complete(
    pool: &sqlx::PgPool,
    store_path_hash: &[u8],
) -> Result<bool> {
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
    pool: &sqlx::PgPool,
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

    Ok(secs.map(std::time::Duration::from_secs_f64))
}

/// Reclaim placeholder rows from a failed upload — narinfo + manifests
/// rows ONLY, no chunk-refcount decrement.
///
/// Test-only: production callers use [`crate::gc::orphan::reap_one`]
/// (chunk-aware). This row-only delete is kept for the defense-in-depth
/// test that asserts a leaked refcount no longer causes upload-skip.
#[cfg(test)]
#[instrument(skip(pool), fields(store_path_hash = hex::encode(store_path_hash)))]
pub(crate) async fn delete_manifest_uploading(
    pool: &sqlx::PgPool,
    store_path_hash: &[u8],
) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;

    // =======================================================================
    // MetadataError classification (From<sqlx::Error>)
    // =======================================================================

    #[test]
    fn classify_row_not_found() {
        let e: MetadataError = sqlx::Error::RowNotFound.into();
        assert!(matches!(e, MetadataError::NotFound));
    }

    #[test]
    fn classify_pool_timed_out() {
        let e: MetadataError = sqlx::Error::PoolTimedOut.into();
        // PoolTimedOut = PG reachable but all pool connections
        // checked out. Maps to ResourceExhausted (backoff-retry),
        // not Connection (try-another-replica).
        assert!(matches!(e, MetadataError::ResourceExhausted(_)));
    }

    #[test]
    fn classify_pool_closed() {
        let e: MetadataError = sqlx::Error::PoolClosed.into();
        assert!(matches!(e, MetadataError::Connection(_)));
    }

    /// Decode errors, column-type mismatches, protocol weirdness —
    /// anything not explicitly classified lands in Other.
    #[test]
    fn classify_unknown_falls_through_to_other() {
        let e: MetadataError = sqlx::Error::ColumnNotFound("x".into()).into();
        assert!(matches!(e, MetadataError::Other(_)));
    }

    // =======================================================================
    // Integration: trigger real PG SQLSTATE codes, assert classification
    // =======================================================================

    /// Real 23505 unique_violation → Conflict. Insert the same PK twice.
    #[tokio::test]
    async fn integration_unique_violation_is_conflict() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash = vec![0xAAu8; 32];

        // First insert: OK.
        sqlx::query(
            "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) \
             VALUES ($1, '/nix/store/a-x', $2, 0)",
        )
        .bind(&hash)
        .bind(vec![0u8; 32])
        .execute(&db.pool)
        .await
        .unwrap();

        // Second insert on same PK: 23505.
        let err: MetadataError = sqlx::query(
            "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) \
             VALUES ($1, '/nix/store/a-x', $2, 0)",
        )
        .bind(&hash)
        .bind(vec![0u8; 32])
        .execute(&db.pool)
        .await
        .unwrap_err()
        .into();

        assert!(
            matches!(err, MetadataError::Conflict(_)),
            "expected Conflict for 23505 unique_violation, got {err:?}"
        );
    }

    /// Real 23503 foreign_key_violation → Conflict. Insert a manifests
    /// row whose store_path_hash FK doesn't exist in narinfo.
    #[tokio::test]
    async fn integration_fk_violation_is_conflict() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let err: MetadataError = sqlx::query("INSERT INTO manifests (store_path_hash) VALUES ($1)")
            .bind(vec![0xBBu8; 32]) // no matching narinfo row
            .execute(&db.pool)
            .await
            .unwrap_err()
            .into();

        assert!(
            matches!(err, MetadataError::Conflict(_)),
            "expected Conflict for 23503 foreign_key_violation, got {err:?}"
        );
    }

    /// Real 57P01 admin_shutdown → Connection (retriable, NOT Other).
    /// PG sends class-57 as `ErrorResponse` on the wire (→
    /// `sqlx::Error::Database`, not `Io`); without the explicit match
    /// arm a routine PG rolling restart surfaces as non-retriable
    /// `Internal`. PL/pgSQL `RAISE … USING ERRCODE` produces a real
    /// `Database` error carrying the SQLSTATE.
    #[tokio::test]
    async fn integration_admin_shutdown_is_connection() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        for code in ["57P01", "57P02", "57P03"] {
            // AssertSqlSafe: SQLSTATE code from the fixed array above, test-only.
            let err: MetadataError = sqlx::query(sqlx::AssertSqlSafe(format!(
                "DO $$ BEGIN RAISE EXCEPTION 'shutdown' USING ERRCODE = '{code}'; END $$"
            )))
            .execute(&db.pool)
            .await
            .unwrap_err()
            .into();

            assert!(
                matches!(err, MetadataError::Connection(_)),
                "expected Connection for {code}, got {err:?}"
            );
        }
    }

    /// PlaceholderMissing: call complete_manifest_chunked WITHOUT
    /// insert_manifest_uploading first. rows_affected() == 0 on both
    /// UPDATEs → PlaceholderMissing, NOT a sqlx error.
    #[tokio::test]
    async fn integration_complete_without_placeholder_is_placeholder_missing() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let info = rio_test_support::fixtures::make_path_info(
            &rio_test_support::fixtures::test_store_path("noplaceholder"),
            b"nar",
            [0xCCu8; 32],
        );

        let err = complete_manifest_chunked(&db.pool, &info, uuid::Uuid::new_v4(), &[], None)
            .await
            .expect_err("should fail without placeholder");

        assert!(
            matches!(err, MetadataError::PlaceholderMissing { .. }),
            "expected PlaceholderMissing, got {err:?}"
        );
    }

    /// `r[store.put.placeholder-claim+2]`: a stale uploader whose row
    /// was reaped + replaced must NOT clobber the re-uploader's
    /// metadata. `complete_manifest_in_conn` filters on `claim_id` so
    /// the foreign call returns `PlaceholderMissing` and touches zero
    /// rows; the real claim then succeeds.
    // r[verify store.put.placeholder-claim+2]
    #[tokio::test]
    async fn complete_manifest_rejects_foreign_claim() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let path = rio_test_support::fixtures::test_store_path("foreign-claim");
        let mut info = rio_test_support::fixtures::make_path_info(&path, b"nar", [0xC1u8; 32]);
        info.signatures = vec!["good:sig".into()];
        let store_path_hash = info.store_path.sha256_digest().to_vec();
        info.store_path_hash = store_path_hash.clone();

        let claim_a = insert_manifest_uploading(&db.pool, &store_path_hash, &path, &[])
            .await
            .unwrap()
            .unwrap();

        // Foreign claim → PlaceholderMissing; row stays 'uploading'
        // with the original claim and narinfo untouched (nar_size=0).
        let mut bad = info.clone();
        bad.signatures = vec!["evil:sig".into()];
        let err = complete_manifest_chunked(&db.pool, &bad, uuid::Uuid::new_v4(), &[], None)
            .await
            .expect_err("foreign claim must fail");
        assert!(
            matches!(err, MetadataError::PlaceholderMissing { .. }),
            "expected PlaceholderMissing, got {err:?}"
        );
        let (status, claim, nar_size): (String, uuid::Uuid, i64) = sqlx::query_as(
            "SELECT m.status::text, m.claim_id, n.nar_size \
             FROM manifests m JOIN narinfo n USING (store_path_hash) \
             WHERE m.store_path_hash = $1",
        )
        .bind(&store_path_hash)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(status, "uploading", "foreign complete must not flip status");
        assert_eq!(claim, claim_a, "claim_id unchanged");
        assert_eq!(nar_size, 0, "narinfo untouched (manifests gate runs first)");

        // Real claim → Ok; status flipped, OUR signatures landed.
        complete_manifest_chunked(&db.pool, &info, claim_a, &[], None)
            .await
            .unwrap();
        let (status, sigs): (String, Vec<String>) = sqlx::query_as(
            "SELECT m.status::text, n.signatures \
             FROM manifests m JOIN narinfo n USING (store_path_hash) \
             WHERE m.store_path_hash = $1",
        )
        .bind(&store_path_hash)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(status, "complete");
        assert_eq!(sigs, vec!["good:sig".to_string()]);
    }

    // =======================================================================
    // metadata_status gRPC mapping — verified via the actual mapper
    // =======================================================================

    /// Verifies the grpc.rs metadata_status function produces the right
    /// codes. Not just the From<sqlx::Error> classification — the
    /// full chain: sqlx error → MetadataError variant → tonic::Code.
    #[test]
    fn grpc_status_code_mapping() {
        use crate::grpc::metadata_status;
        use tonic::Code;

        let cases: &[(MetadataError, Code)] = &[
            (MetadataError::NotFound, Code::NotFound),
            (MetadataError::Conflict("dup".into()), Code::AlreadyExists),
            (
                MetadataError::Connection(sqlx::Error::PoolClosed),
                Code::Unavailable,
            ),
            (MetadataError::Serialization, Code::Aborted),
            (
                MetadataError::Deadlock(sqlx::Error::PoolClosed),
                Code::Aborted,
            ),
            (
                MetadataError::PlaceholderMissing {
                    store_path: "/nix/store/x".into(),
                },
                Code::Aborted,
            ),
            (
                MetadataError::InvariantViolation("x".into()),
                Code::Internal,
            ),
            (
                MetadataError::Other(sqlx::Error::RowNotFound),
                Code::Internal,
            ),
            (
                MetadataError::CorruptManifest {
                    store_path: "/nix/store/x".into(),
                    source: crate::manifest::ManifestError::Empty,
                },
                Code::DataLoss,
            ),
            (
                MetadataError::MalformedRow(
                    rio_proto::validated::PathInfoValidationError::StorePath {
                        path: "bad".into(),
                        source: rio_nix::store_path::StorePathError::TooShort,
                    },
                ),
                Code::Internal,
            ),
            (
                MetadataError::ResourceExhausted("quota".into()),
                Code::ResourceExhausted,
            ),
            (
                MetadataError::RealisationConflict {
                    drv_hash: "ab".into(),
                    output_name: "out".into(),
                    existing: "/nix/store/a".into(),
                    attempted: "/nix/store/b".into(),
                },
                Code::AlreadyExists,
            ),
        ];
        for (err, expected_code) in cases {
            // MetadataError isn't Clone; reconstruct for the call.
            let code = metadata_status("test", clone_for_test(err)).code();
            assert_eq!(
                code, *expected_code,
                "wrong code for {err:?}: got {code:?}, expected {expected_code:?}"
            );
        }
    }

    /// upgrade_manifest_to_chunked's ON CONFLICT upsert must clear
    /// `deleted=false` when resurrecting a chunk. Without this,
    /// PutPath bumps refcount but leaves deleted=true → chunks row
    /// is inconsistent (refcount>0 but marked deleted). The drain
    /// re-check catches it either way, but self-consistent row state
    /// makes the chunks table correct on its own.
    #[tokio::test]
    async fn integration_chunked_upsert_clears_deleted() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed a "sweep just marked me dead" chunk: refcount=0,
        // deleted=true.
        let chunk_hash = vec![0xEEu8; 32];
        sqlx::query(
            "INSERT INTO chunks (blake3_hash, refcount, size, deleted) \
             VALUES ($1, 0, 100, true)",
        )
        .bind(&chunk_hash)
        .execute(&db.pool)
        .await
        .unwrap();

        // Set up placeholder for upgrade_manifest_to_chunked (requires
        // existing 'uploading' manifests row, which requires narinfo).
        let store_path_hash = vec![0xDDu8; 32];
        insert_manifest_uploading(&db.pool, &store_path_hash, "/nix/store/d-dummy", &[])
            .await
            .unwrap();

        // Upgrade with a chunk_list referencing our dead chunk.
        // Minimal Manifest: one entry. The upsert should bump
        // refcount 0→1 AND clear deleted→false.
        let manifest = crate::manifest::Manifest {
            entries: vec![crate::manifest::ManifestEntry {
                hash: [0xEEu8; 32],
                size: 100,
            }],
        };
        let _ = chunked::upgrade_manifest_to_chunked(
            &db.pool,
            &store_path_hash,
            &manifest.serialize(),
            std::slice::from_ref(&chunk_hash),
            &[100i64],
        )
        .await
        .unwrap();

        // Verify: refcount=1, deleted=false. refcount is PG INTEGER → i32.
        let (refcount, deleted): (i32, bool) =
            sqlx::query_as("SELECT refcount, deleted FROM chunks WHERE blake3_hash = $1")
                .bind(&chunk_hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(refcount, 1, "upsert bumped refcount");
        assert!(!deleted, "upsert cleared deleted=false (chunk resurrected)");
    }

    /// Test-only shallow clone. MetadataError can't derive Clone (holds
    /// sqlx::Error which isn't Clone); this reconstructs equivalent
    /// variants for the mapping test above.
    fn clone_for_test(e: &MetadataError) -> MetadataError {
        match e {
            MetadataError::NotFound => MetadataError::NotFound,
            MetadataError::Conflict(s) => MetadataError::Conflict(s.clone()),
            MetadataError::Connection(_) => MetadataError::Connection(sqlx::Error::PoolClosed),
            MetadataError::Serialization => MetadataError::Serialization,
            MetadataError::Deadlock(_) => MetadataError::Deadlock(sqlx::Error::PoolClosed),
            MetadataError::PlaceholderMissing { store_path } => MetadataError::PlaceholderMissing {
                store_path: store_path.clone(),
            },
            MetadataError::InvariantViolation(s) => MetadataError::InvariantViolation(s.clone()),
            MetadataError::CorruptManifest { store_path, .. } => MetadataError::CorruptManifest {
                store_path: store_path.clone(),
                source: crate::manifest::ManifestError::Empty,
            },
            MetadataError::MalformedRow(_) => MetadataError::MalformedRow(
                rio_proto::validated::PathInfoValidationError::StorePath {
                    path: "x".into(),
                    source: rio_nix::store_path::StorePathError::TooShort,
                },
            ),
            MetadataError::ResourceExhausted(s) => MetadataError::ResourceExhausted(s.clone()),
            MetadataError::Other(_) => MetadataError::Other(sqlx::Error::RowNotFound),
            MetadataError::RealisationConflict {
                drv_hash,
                output_name,
                existing,
                attempted,
            } => MetadataError::RealisationConflict {
                drv_hash: drv_hash.clone(),
                output_name: output_name.clone(),
                existing: existing.clone(),
                attempted: attempted.clone(),
            },
        }
    }
}
