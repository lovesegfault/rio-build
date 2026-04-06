//! scheduler_live_pins — auto-pin live-build input closure.
//!
//! Scheduler+store share PG (same migrations/ dir). Scheduler writes
//! directly to scheduler_live_pins; store's gc/mark.rs seeds from it.
//! Best-effort: PG failure during pin/unpin logs + continues (24h grace
//! period is the fallback safety net).

use uuid::Uuid;

use super::{SchedulerDb, TERMINAL_STATUS_SQL};
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
        sqlx::query(
            r#"
            INSERT INTO scheduler_live_pins (store_path_hash, drv_hash)
            SELECT * FROM UNNEST($1::bytea[], $2::text[])
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(&hashes)
        .bind(vec![drv_hash.as_str(); hashes.len()])
        .execute(&self.pool)
        .await?;
        Ok(())
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
        let result = sqlx::query(
            r#"
            INSERT INTO path_tenants (store_path_hash, tenant_id)
            SELECT * FROM UNNEST($1::bytea[], $2::uuid[])
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(&hashes)
        .bind(&tids)
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
    /// (both interpolate `TERMINAL_STATUS_SQL`): a drv NOT in that
    /// set is terminal (or deleted entirely).
    pub async fn sweep_stale_live_pins(&self) -> Result<u64, sqlx::Error> {
        // format! of a compile-time const — no injection surface.
        // See TERMINAL_STATUS_SQL doc for why this isn't a bind param.
        let result = sqlx::query(&format!(
            r"
            DELETE FROM scheduler_live_pins
             WHERE drv_hash NOT IN (
               SELECT drv_hash FROM derivations
                WHERE status NOT IN {TERMINAL_STATUS_SQL}
             )
            "
        ))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
