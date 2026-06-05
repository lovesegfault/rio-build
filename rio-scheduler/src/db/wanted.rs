//! Durable per-(build, derivation) wanted relation — the
//! substitution-replacement campaign's AW4 successor (design §6,
//! migration 078 `build_wanted_outputs`).
//!
//! Every standalone write is claims-floor fenced (the Phase-1 Wave-3
//! pattern from `db/batch.rs`, reusing `SchedulerDb::claims_floor` /
//! `SchedulerDb::at_or_above_floor` verbatim): a deposed leader's
//! writes are discarded, never applied. The in-tx form rides the
//! caller's transaction (the merge tx), which already carries the
//! merge fence — one fence per transaction, no second floor read.
//!
//! The effective-wanted union is computed by ONE query helper here and
//! nowhere else — the '{}'-means-all saturation convention (the 062
//! convention) lives in exactly one place.
// r[impl sched.materialize.job+2]

use sqlx::PgConnection;
use uuid::Uuid;

use super::{FencedBegin, FencedOutcome, SchedulerDb, ServingGeneration, encode_pg_text_array};

/// One (build, derivation) wanted contribution, as the merge records it.
pub(crate) struct WantedRow<'a> {
    pub build_id: Uuid,
    pub derivation_id: Uuid,
    /// Empty slice = all declared outputs wanted (the 062 convention).
    pub wanted_output_names: &'a [String],
}

