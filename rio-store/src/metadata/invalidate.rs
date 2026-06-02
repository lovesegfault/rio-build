//! Admin invalidation of a store path's cache metadata.
//!
//! Backs `StoreAdminService.InvalidatePath` — the operator remediation
//! for the "wrong-success" incident class (an output uploaded, signed,
//! and cached with wrong content). Deleting the metadata rows is what
//! makes the next submission miss the cache and re-execute. Chunk
//! *bytes* are untouched, but chunk **refcounts are decremented here**,
//! exactly as the GC sweep would have done for this path: the manifest's
//! `chunk_list` is the only record of which chunks carry this path's
//! +1, and the narinfo CASCADE destroys it — skipping the decrement
//! would leak unique chunks forever (the orphan sweep only reaps
//! `refcount = 0`) and permanently over-count shared ones.
//!
//! The deletion set is a declared SUPERSET of the GC sweep's
//! `delete_swept_path` (`gc/sweep.rs`): realisations have no FK to
//! narinfo and must be deleted explicitly, as must `path_tenants`
//! (orphaned rows would grant stale tenant visibility on a later
//! re-upload of the same path); `manifests` / `manifest_data` /
//! `content_index` follow the narinfo row via `ON DELETE CASCADE`.
//! Beyond the sweep, this also removes `realisation_deps` junction
//! rows touching the deleted realisations in either role — their FKs
//! are `ON DELETE RESTRICT`, and an explicit operator invalidation is
//! precisely the case where removing the edges is intended rather than
//! a bug to surface — and `drv_modulo_cache`, which the sweep
//! deliberately PRESERVES (`store.put.ia-deriver-proof+3`: proofs
//! survive deriver GC) but which "invalidate everything about this
//! path" must purge: a surviving modulo row would keep proving IA
//! outputs of a `.drv` whose narinfo the operator just removed.

use std::sync::Arc;

use sqlx::PgPool;
use tracing::info;

use rio_nix::store_path::StorePath;

use crate::backend::ChunkBackend;

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
    /// Chunks whose refcount hit 0 in the decrement step (marked
    /// deleted + enqueued for S3 delete). Log/observability only —
    /// not part of the RPC response.
    pub chunks_zeroed: u64,
    /// `drv_modulo_cache` rows purged. The sweep preserves these; the
    /// operator purge is their only path-scoped deletion.
    pub drv_modulo_deleted: u64,
}

impl InvalidateCounts {
    pub(crate) fn found(&self) -> bool {
        // An orphan-only modulo row still counts: the operator must see
        // that the purge took effect (store.admin.invalidate-total).
        self.narinfo_deleted > 0 || self.realisations_deleted > 0 || self.drv_modulo_deleted > 0
    }
}

