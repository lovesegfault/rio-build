//! Admin invalidation of a store path's cache metadata.
//!
//! Backs `StoreAdminService.InvalidatePath` — the operator remediation
//! for the "wrong-success" incident class (an output uploaded, signed,
//! and cached with wrong content). Deleting the metadata rows is what
//! makes the next submission miss the cache and re-execute; chunk data
//! is deliberately untouched (GC owns chunk lifecycle and reclaims
//! orphans on its normal sweep).
//!
//! The deletion set mirrors the GC sweep's `delete_swept_path`
//! (`gc/sweep.rs`): realisations have no FK to narinfo and must be
//! deleted explicitly, as must `path_tenants` (orphaned rows would
//! grant stale tenant visibility on a later re-upload of the same
//! path); `manifests` / `manifest_data` / `content_index` follow the
//! narinfo row via `ON DELETE CASCADE`. Beyond the sweep, this also
//! removes `realisation_deps` junction rows touching the deleted
//! realisations in either role — their FKs are `ON DELETE RESTRICT`,
//! and an explicit operator invalidation is precisely the case where
//! removing the edges is intended rather than a bug to surface.

use sqlx::PgPool;
use tracing::info;

use rio_nix::store_path::StorePath;

use super::Result;

/// Per-table row counts from an [`invalidate_path`] call. All zero =
/// the path was not present anywhere (the call is idempotent).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct InvalidateCounts {
    pub narinfo_deleted: u64,
    /// 0/1 — manifests/manifest_data/content_index go via CASCADE, so
    /// only existence is observable.
    pub manifest_existed: u64,
    pub realisations_deleted: u64,
    pub realisation_deps_deleted: u64,
    pub path_tenants_deleted: u64,
}

impl InvalidateCounts {
    pub(crate) fn found(&self) -> bool {
        self.narinfo_deleted > 0 || self.realisations_deleted > 0
    }
}

/// Delete the metadata that makes `store_path` a cache hit.
///
/// One transaction; idempotent (an absent path returns all-zero
/// counts). `keep_realisations` skips the realisations /
/// realisation_deps deletions (e.g. when the operator wants to
/// re-upload corrected content under the same realised path rather
/// than force a rebuild).
pub(crate) async fn invalidate_path(
    pool: &PgPool,
    store_path: &StorePath,
    keep_realisations: bool,
) -> Result<InvalidateCounts> {
    let mut tx = pool.begin().await?;
    let mut counts = InvalidateCounts::default();
    let path_str = store_path.as_str();
    let path_hash = store_path.sha256_digest().to_vec();

    if !keep_realisations {
        // Junction rows first: their FKs are ON DELETE RESTRICT, so the
        // realisations delete below would otherwise fail whenever the
        // invalidated realisation participates in a dependency edge.
        counts.realisation_deps_deleted = sqlx::query(
            r#"
            DELETE FROM realisation_deps
             WHERE (drv_hash, output_name) IN (
                     SELECT drv_hash, output_name FROM realisations WHERE output_path = $1
                   )
                OR (dep_drv_hash, dep_output_name) IN (
                     SELECT drv_hash, output_name FROM realisations WHERE output_path = $1
                   )
            "#,
        )
        .bind(path_str)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        counts.realisations_deleted =
            sqlx::query("DELETE FROM realisations WHERE output_path = $1")
                .bind(path_str)
                .execute(&mut *tx)
                .await?
                .rows_affected();
    }

    counts.path_tenants_deleted =
        sqlx::query("DELETE FROM path_tenants WHERE store_path_hash = $1")
            .bind(&path_hash)
            .execute(&mut *tx)
            .await?
            .rows_affected();

    // Existence of the manifest is only observable BEFORE the narinfo
    // delete (CASCADE removes it without reporting).
    let manifest_existed: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM manifests WHERE store_path_hash = $1)")
            .bind(&path_hash)
            .fetch_one(&mut *tx)
            .await?;
    counts.manifest_existed = u64::from(manifest_existed);

    counts.narinfo_deleted = sqlx::query("DELETE FROM narinfo WHERE store_path_hash = $1")
        .bind(&path_hash)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    tx.commit().await?;

    info!(
        store_path = %store_path,
        narinfo = counts.narinfo_deleted,
        manifest = counts.manifest_existed,
        realisations = counts.realisations_deleted,
        realisation_deps = counts.realisation_deps_deleted,
        path_tenants = counts.path_tenants_deleted,
        keep_realisations,
        "invalidated store path metadata"
    );

    Ok(counts)
}
