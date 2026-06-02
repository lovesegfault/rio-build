//! Durable per-(build, derivation) wanted relation — the
//! substitution-replacement campaign's AW4 successor (design §6,
//! migration 078 `build_wanted_outputs`).
//!
//! Every standalone write is claims-floor fenced (the Phase-1 Wave-3
//! pattern from `db/batch.rs`, reusing [`SchedulerDb::claims_floor`] /
//! [`SchedulerDb::at_or_above_floor`] verbatim): a deposed leader's
//! writes are discarded, never applied. The in-tx form rides the
//! caller's transaction (the merge tx), which already carries the
//! merge fence — one fence per transaction, no second floor read.
//!
//! The effective-wanted union is computed by ONE query helper here and
//! nowhere else — the '{}'-means-all saturation convention (the 062
//! convention) lives in exactly one place.
// r[impl sched.materialize.job]

use sqlx::PgConnection;
use uuid::Uuid;

use super::{FencedWrite, SchedulerDb, encode_pg_text_array};

/// One (build, derivation) wanted contribution, as the merge records it.
pub(crate) struct WantedRow<'a> {
    pub build_id: Uuid,
    pub derivation_id: Uuid,
    /// Empty slice = all declared outputs wanted (the 062 convention).
    pub wanted_output_names: &'a [String],
}