impl SchedulerDb {
    /// Record builds' wanted contributions for a batch of
    /// derivations, inside the caller's transaction (the merge tx).
    /// `ON CONFLICT (build_id, derivation_id) DO UPDATE` is the
    /// SATURATING UNION (merged_bug_176/059 — NOT last-write-wins: a
    /// narrower re-record must never shrink demand; either side '{}'
    /// saturates to all-declared; the stored row equals the kernel's
    /// `union_wanted_saturating` fold, pinned by the db/tests/wanted
    /// sequence battery). Never touches another build's rows (PK
    /// isolation). The caller's transaction carries the merge fence;
    /// this helper adds no second floor read (one fence per
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
        // Saturating UNION on conflict (merged_bug_176): the in-memory
        // DAG fold and the gateway dedup union per-build contributions
        // with `union_wanted_saturating`; the SQL row must agree or a
        // multi-root submission's second record drops the first root's
        // demand. Either side '{}' (= all declared) saturates; else
        // sorted distinct union.
        sqlx::query(
            "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
             SELECT b, d, w::text[] \
               FROM UNNEST($1::uuid[], $2::uuid[], $3::text[]) AS t(b, d, w) \
             ON CONFLICT (build_id, derivation_id) DO UPDATE \
                 SET wanted_output_names = CASE \
                         WHEN build_wanted_outputs.wanted_output_names = '{}'::text[] \
                           OR EXCLUDED.wanted_output_names = '{}'::text[] THEN '{}'::text[] \
                         ELSE ARRAY(SELECT DISTINCT x \
                                      FROM UNNEST(build_wanted_outputs.wanted_output_names \
                                                  || EXCLUDED.wanted_output_names) AS t(x) \
                                     ORDER BY x) \
                     END, \
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
    /// Returns [`FencedOutcome::Fenced`] (nothing written) when
    /// `serving_generation` is below the durable claims floor.
    /// Test-battery form (merged_bug_284 sweep): production wanted
    /// writes flow through [`Self::record_wanted_in_tx`] ABOVE (the
    /// union upsert in THIS file, called from the merge transaction —
    /// db/batch.rs explicitly disclaims owning wanted rows); this
    /// fenced singular IS the relation's specification and
    /// db/tests/wanted.rs pins it (merged_bug_059: the old pointer
    /// sent sibling sweeps to audit the wrong file).
    #[cfg(test)]
    pub(crate) async fn record_wanted_fenced(
        &self,
        serving_generation: ServingGeneration,
        rows: &[WantedRow<'_>],
    ) -> Result<FencedOutcome, sqlx::Error> {
        if rows.is_empty() {
            return Ok(FencedOutcome::Applied(0));
        }
        let mut tx = match self.begin_fenced(serving_generation).await? {
            FencedBegin::Fenced { .. } => return Ok(FencedOutcome::Fenced),
            FencedBegin::Open(ftx) => ftx,
        };
        Self::record_wanted_in_tx(tx.conn(), rows).await?;
        tx.commit().await?;
        Ok(FencedOutcome::Applied(rows.len() as u64))
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
        // 086: read through `live_wanted_interest` — interest derives
        // from build_derivations MEMBERSHIP, so a live build without a
        // wanted row contributes the saturating '{}' default
        // (merged_bug_176). `saturated_default` marks those rows so the
        // width saturation is observable.
        let rows: Vec<(Uuid, Vec<String>, bool)> = sqlx::query_as(
            "SELECT build_id, wanted_output_names, saturated_default \
               FROM live_wanted_interest \
              WHERE derivation_id = $1",
        )
        .bind(derivation_id)
        .fetch_all(&self.pool)
        .await?;
        if rows.is_empty() {
            return Ok(None);
        }
        // Saturating union: any '{}' contribution saturates to "all".
        // merged_bug_059: ALL rows are scanned before the saturated
        // answer is returned, so the DQ-2 saturation note is a
        // function of the row SET, not of unspecified PG heap order —
        // every legacy defaulted row is noted even when an explicit
        // '{}' row happens to be fetched first.
        let mut union: Vec<String> = Vec::new();
        let mut saturated = false;
        for (build_id, names, saturated_default) in rows {
            if names.is_empty() {
                if saturated_default {
                    crate::state::note_width_event(crate::state::WidthEvent::SaturatedToDeclared {
                        build_id,
                    });
                }
                saturated = true;
                continue;
            }
            if !saturated {
                for n in names {
                    if !union.contains(&n) {
                        union.push(n);
                    }
                }
            }
        }
        if saturated {
            return Ok(Some(Vec::new()));
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
        serving_generation: ServingGeneration,
        build_id: Uuid,
        derivation_id: Uuid,
    ) -> Result<FencedOutcome, sqlx::Error> {
        let mut tx = match self.begin_fenced(serving_generation).await? {
            FencedBegin::Fenced { .. } => return Ok(FencedOutcome::Fenced),
            FencedBegin::Open(ftx) => ftx,
        };
        let n = sqlx::query(
            "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
             VALUES ($1, $2, '{}') \
             ON CONFLICT (build_id, derivation_id) DO NOTHING",
        )
        .bind(build_id)
        .bind(derivation_id)
        .execute(tx.conn())
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(FencedOutcome::Applied(n))
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

    /// D1/A6 (merged_bug_163): build_wanted_outputs rows die with their
    /// build — terminal arm (the build finished past the horizon,
    /// 001_scheduler.sql `builds.finished_at`) UNION orphan arm (no
    /// builds row at all: pre-wiring failed-merge leaks). DELETE by PK
    /// (build_id, derivation_id); bounded anti-join per the accepted
    /// gc_attempt_ledger orphan-arm class. The per-build purge lives in
    /// [`SchedulerDb::delete_build`]'s fenced one-tx form (fence and
    /// atomicity together — A1 + D1 composed).
    // r[impl sched.db.table-retention+1]
    pub(crate) async fn gc_dead_build_wanted_outputs(
        &self,
        horizon_secs: f64,
        limit: i64,
        serving_generation: ServingGeneration,
    ) -> Result<u64, sqlx::Error> {
        let mut tx = match self.begin_fenced(serving_generation).await? {
            FencedBegin::Fenced { .. } => return Ok(0),
            FencedBegin::Open(ftx) => ftx,
        };
        let result = sqlx::query(
            "DELETE FROM build_wanted_outputs WHERE (build_id, derivation_id) IN (
                 SELECT w.build_id, w.derivation_id
                 FROM build_wanted_outputs w
                 LEFT JOIN builds b ON b.build_id = w.build_id
                 WHERE (b.finished_at IS NOT NULL
                        AND b.finished_at < now() - make_interval(secs => $1))
                    OR b.build_id IS NULL
                 LIMIT $2)",
        )
        .bind(horizon_secs)
        .bind(limit)
        .execute(tx.conn())
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }
}
