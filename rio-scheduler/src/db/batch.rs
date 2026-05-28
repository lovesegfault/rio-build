//! Batch operations for `persist_merge_to_db` — build-derivation mapping +
//! UNNEST-based bulk inserts.

use std::collections::HashMap;

use sqlx::PgConnection;
use uuid::Uuid;

use super::{DerivationRow, SchedulerDb, encode_pg_text_array};

impl SchedulerDb {
    /// Link a build to a derivation. Test-only singular form; production
    /// path is [`Self::batch_insert_build_derivations`].
    #[cfg(test)]
    pub async fn insert_build_derivation(
        &self,
        build_id: Uuid,
        derivation_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO build_derivations (build_id, derivation_id)
            VALUES ($1, $2)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(build_id)
        .bind(derivation_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // r[impl sched.db.batch-unnest]
    /// Batch-upsert derivations. Returns a map
    /// `drv_hash -> (derivation_id, resource_floor)`.
    ///
    /// Array parameters via `UNNEST`: 13 bind params total regardless of
    /// row count (vs `push_values`' 13×N, which hits PG's 65535-param
    /// limit at ~5041 rows). `RETURNING drv_hash` because PG doesn't
    /// guarantee `RETURNING` order matches `UNNEST` input order either.
    ///
    /// `floor_*` columns are returned so merge can hydrate them onto
    /// newly-inserted in-memory state (I-208) — `try_from_node` sets
    /// `floor=zeros`, but the DB row may pre-exist (ON CONFLICT) with a
    /// floor promoted by a prior run's failures. Without this the next
    /// SpawnIntent re-uses probe defaults and re-OOMs every run.
    pub(crate) async fn batch_upsert_derivations(
        tx: &mut PgConnection,
        rows: &[DerivationRow],
    ) -> Result<HashMap<String, (Uuid, crate::state::ResourceFloor)>, sqlx::Error> {
        if rows.is_empty() {
            return Ok(HashMap::new());
        }

        // Decompose struct-of-rows into row-of-arrays. Thirteen parallel
        // Vecs, one per column. This IS a transpose — lives for the
        // duration of one INSERT, cheaper than N roundtrips.
        //
        // Nested-array columns (required_features, expected_output_paths,
        // output_names, wanted_output_names) can't unnest as text[][] —
        // PG's multidim arrays are rectangular, but per-row feature
        // lists have variable length. Encode as pg text[] literals
        // ("{a,b,c}") and cast back in the SELECT. sqlx doesn't expose
        // a Vec<Vec<String>> → text[][] Encode anyway.
        let mut drv_hash = Vec::with_capacity(rows.len());
        let mut drv_path = Vec::with_capacity(rows.len());
        let mut pname = Vec::with_capacity(rows.len());
        let mut system = Vec::with_capacity(rows.len());
        let mut status = Vec::with_capacity(rows.len());
        let mut required_features = Vec::with_capacity(rows.len());
        let mut expected_output_paths = Vec::with_capacity(rows.len());
        let mut output_names = Vec::with_capacity(rows.len());
        let mut is_fixed_output = Vec::with_capacity(rows.len());
        let mut is_ca = Vec::with_capacity(rows.len());
        let mut wanted_output_names = Vec::with_capacity(rows.len());
        let mut topdown_pruned = Vec::with_capacity(rows.len());
        let mut closure_hole = Vec::with_capacity(rows.len());
        // bytea[] bind: sqlx's array encoder wants a homogeneous
        // element type, so None is encoded as an EMPTY bytea and
        // converted back to NULL SQL-side via NULLIF — the column
        // must be NULL (not '') for nodes without authoritative bytes.
        // The statement itself is last-write-wins for the rows it
        // receives; its only production caller is creation-scoped
        // (sched.persist.creation-scoped), so live-node joins never
        // reach it.
        let mut drv_content = Vec::with_capacity(rows.len());
        for r in rows {
            drv_hash.push(r.drv_hash.as_str());
            drv_path.push(r.drv_path.as_str());
            pname.push(r.pname.as_deref());
            system.push(r.system.as_str());
            status.push(r.status.as_str());
            required_features.push(encode_pg_text_array(&r.required_features));
            expected_output_paths.push(encode_pg_text_array(&r.expected_output_paths));
            output_names.push(encode_pg_text_array(&r.output_names));
            is_fixed_output.push(r.is_fixed_output);
            is_ca.push(r.is_ca);
            wanted_output_names.push(encode_pg_text_array(&r.wanted_output_names));
            topdown_pruned.push(r.topdown_pruned);
            closure_hole.push(r.closure_hole);
            drv_content.push(r.drv_content.clone().unwrap_or_default());
        }

        // r[impl sched.persist.recreate-refresh]
        // ON CONFLICT: refresh the full creation-time snapshot. Rows are
        // written only by submissions that (re)create the in-memory node
        // (sched.persist.creation-scoped) — joins never reach this query —
        // so last-write-wins on the declared identity (pname/system/
        // required_features) and on status='created' simply mirrors the
        // in-memory first-writer/displacement truth. The old "same
        // drv_hash → same content" justification is exactly what the
        // displacement path (sched.merge.authoritative-conflict)
        // invalidates: a displacing submission may carry a DIFFERENT
        // verifiable identity, and without the refresh a leader failover
        // would rebuild the node from the displaced squatter's identity.
        // Live accumulators (floor_*, poisoned_at, failed_builders,
        // retry_count, resubmit_cycles) are NOT touched — they have their
        // own writers (clear_poison/clear_poison_batch, floor updates).
        //
        // wanted_output_names is the exception to last-write-wins: it is
        // NOT a function of drv_hash — it is a function of who CONSUMES
        // the derivation, and a second build may want a different output
        // subset. Overwrite would let build B's narrower {out} clobber
        // build A's {out,dev} and un-want an output a still-live build
        // needs. It is therefore UNIONED on conflict, with empty
        // saturating to empty: '{}' is the "all declared outputs wanted"
        // sentinel, so all ∪ X = all (mirrors
        // `DerivationState::union_wanted`). The stored union only ever
        // grows for a given drv_hash; it is the persistence/recovery
        // fallback — classification reads the live effective set
        // (`effective_wanted`, in-memory per-build contributions) and
        // only falls back to this column.
        //
        // topdown_pruned is OR-combined on conflict for the same reason:
        // an unrelated, non-pruned merge of the same drv elsewhere must
        // never clear a prior pruned merge's marker through the upsert.
        // Clearing happens elsewhere: `clear_topdown_pruned_by_hashes`
        // from the post-reconciliation clear pass in `handle_merge_dag`,
        // the completion-time `clear_topdown_pruned_for_produced_parents`,
        // and the recovery-time gate in `load_dag_from_rows` (each keyed
        // on the node's children being produced — see each caller's
        // doc), and `clear_topdown_pruned_by_hash` for the lazy
        // walk-failure clear and when the topdown fail-fast consumes
        // the marker.
        //
        // closure_hole is OR-combined too, for the symmetric reason: the
        // merge bind is ALWAYS false (the upsert is never a stamping
        // site — the breadcrumb is set via `set_closure_hole_by_hashes`
        // by the leader's reap hook, by the recovery-time stamp in
        // `load_dag_from_rows`, and by the poison-clear paths — admin
        // ClearPoison and the poison-TTL sweep), and a pruned /
        // single-node re-merge of the same drv does not re-declare its
        // edges, so it must not launder the persisted truncation
        // evidence through the upsert. The only merge-side clear is
        // the explicit heal in `handle_merge_dag`
        // (`clear_closure_hole_by_hashes`, edge parents of a full
        // merge); the batched mark-clear helper below drops it
        // together with `topdown_pruned`, while the single-row
        // `clear_topdown_pruned_by_hash` is mark-only (the fail-fast
        // retains the breadcrumb for the directed resubmit).
        let result: Vec<(String, Uuid, i64, i64, i64)> = sqlx::query_as(
            r#"
            INSERT INTO derivations
                (drv_hash, drv_path, pname, system, status, required_features,
                 expected_output_paths, output_names, is_fixed_output, is_ca,
                 wanted_output_names, topdown_pruned, closure_hole, drv_content)
            SELECT
                drv_hash, drv_path, pname, system, status,
                required_features::text[],
                expected_output_paths::text[],
                output_names::text[],
                is_fixed_output, is_ca,
                wanted_output_names::text[],
                topdown_pruned, closure_hole,
                NULLIF(drv_content, ''::bytea)
            FROM UNNEST(
                $1::text[], $2::text[], $3::text[], $4::text[], $5::text[],
                $6::text[], $7::text[], $8::text[], $9::bool[], $10::bool[],
                $11::text[], $12::bool[], $13::bool[], $14::bytea[]
            ) AS t(drv_hash, drv_path, pname, system, status,
                   required_features, expected_output_paths, output_names,
                   is_fixed_output, is_ca, wanted_output_names, topdown_pruned,
                   closure_hole, drv_content)
            -- is_ca rides the same creation-snapshot refresh as the
            -- other identity columns: rows are written only by
            -- submissions that (re)create the node, and a displacing
            -- re-creation (sched.merge.authoritative-conflict) may
            -- legitimately carry a different declared identity than the
            -- row it replaces, so EXCLUDED is applied rather than
            -- assumed equal. Kept in the SET-list alongside the other
            -- UNNEST-bound columns ($10).
            --
            -- wanted_output_names is NOT idempotent-by-construction (it
            -- depends on the consumers, not the drv): union-with-empty-
            -- saturation. '{}' = "all wanted", so all ∪ X = all → '{}'
            -- if either side is empty; otherwise the sorted distinct
            -- union. Monotonically growing — never overwrite.
            --
            -- topdown_pruned: OR — set by pruned merges; this upsert
            -- never clears it. Cleared by clear_topdown_pruned_by_hashes
            -- (post-reconciliation pass, completion-time clear,
            -- recovery-time gate — once the node's children are
            -- produced) and by clear_topdown_pruned_by_hash (lazy
            -- walk-failure clear; fail-fast consumed it).
            --
            -- closure_hole: OR — set by the leader's reap hook, the
            -- recovery-time stamp in load_dag_from_rows, and the
            -- poison-clear paths (merges always bind false); this
            -- upsert never clears it.
            -- Cleared by the merge-time heal
            -- (clear_closure_hole_by_hashes) and alongside the mark by
            -- the batched clear_topdown_pruned_by_hashes helper (the
            -- single-row clear_topdown_pruned_by_hash is mark-only).
            ON CONFLICT (drv_hash) DO UPDATE SET
                updated_at = now(),
                pname = EXCLUDED.pname,
                system = EXCLUDED.system,
                required_features = EXCLUDED.required_features,
                status = EXCLUDED.status,
                expected_output_paths = EXCLUDED.expected_output_paths,
                output_names = EXCLUDED.output_names,
                is_fixed_output = EXCLUDED.is_fixed_output,
                is_ca = EXCLUDED.is_ca,
                wanted_output_names = CASE
                    WHEN cardinality(derivations.wanted_output_names) = 0
                      OR cardinality(EXCLUDED.wanted_output_names) = 0
                    THEN '{}'::text[]
                    ELSE ARRAY(
                        SELECT DISTINCT unnest(
                            derivations.wanted_output_names
                                || EXCLUDED.wanted_output_names
                        ) ORDER BY 1
                    )
                END,
                topdown_pruned = derivations.topdown_pruned OR EXCLUDED.topdown_pruned,
                closure_hole = derivations.closure_hole OR EXCLUDED.closure_hole,
                -- Only merges that (re)create the node reach this
                -- statement (sched.persist.creation-scoped); for those
                -- rows last write wins — an authoritative re-creation
                -- refreshes the persisted bytes, a store-backed
                -- re-creation clears them (the .drv is then fetchable
                -- from the store, so a persisted copy is unnecessary).
                -- A live node's bytes are never cleared by a later
                -- join; hostile content is bounded by SubmitBuild
                -- ingress validation plus node lifecycle, not by this
                -- clear.
                drv_content = EXCLUDED.drv_content
            RETURNING drv_hash, derivation_id,
                      floor_mem_bytes, floor_disk_bytes, floor_deadline_secs
            "#,
        )
        .bind(&drv_hash)
        .bind(&drv_path)
        .bind(&pname)
        .bind(&system)
        .bind(&status)
        .bind(&required_features)
        .bind(&expected_output_paths)
        .bind(&output_names)
        .bind(&is_fixed_output)
        .bind(&is_ca)
        .bind(&wanted_output_names)
        .bind(&topdown_pruned)
        .bind(&closure_hole)
        .bind(&drv_content)
        .fetch_all(&mut *tx)
        .await?;
        Ok(result
            .into_iter()
            .map(|(h, id, mem, disk, deadline)| {
                (
                    h,
                    (
                        id,
                        crate::state::ResourceFloor {
                            mem_bytes: mem.max(0) as u64,
                            disk_bytes: disk.max(0) as u64,
                            deadline_secs: deadline.clamp(0, u32::MAX as i64) as u32,
                        },
                    ),
                )
            })
            .collect())
    }

    /// Batch-insert build_derivations links.
    pub async fn batch_insert_build_derivations(
        tx: &mut PgConnection,
        build_id: Uuid,
        derivation_ids: &[Uuid],
    ) -> Result<(), sqlx::Error> {
        if derivation_ids.is_empty() {
            return Ok(());
        }
        // build_id is constant across rows — bind once as scalar $1,
        // cross-join UNNEST of the derivation_id array. Two binds total.
        sqlx::query(
            r#"
            INSERT INTO build_derivations (build_id, derivation_id)
            SELECT $1, derivation_id FROM UNNEST($2::uuid[]) AS t(derivation_id)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(build_id)
        .bind(derivation_ids)
        .execute(&mut *tx)
        .await?;
        Ok(())
    }

    /// Durable half of the displacement interest prune
    /// (`sched.merge.authoritative-conflict`): delete prior interested
    /// builds' links to a displaced derivation inside the same
    /// transaction as its recreate-refresh, so a leader failover — which
    /// rebuilds `interested_builds` purely from `build_derivations` —
    /// cannot re-point those builds at the displacing definition. The
    /// caller passes only non-terminal prior builds (terminal builds keep
    /// their links as settled history) and never the displacing build,
    /// whose own link is inserted by the same transaction.
    /// Returns the number of links removed. Plain runtime query — no
    /// `.sqlx/` impact.
    pub(crate) async fn delete_displaced_build_links(
        tx: &mut PgConnection,
        derivation_id: Uuid,
        prior_build_ids: &[Uuid],
    ) -> Result<u64, sqlx::Error> {
        if prior_build_ids.is_empty() {
            return Ok(0);
        }
        let res = sqlx::query(
            "DELETE FROM build_derivations WHERE derivation_id = $1 AND build_id = ANY($2)",
        )
        .bind(derivation_id)
        .bind(prior_build_ids)
        .execute(&mut *tx)
        .await?;
        Ok(res.rows_affected())
    }

    /// Batch-insert edges.
    pub async fn batch_insert_edges(
        tx: &mut PgConnection,
        edges: &[(Uuid, Uuid)],
    ) -> Result<(), sqlx::Error> {
        if edges.is_empty() {
            return Ok(());
        }
        let (parents, children): (Vec<Uuid>, Vec<Uuid>) = edges.iter().copied().unzip();
        sqlx::query(
            r#"
            INSERT INTO derivation_edges (parent_id, child_id)
            SELECT * FROM UNNEST($1::uuid[], $2::uuid[])
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(&parents)
        .bind(&children)
        .execute(&mut *tx)
        .await?;
        Ok(())
    }

    /// Tx-scoped batched `topdown_pruned` clear keyed by
    /// `derivation_id`. No production caller today: the merge-time
    /// clear that ran here in the edge-insert transaction was replaced
    /// by the post-reconciliation clear pass in `handle_merge_dag`
    /// (`clear_topdown_pruned_by_hashes`), which decides per unique
    /// parent only after `verify_preexisting_completed` has re-verified
    /// stale Completed children. Retained (test-only, like
    /// `insert_build_derivation` above) for the DB test pinning the
    /// OR-on-conflict + clear interplay and for potential future
    /// tx-scoped use.
    #[cfg(test)]
    pub(crate) async fn clear_topdown_pruned_for_parents(
        tx: &mut PgConnection,
        derivation_ids: &[Uuid],
    ) -> Result<(), sqlx::Error> {
        if derivation_ids.is_empty() {
            return Ok(());
        }
        sqlx::query(
            r#"
            UPDATE derivations SET topdown_pruned = false, updated_at = now()
            WHERE derivation_id = ANY($1) AND topdown_pruned
            "#,
        )
        .bind(derivation_ids)
        .execute(&mut *tx)
        .await?;
        Ok(())
    }

    /// Best-effort batched `topdown_pruned` clear keyed by `drv_hash`,
    /// on the pool (outside any transaction). Callers: the
    /// post-reconciliation clear pass in `handle_merge_dag` (unique
    /// parents whose children are all produced — and verified — after
    /// `reconcile_merged_state`),
    /// `clear_topdown_pruned_for_produced_parents` in completion.rs
    /// (parents whose last child just became produced), and the
    /// recovery-time gate in `load_dag_from_rows` (restored marks whose
    /// persisted children are all produced and vouched for by a live
    /// (`pending`/`active`) build that also owns the parent); each
    /// clears its batch in one statement.
    /// Also resets the `closure_hole` breadcrumb (`migrations/064`):
    /// the breadcrumb only qualifies the mark, so it travels with it —
    /// and the widened WHERE additionally mops up a markless leftover
    /// hole (a heal whose best-effort PG write was lost after the mark
    /// itself had already been cleared).
    /// Returns the number of rows actually touched.
    /// Same error posture as `clear_topdown_pruned_by_hash`: the caller
    /// warns and continues — the in-memory clear already happened and
    /// the merge outcome must not depend on this write.
    pub(crate) async fn clear_topdown_pruned_by_hashes(
        &self,
        drv_hashes: &[String],
    ) -> Result<u64, sqlx::Error> {
        if drv_hashes.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            r#"
            UPDATE derivations
            SET topdown_pruned = false, closure_hole = false, updated_at = now()
            WHERE drv_hash = ANY($1) AND (topdown_pruned OR closure_hole)
            "#,
        )
        .bind(drv_hashes)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Best-effort single-row, mark-only `topdown_pruned` clear, keyed
    /// by `drv_hash`, outside any transaction. Never touches the
    /// `closure_hole` breadcrumb (unlike `clear_topdown_pruned_by_hashes`,
    /// whose callers are all keyed on produced/vouched children). Two
    /// callers, and mark-only is correct at both:
    ///  - the topdown fail-fast when it parks a node: the marker it
    ///    just consumed must not survive in PG (or the next leader
    ///    restores it onto a childless node and the fail-fast re-arms
    ///    after every failover), but the breadcrumb is deliberately
    ///    retained — the directed resubmit the fail-fast solicits goes
    ///    through the resubmit-reset, which keeps the truncated child
    ///    edges and carries the breadcrumb, so the re-pruning merge's
    ///    stamp gates re-stamp the node instead of reading its produced
    ///    survivors as Vouched (round-23 bug_006);
    ///  - the lazy clear in `handle_substitute_complete` when the
    ///    node's children are all already produced at walk-failure
    ///    time: that arm fires only on `Vouched` closure evidence,
    ///    which requires the in-memory hole to be false — there is no
    ///    in-memory hole consumption to mirror, and a persisted-only
    ///    leftover (a lost heal write) is the next full merge's heal to
    ///    drop, not this helper's.
    ///
    /// Callers treat an error as warn-and-continue — the in-memory
    /// clear already happened and the build verdict must not depend on
    /// this write.
    pub(crate) async fn clear_topdown_pruned_by_hash(
        &self,
        drv_hash: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE derivations
            SET topdown_pruned = false, updated_at = now()
            WHERE drv_hash = $1 AND topdown_pruned
            "#,
        )
        .bind(drv_hash)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Best-effort batched `closure_hole` stamp keyed by `drv_hash`, on
    /// the pool (outside any transaction). Four callers, one per
    /// production removal of an un-produced child out from under a
    /// surviving parent: the leader-gated survivor hook in
    /// `handle_cleanup_terminal_build`, for the parents the
    /// terminal-build reap just holed (`ReapOutcome::holed_parents`);
    /// the recovery-time stamp in `load_dag_from_rows`, for recovered
    /// parents whose un-produced terminal children's edges the recovery
    /// load dropped (the recovery-side analogue of the reap); and the
    /// two poison-clear paths — admin ClearPoison
    /// (`handle_clear_poison`) and the poison-TTL sweep
    /// (`tick_process_expired_poisons`) — for the surviving parents of
    /// the Poisoned (by definition un-produced) child they remove. All
    /// four share the same posture: the write runs only on the leader
    /// (hook gate, recovery, admin leader guard, standby tick no-op —
    /// `r[sched.lease.standby-drops-writes]`) and the in-memory
    /// breadcrumb is stamped at the removal site itself, independently
    /// of this write.
    /// Returns the number of rows actually stamped. The caller warns and
    /// continues on error — losing the write costs durability of the
    /// breadcrumb across a failover (the already-accepted best-effort
    /// window), never this tenure's correctness.
    pub(crate) async fn set_closure_hole_by_hashes(
        &self,
        drv_hashes: &[String],
    ) -> Result<u64, sqlx::Error> {
        if drv_hashes.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            r#"
            UPDATE derivations SET closure_hole = true, updated_at = now()
            WHERE drv_hash = ANY($1) AND NOT closure_hole
            "#,
        )
        .bind(drv_hashes)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Best-effort batched `closure_hole` clear keyed by `drv_hash`, on
    /// the pool (outside any transaction). Sole caller: the merge-time
    /// heal in `handle_merge_dag`, for EVERY edge parent of a full
    /// merge (the heal clears the hole even when the `topdown_pruned`
    /// mark stays, so it cannot ride the mark-clear helpers above; it
    /// is total — not keyed on the in-memory bit — because the
    /// persisted copy can be stale when the in-memory one was cleared
    /// elsewhere or lost, and the `AND closure_hole` WHERE keeps the
    /// statement a no-op for clean rows). Returns the number of rows
    /// actually cleared. The caller warns and continues on error — a
    /// stale persisted hole errs toward the bounded fail-fast after a
    /// later failover, never the doomed from-source dispatch.
    pub(crate) async fn clear_closure_hole_by_hashes(
        &self,
        drv_hashes: &[String],
    ) -> Result<u64, sqlx::Error> {
        if drv_hashes.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            r#"
            UPDATE derivations SET closure_hole = false, updated_at = now()
            WHERE drv_hash = ANY($1) AND closure_hole
            "#,
        )
        .bind(drv_hashes)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