/// Delete the metadata that makes `store_path` a cache hit.
///
/// One transaction; idempotent (an absent path returns all-zero
/// counts). `keep_realisations` skips the realisations /
/// realisation_deps deletions (e.g. when the operator wants to
/// re-upload corrected content under the same realised path rather
/// than force a rebuild).
///
/// `chunk_backend` is only used to enqueue S3 keys for chunks whose
/// refcount reaches 0 (same role as in the GC sweep); `None` means an
/// inline-only store where there is nothing to enqueue.
pub(crate) async fn invalidate_path(
    pool: &PgPool,
    store_path: &StorePath,
    keep_realisations: bool,
    chunk_backend: Option<&Arc<dyn ChunkBackend>>,
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

    // r[impl store.admin.invalidate-total]
    // drv_modulo_cache keys on `drv_path_hash = sha256(store_path)` —
    // the SAME digest as `path_hash` above (StorePath::sha256_digest is
    // sha256 over the full path string; populate_on_ingest hashes the
    // identical string). Unconditional: for non-.drv paths the row
    // simply never exists and this is a no-op.
    counts.drv_modulo_deleted =
        sqlx::query("DELETE FROM drv_modulo_cache WHERE drv_path_hash = $1")
            .bind(&path_hash)
            .execute(&mut *tx)
            .await?
            .rows_affected();

    // Read the manifest's chunk_list BEFORE the narinfo delete: the
    // CASCADE destroys the only record of which chunks carry this
    // path's refcount. `FOR UPDATE OF m` is the same locking
    // discipline as the GC sweep / orphan reap — a concurrent PutPath
    // for the SAME path blocks until this transaction commits, so the
    // decrement and the delete are atomic with respect to re-uploads.
    // Outer Option = manifest row existence (observable only before
    // the CASCADE); inner Option = chunk_list (NULL for inline NARs).
    let manifest_row: Option<Option<Vec<u8>>> = sqlx::query_scalar(
        r#"
        SELECT md.chunk_list
          FROM manifests m
          LEFT JOIN manifest_data md USING (store_path_hash)
         WHERE m.store_path_hash = $1
           FOR UPDATE OF m
        "#,
    )
    .bind(&path_hash)
    .fetch_optional(&mut *tx)
    .await?;
    counts.manifest_existed = u64::from(manifest_row.is_some());

    // Decrement chunk refcounts exactly as the GC sweep would have for
    // this path; chunks that hit 0 are marked deleted and their S3
    // keys enqueued to pending_s3_deletes (drain owns the actual
    // object deletion). Inline manifests have no chunk_list → no-op.
    if let Some(Some(chunk_list)) = &manifest_row {
        let stats = crate::gc::decrement_and_enqueue(&mut tx, chunk_list, chunk_backend).await?;
        counts.chunks_zeroed = stats.chunks_zeroed;
    }

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
        chunks_zeroed = counts.chunks_zeroed,
        drv_modulo = counts.drv_modulo_deleted,
        keep_realisations,
        "invalidated store path metadata"
    );

    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Manifest, ManifestEntry};
    use crate::test_helpers::{ChunkSeed, StoreSeed, mem_backend, path_hash};
    use rio_test_support::TestDb;
    use rio_test_support::fixtures::test_store_path;

    /// Invalidating a chunked path must decrement chunk refcounts the
    /// same way the GC sweep does: a chunk unique to the invalidated
    /// content reaches refcount=0 (deleted + enqueued for S3 delete),
    /// a chunk shared with another live path keeps exactly one
    /// reference, and a second invalidate of the now-absent path is a
    /// no-op.
    #[tokio::test]
    async fn invalidate_decrements_chunk_refcounts_like_the_sweep() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let path = test_store_path("invalidate-chunked");
        let sp_hash = path_hash(&path);

        // One chunk only this path references, one shared with another
        // (still live) path.
        let unique = ChunkSeed::new(0xC1)
            .with_refcount(1)
            .with_size(100)
            .seed(&db.pool)
            .await;
        let shared = ChunkSeed::new(0xC2)
            .with_refcount(2)
            .with_size(200)
            .seed(&db.pool)
            .await;

        let seeded = StoreSeed::raw_path(&path).seed(&db.pool).await;
        assert_eq!(
            seeded, sp_hash,
            "StoreSeed and StorePath must agree on the path hash"
        );
        let chunk_list = Manifest {
            entries: vec![
                ManifestEntry {
                    hash: unique,
                    size: 100,
                },
                ManifestEntry {
                    hash: shared,
                    size: 200,
                },
            ],
        }
        .serialize();
        sqlx::query("INSERT INTO manifest_data (store_path_hash, chunk_list) VALUES ($1, $2)")
            .bind(&sp_hash)
            .bind(&chunk_list)
            .execute(&db.pool)
            .await
            .unwrap();

        let backend: Arc<dyn ChunkBackend> = mem_backend();
        let sp = StorePath::parse(&path).unwrap();
        let counts = invalidate_path(&db.pool, &sp, false, Some(&backend))
            .await
            .unwrap();
        assert_eq!(counts.narinfo_deleted, 1);
        assert_eq!(counts.manifest_existed, 1);
        assert_eq!(counts.chunks_zeroed, 1, "only the unique chunk hits zero");

        let (rc, deleted): (i32, bool) =
            sqlx::query_as("SELECT refcount, deleted FROM chunks WHERE blake3_hash = $1")
                .bind(unique.as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            (rc, deleted),
            (0, true),
            "unique chunk zeroed and marked deleted"
        );
        let (rc, deleted): (i32, bool) =
            sqlx::query_as("SELECT refcount, deleted FROM chunks WHERE blake3_hash = $1")
                .bind(shared.as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            (rc, deleted),
            (1, false),
            "shared chunk keeps the other path's reference"
        );
        let enqueued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(enqueued, 1, "only the zeroed chunk's S3 key is enqueued");

        // Idempotent: nothing left to find or decrement.
        let counts = invalidate_path(&db.pool, &sp, false, Some(&backend))
            .await
            .unwrap();
        assert!(!counts.found());
        assert_eq!(counts.chunks_zeroed, 0);
        let (rc,): (i32,) = sqlx::query_as("SELECT refcount FROM chunks WHERE blake3_hash = $1")
            .bind(shared.as_slice())
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(rc, 1, "second invalidate must not double-decrement");
    }

    // r[verify store.admin.invalidate-total]
    /// Invalidating a `.drv` path purges its `drv_modulo_cache` row in
    /// the same transaction (the GC sweep deliberately preserves these,
    /// so the operator purge is their only path-scoped deletion), stays
    /// idempotent, and an ORPHAN-only cache row (no narinfo at all)
    /// still reports `found = true` so the operator sees the purge took
    /// effect.
    #[tokio::test]
    async fn invalidate_purges_drv_modulo_cache_and_reports_orphans() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let drv_path = test_store_path("invalidate-me.drv");
        let sp = StorePath::parse(&drv_path).unwrap();

        let seed_row = |pool: sqlx::PgPool, path: String| async move {
            let hash: Vec<u8> = {
                use sha2::Digest as _;
                sha2::Sha256::digest(path.as_bytes()).to_vec()
            };
            sqlx::query(
                "INSERT INTO drv_modulo_cache \
                 (drv_path_hash, drv_path, modulo_hash, ia_output_paths, deferred) \
                 VALUES ($1, $2, $3, '[]'::jsonb, FALSE) \
                 ON CONFLICT (drv_path_hash) DO NOTHING",
            )
            .bind(&hash)
            .bind(&path)
            .bind([0u8; 32].as_slice())
            .execute(&pool)
            .await
            .unwrap();
        };

        // (a) Resident narinfo + cache row: both purged, counts split.
        StoreSeed::raw_path(&drv_path).seed(&db.pool).await;
        seed_row(db.pool.clone(), drv_path.clone()).await;
        let counts = invalidate_path(&db.pool, &sp, false, None).await.unwrap();
        assert!(counts.found());
        assert_eq!(counts.narinfo_deleted, 1);
        assert_eq!(counts.drv_modulo_deleted, 1, "modulo row purged");

        // (b) Idempotent: second call finds nothing.
        let counts = invalidate_path(&db.pool, &sp, false, None).await.unwrap();
        assert!(!counts.found());
        assert_eq!(counts.drv_modulo_deleted, 0);

        // (c) Orphan-only: cache row without any narinfo (e.g. the
        // narinfo was swept while the proof row survived by design).
        seed_row(db.pool.clone(), drv_path.clone()).await;
        let counts = invalidate_path(&db.pool, &sp, false, None).await.unwrap();
        assert!(
            counts.found(),
            "orphan-only modulo purge must report found=true"
        );
        assert_eq!(counts.narinfo_deleted, 0);
        assert_eq!(counts.drv_modulo_deleted, 1);
    }

    /// Inline (non-chunked) paths have no chunk_list: the decrement
    /// step is a no-op and the delete still works.
    #[tokio::test]
    async fn invalidate_inline_path_skips_chunk_decrement() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let path = test_store_path("invalidate-inline");
        StoreSeed::raw_path(&path)
            .with_inline_blob(b"inline bytes".to_vec())
            .seed(&db.pool)
            .await;

        let sp = StorePath::parse(&path).unwrap();
        let counts = invalidate_path(&db.pool, &sp, false, None).await.unwrap();
        assert_eq!(counts.narinfo_deleted, 1);
        assert_eq!(counts.manifest_existed, 1);
        assert_eq!(counts.chunks_zeroed, 0);
    }
}
