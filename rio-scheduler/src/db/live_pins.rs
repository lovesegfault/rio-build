//! scheduler_live_pins — auto-pin live-build input closure.
//!
//! Scheduler+store share PG (same migrations/ dir). Scheduler writes
//! directly to scheduler_live_pins; store's gc/mark.rs seeds from it.
//! Best-effort: PG failure during pin/unpin logs + continues (24h grace
//! period is the fallback safety net).
// r[impl sched.gc.live-pins]

use uuid::Uuid;

use super::{SchedulerDb, terminal_status_sql};
use crate::state::DrvHash;

impl SchedulerDb {
    /// Pin a batch of store paths as live-build inputs for a drv.
    /// SHA-256 each path for store_path_hash (matches narinfo keying).
    /// ON CONFLICT DO NOTHING: re-pin is idempotent.
    pub async fn pin_live_inputs(
        &self,
        drv_hash: &DrvHash,
        store_paths: &[String],
    ) -> Result<(), sqlx::Error> {
        if store_paths.is_empty() {
            return Ok(());
        }
        use sha2::Digest;
        let hashes: Vec<Vec<u8>> = store_paths
            .iter()
            .map(|p| sha2::Sha256::digest(p.as_bytes()).to_vec())
            .collect();

        // Batch INSERT via UNNEST. Arrays are parallel (same length
        // by construction: same source vec). ON CONFLICT DO NOTHING
        // for idempotence — re-dispatching a drv (after reassign)
        // shouldn't error.
        //
        // `query!` (not runtime `query`): compile-checks the column
        // list against `rio_migrations::schema::LivePin` — store reads
        // these columns in gc/mark.rs + gc/sweep.rs.
        let drv_hashes = vec![drv_hash.as_str(); hashes.len()];
        sqlx::query!(
            r#"
            INSERT INTO scheduler_live_pins (store_path_hash, drv_hash)
            SELECT * FROM UNNEST($1::bytea[], $2::text[])
            ON CONFLICT DO NOTHING
            "#,
            &hashes,
            &drv_hashes as &[&str],
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Pin each derivation's own `.drv` path under its own drv_hash —
    /// the ADR-024 drv-blob GC root (`r[store.drv.gc-build-pinned]`):
    /// the store's drv sweep joins `drv_blobs.drv_path_hash =
    /// sha256(drv_path)` against these rows, so a drv blob referenced
    /// by a live build survives GC from submission accept until the
    /// derivation's terminal status unpins it (the same per-drv
    /// lifecycle as the dispatch-time input pins — one `DELETE ...
    /// WHERE drv_hash` clears both).
    ///
    /// Batch via UNNEST: one round-trip per merge, not per node. Same
    /// idempotence as [`pin_live_inputs`]: ON CONFLICT DO NOTHING.
    ///
    /// [`pin_live_inputs`]: Self::pin_live_inputs
    pub async fn pin_drv_paths(&self, pairs: &[(&str, &str)]) -> Result<u64, sqlx::Error> {
        if pairs.is_empty() {
            return Ok(0);
        }
        use sha2::Digest;
        let hashes: Vec<Vec<u8>> = pairs
            .iter()
            .map(|(_, path)| sha2::Sha256::digest(path.as_bytes()).to_vec())
            .collect();
        let drv_hashes: Vec<&str> = pairs.iter().map(|(h, _)| *h).collect();
        let result = sqlx::query!(
            r#"
            INSERT INTO scheduler_live_pins (store_path_hash, drv_hash)
            SELECT * FROM UNNEST($1::bytea[], $2::text[])
            ON CONFLICT DO NOTHING
            "#,
            &hashes,
            &drv_hashes as &[&str],
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Upsert the (output_path × tenant_id) cartesian product into
    /// path_tenants. SHA-256 each path (matches narinfo.store_path_hash
    /// keying — same as `pin_live_inputs`). ON CONFLICT DO NOTHING on
    /// the composite PK (store_path_hash, tenant_id): repeated builds
    /// of the same path by the same tenant are idempotent.
    ///
    /// Best-effort: caller warns on Err but does NOT fail completion.
    /// GC may under-retain a path if this upsert fails, but the build
    /// still succeeds (24h global grace is the fallback).
    ///
    /// Returns `rows_affected()` so callers/tests can assert on the
    /// delta (0 on re-call = idempotence proof).
    pub async fn upsert_path_tenants(
        &self,
        output_paths: &[String],
        tenant_ids: &[Uuid],
    ) -> Result<u64, sqlx::Error> {
        if output_paths.is_empty() || tenant_ids.is_empty() {
            return Ok(0);
        }
        use sha2::Digest;
        // Cartesian product: every path × every tenant. Parallel arrays
        // for UNNEST (same length by construction).
        let n = output_paths.len() * tenant_ids.len();
        let mut hashes: Vec<Vec<u8>> = Vec::with_capacity(n);
        let mut tids: Vec<Uuid> = Vec::with_capacity(n);
        for p in output_paths {
            let h = sha2::Sha256::digest(p.as_bytes()).to_vec();
            for t in tenant_ids {
                hashes.push(h.clone());
                tids.push(*t);
            }
        }
        self.upsert_path_tenants_raw(&hashes, &tids).await
    }

    /// Pre-flattened variant of [`upsert_path_tenants`]: caller has
    /// already built the parallel `(store_path_hash, tenant_id)` arrays
    /// (no cartesian product applied here). Used by the batched
    /// merge-time path where each drv may have a different tenant set,
    /// so the caller flattens across drvs and issues ONE round-trip
    /// instead of N. Same UNNEST + `ON CONFLICT DO NOTHING` semantics.
    ///
    /// [`upsert_path_tenants`]: Self::upsert_path_tenants
    pub async fn upsert_path_tenants_raw(
        &self,
        hashes: &[Vec<u8>],
        tids: &[Uuid],
    ) -> Result<u64, sqlx::Error> {
        debug_assert_eq!(hashes.len(), tids.len());
        if hashes.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            r#"
            INSERT INTO path_tenants (store_path_hash, tenant_id)
            SELECT * FROM UNNEST($1::bytea[], $2::uuid[])
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(hashes)
        .bind(tids)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Unpin all live inputs for a drv. Called on terminal status.
    /// Idempotent: unpinning a never-pinned drv = 0 rows deleted.
    pub async fn unpin_live_inputs(&self, drv_hash: &DrvHash) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "DELETE FROM scheduler_live_pins WHERE drv_hash = $1",
            drv_hash.as_str(),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Batch variant of [`unpin_live_inputs`]: delete pins for many
    /// derivations in one round-trip. Paired with
    /// [`update_derivation_status_batch`] for the cancel-build path
    /// where N sequential unpins stalled the actor.
    ///
    /// [`unpin_live_inputs`]: Self::unpin_live_inputs
    /// [`update_derivation_status_batch`]: Self::update_derivation_status_batch
    pub async fn unpin_live_inputs_batch(&self, drv_hashes: &[&str]) -> Result<u64, sqlx::Error> {
        if drv_hashes.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query!(
            "DELETE FROM scheduler_live_pins WHERE drv_hash = ANY($1::text[])",
            drv_hashes as &[&str],
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Sweep stale pins: delete rows for derivations that are no
    /// longer in non-terminal state. Called after recovery (handles
    /// crash-between-pin-and-unpin — scheduler crashed after pin at
    /// dispatch but before unpin at completion).
    ///
    /// The subquery matches `load_nonterminal_derivations`' filter
    /// (both splice `terminal_status_sql!`): a drv NOT in that
    /// set is terminal (or deleted entirely).
    pub async fn sweep_stale_live_pins(&self) -> Result<u64, sqlx::Error> {
        // Compile-time splice of the terminal-status tuple — see
        // terminal_status_sql! for why it isn't a bind param.
        let result = sqlx::query(terminal_status_sql!(
            r"
            DELETE FROM scheduler_live_pins
             WHERE drv_hash NOT IN (
               SELECT drv_hash FROM derivations
                WHERE status NOT IN ",
            r"
             )
            "
        ))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