impl SchedulerDb {
    /// Record/replace builds' wanted contributions for a batch of
    /// derivations, inside the caller's transaction (the merge tx).
    /// `ON CONFLICT (build_id, derivation_id) DO UPDATE` —
    /// last-write-wins per build; never touches another build's rows
    /// (PK isolation). The caller's transaction carries the merge
    /// fence; this helper adds no second floor read (one fence per
    /// transaction — the merge-tx discipline from Phase-1 Wave 3).
    pub(crate) async fn record_wanted_in_tx(
        tx: &mut PgConnection,
        rows: &[WantedRow<'_>],
    ) -> Result<(), sqlx::Error> {
        if rows.is_empty() {
            return Ok(());
        }
        // Batched UNNEST upsert (the batch_upsert_derivations shape).
        // The nested wanted_output_names arrays can't unnest as
        // text[][] (PG multidim arrays are rectangular), so each is
        // encoded as a pg text-array literal and cast back in the
        // SELECT — same convention as db/batch.rs.
        let build_ids: Vec<Uuid> = rows.iter().map(|r| r.build_id).collect();
        let drv_ids: Vec<Uuid> = rows.iter().map(|r| r.derivation_id).collect();
        let wanted: Vec<String> = rows
            .iter()
            .map(|r| encode_pg_text_array(r.wanted_output_names))
            .collect();
        sqlx::query(
            "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
             SELECT b, d, w::text[] \
               FROM UNNEST($1::uuid[], $2::uuid[], $3::text[]) AS t(b, d, w) \
             ON CONFLICT (build_id, derivation_id) DO UPDATE \
                 SET wanted_output_names = EXCLUDED.wanted_output_names, \
                     recorded_at = now()",
        )
        .bind(&build_ids)
        .bind(&drv_ids)
        .bind(&wanted)
        .execute(&mut *tx)
        .await?;
        Ok(())
    }

    /// Standalone fenced form (for callers outside the merge tx — the
    /// Phase A actor smoke test and Phase B's reprobe-lane creation).
    /// Returns [`FencedWrite::Fenced`] (nothing written) when
    /// `serving_generation` is below the durable claims floor.
    pub(crate) async fn record_wanted_fenced(
        &self,
        serving_generation: i64,
        rows: &[WantedRow<'_>],
    ) -> Result<FencedWrite, sqlx::Error> {
        if rows.is_empty() {
            return Ok(FencedWrite::Applied(0));
        }
        let mut tx = self.pool.begin().await?;
        let floor = Self::claims_floor(&mut tx).await?;
        if !Self::at_or_above_floor(floor, serving_generation) {
            tx.rollback().await?;
            return Ok(FencedWrite::Fenced);
        }
        Self::record_wanted_in_tx(&mut tx, rows).await?;
        tx.commit().await?;
        Ok(FencedWrite::Applied(rows.len() as u64))
    }

    /// THE effective-wanted query (design §6: computed by the one query
    /// helper; the saturation convention's single home). Returns `None`
    /// when no live build has a contribution (B4: never a vacuous
    /// verdict), `Some(vec![])` when any live contribution is '{}'
    /// (= all declared outputs wanted), `Some(union)` otherwise.
    pub(crate) async fn effective_wanted_union(
        &self,
        derivation_id: Uuid,
    ) -> Result<Option<Vec<String>>, sqlx::Error> {
        let rows: Vec<(Vec<String>,)> = sqlx::query_as(
            "SELECT w.wanted_output_names \
               FROM build_wanted_outputs w \
               JOIN builds b ON b.build_id = w.build_id \
              WHERE w.derivation_id = $1 \
                AND b.status IN ('pending', 'active')",
        )
        .bind(derivation_id)
        .fetch_all(&self.pool)
        .await?;
        if rows.is_empty() {
            return Ok(None);
        }
        // Saturating union: any '{}' contribution saturates to "all".
        let mut union: Vec<String> = Vec::new();
        for (names,) in rows {
            if names.is_empty() {
                return Ok(Some(Vec::new()));
            }
            for n in names {
                if !union.contains(&n) {
                    union.push(n);
                }
            }
        }
        Ok(Some(union))
    }

    /// Gap-filling backfill write (T-D2.3 step 5 — the B4 backfill's
    /// row source): INSERT the saturating `'{}'` (all-declared) row for
    /// (build, derivation) pairs that have NO row yet; `ON CONFLICT DO
    /// NOTHING` — a build that merged flag-on already has its EXACT
    /// row, and the backfill must never widen it (the relation's exact
    /// rows are the AW4 fix; the backfill only fills legacy gaps).
    pub(crate) async fn backfill_wanted_fenced(
        &self,
        serving_generation: i64,
        build_id: Uuid,
        derivation_id: Uuid,
    ) -> Result<FencedWrite, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let floor = Self::claims_floor(&mut tx).await?;
        if !Self::at_or_above_floor(floor, serving_generation) {
            tx.rollback().await?;
            return Ok(FencedWrite::Fenced);
        }
        let n = sqlx::query(
            "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
             VALUES ($1, $2, '{}') \
             ON CONFLICT (build_id, derivation_id) DO NOTHING",
        )
        .bind(build_id)
        .bind(derivation_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(FencedWrite::Applied(n))
    }

    /// Recovery wanted-cache rebuild load (T-D2.3/PD-D5): every
    /// (build, derivation, wanted) contribution row belonging to a
    /// LIVE (`pending`/`active`) build. Feeds `wanted_by_build` at
    /// recovery so the in-memory union is the EXACT live union — the
    /// relation has exactly the per-build rows (written by every
    /// flag-on merge since Phase B; the B4 backfill covers probe-era
    /// gaps). A live build with NO rows here is the legacy shape: the
    /// conservative-absent arm saturates it to all-declared width.
    pub(crate) async fn load_wanted_for_live_builds(
        &self,
    ) -> Result<Vec<(Uuid, Uuid, Vec<String>)>, sqlx::Error> {
        sqlx::query_as(
            "SELECT w.build_id, w.derivation_id, w.wanted_output_names \
               FROM build_wanted_outputs w \
               JOIN builds b ON b.build_id = w.build_id \
              WHERE b.status IN ('pending', 'active')",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Purge a build's contributions (called with the build row's own
    /// purge — existing lifecycle; in Phase A only tests call it).
    pub(crate) async fn delete_wanted_for_build(&self, build_id: Uuid) -> Result<u64, sqlx::Error> {
        Ok(
            sqlx::query("DELETE FROM build_wanted_outputs WHERE build_id = $1")
                .bind(build_id)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }
}
