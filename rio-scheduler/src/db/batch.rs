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
    /// Array parameters via `UNNEST`: 17 bind params total regardless of
    /// row count (vs `push_values`' 17×N, which hits PG's 65535-param
    /// limit at ~3855 rows). `RETURNING drv_hash` because PG doesn't
    /// guarantee `RETURNING` order matches `UNNEST` input order either.
    ///
    /// `floor_*` columns are returned so merge can hydrate them onto
    /// newly-inserted in-memory state (I-208) — `try_from_node` sets
    /// `floor=zeros`, but the DB row may pre-exist (ON CONFLICT) with a
    /// floor promoted by a prior run's failures. Without this the next
    /// SpawnIntent re-uses probe defaults and re-OOMs every run.
    ///
    /// `definition_changed` is the merge's full APPROVED
    /// definition-change set: DAG-level displacements and authority
    /// takeovers (`sched.merge.authoritative-conflict`) plus row-only
    /// store-evidence displacements
    /// (`sched.merge.store-evidence-displacement+3`). Only these may
    /// pass the settled-identity WHERE guard below
    /// (`sched.persist.settled-identity-freeze+3`); the actor's
    /// arbitration is the decision, this array is its in-transaction
    /// execution. The same enumeration feeds Batch 1a's
    /// closure-witness clear — assembled ONCE at the caller so the two
    /// consumers cannot drift on the population.
    ///
    /// `evidence_displaced` (the old name/scope — row-only arm alone)
    /// was an axis-gap accident: resident displacements only passed
    /// the guard because the pre-+3 conflict predicate lacked the
    /// path/hash axes.
    pub(crate) async fn batch_upsert_derivations(
        tx: &mut PgConnection,
        rows: &[DerivationRow],
        definition_changed: &[String],
    ) -> Result<HashMap<String, (Uuid, crate::state::ResourceFloor)>, sqlx::Error> {
        if rows.is_empty() {
            return Ok(HashMap::new());
        }

        // Decompose struct-of-rows into row-of-arrays. Sixteen parallel
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
        // bytea[] binds: sqlx's array encoder wants a homogeneous
        // element type, so None is encoded as an EMPTY bytea and
        // converted back to NULL SQL-side via NULLIF — the columns
        // must be NULL (not '') for nodes without authoritative bytes
        // or without a CA modular hash.
        // The statement itself is last-write-wins for the rows it
        // receives; its only production caller is creation-scoped
        // (sched.persist.creation-scoped), so live-node joins never
        // reach it.
        let mut drv_content = Vec::with_capacity(rows.len());
        let mut ca_modular_hash = Vec::with_capacity(rows.len());
        let mut ca_modular_hash_stripped = Vec::with_capacity(rows.len());
        let mut evidence_rank = Vec::with_capacity(rows.len());
        let mut needs_resolve = Vec::with_capacity(rows.len());
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
            ca_modular_hash.push(r.ca_modular_hash.map(|h| h.to_vec()).unwrap_or_default());
            ca_modular_hash_stripped.push(
                r.ca_modular_hash_stripped
                    .map(|h| h.to_vec())
                    .unwrap_or_default(),
            );
            evidence_rank.push(r.evidence_rank.as_str());
            needs_resolve.push(r.needs_resolve);
        }

        // r[impl sched.persist.recreate-refresh+2]
        // ON CONFLICT: refresh the full creation-time snapshot. Rows are
        // written only by submissions that (re)create the in-memory node
        // (sched.persist.creation-scoped) — joins never reach this query —
        // so last-write-wins on the declared identity (pname/system/
        // required_features), on the declared `drv_path` (recovery and
        // dispatch read the .drv path from this row, so a displaced
        // squatter's decoy path must not survive into the displacing
        // definition), and on status='created' simply mirrors the
        // in-memory first-writer/displacement truth. The old "same
        // drv_hash → same content" justification is exactly what the
        // displacement path (sched.merge.authoritative-conflict)
        // invalidates: a displacing submission may carry a DIFFERENT
        // verifiable identity, and without the refresh a leader failover
        // would rebuild the node from the displaced squatter's identity.
        //
        // Live accumulators (floor_*, poisoned_at, failed_builders,
        // retry_count, resubmit_cycles) keep their own writers
        // (clear_poison/clear_poison_batch, floor updates) and are NOT
        // touched — EXCEPT on a definition change: when the row's prior
        // creation persisted authoritative bytes and the incoming
        // creation's content is not byte-identical (a store-backed
        // takeover clearing the bytes, or a byte-different authoritative
        // displacement), the accumulators were produced by a DIFFERENT
        // definition's builder and must not carry over
        // (sched.merge.displaced-failure-reset). Doing the reset in this
        // statement makes it ride the merge transaction
        // (sched.persist.atomic-activation) and means RETURNING already
        // yields the reset floors, so the I-208 hydration needs no
        // special-casing for takeover rows. Byte-identical authoritative
        // resubmits and store-backed-origin rows (drv_content IS NULL)
        // keep the floor-preserving semantics.
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
        // site — the breadcrumb is set via `set_closure_holes`
        // by the leader's reap hook, by the recovery-time stamp in
        // `load_dag_from_rows`, and by the poison-clear paths — admin
        // ClearPoison and the poison-TTL sweep), and a pruned /
        // single-node re-merge of the same drv does not re-declare its
        // edges, so it must not launder the persisted truncation
        // evidence through the upsert. The only merge-side clear is
        // the explicit heal in `handle_merge_dag`
        // (`clear_closure_hole_by_hashes`, keyed on
        // `MergeResult::healed_parents` — accepted trigger ∧ witness
        // coverage; see its defining field doc); the batched mark-clear helper below drops it
        // together with `topdown_pruned`, while the single-row
        // `clear_topdown_pruned_by_hash` is mark-only (the fail-fast
        // retains the breadcrumb for the directed resubmit).
        let result: Vec<(String, Uuid, i64, i64, i64)> = sqlx::query_as(
            r#"
            INSERT INTO derivations
                (drv_hash, drv_path, pname, system, status, required_features,
                 expected_output_paths, output_names, is_fixed_output, is_ca,
                 wanted_output_names, topdown_pruned, closure_hole, drv_content,
                 ca_modular_hash, evidence_rank, ca_modular_hash_stripped,
                 needs_resolve)
            SELECT
                drv_hash, drv_path, pname, system, status,
                required_features::text[],
                expected_output_paths::text[],
                output_names::text[],
                is_fixed_output, is_ca,
                wanted_output_names::text[],
                topdown_pruned, closure_hole,
                NULLIF(drv_content, ''::bytea),
                NULLIF(ca_modular_hash, ''::bytea),
                evidence_rank,
                NULLIF(ca_modular_hash_stripped, ''::bytea),
                needs_resolve
            FROM UNNEST(
                $1::text[], $2::text[], $3::text[], $4::text[], $5::text[],
                $6::text[], $7::text[], $8::text[], $9::bool[], $10::bool[],
                $11::text[], $12::bool[], $13::bool[], $14::bytea[], $15::bytea[],
                $16::text[], $17::bytea[], $18::bool[]
            ) AS t(drv_hash, drv_path, pname, system, status,
                   required_features, expected_output_paths, output_names,
                   is_fixed_output, is_ca, wanted_output_names, topdown_pruned,
                   closure_hole, drv_content, ca_modular_hash, evidence_rank,
                   ca_modular_hash_stripped, needs_resolve)
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
                drv_path = EXCLUDED.drv_path,
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
                drv_content = EXCLUDED.drv_content,
                -- r[impl sched.derivation.evidence-rank]
                -- Creation-snapshot semantics like the identity columns
                -- above — deliberately NOT MAX-combined: rank
                -- monotonicity is scoped per node LIFECYCLE, and a
                -- legitimate matching-identity re-creation (resubmit
                -- after store GC, displacement) starts a new lifecycle
                -- at its own ingress rank. Settle/dispatch upgrades go
                -- through the runtime persist_evidence_rank writer, not
                -- this upsert.
                evidence_rank = EXCLUDED.evidence_rank,
                -- r[impl sched.persist.ca-modular-hash+2]
                -- The CA modular hash is snapshot identity (the
                -- content-bound evidence the merge gate compares, and
                -- the realisation key for CA and deferred-IA rows), so
                -- it rides the same unconditional creation-snapshot
                -- refresh as the columns above — NOT the
                -- definition-change accumulator reset below.
                ca_modular_hash = EXCLUDED.ca_modular_hash,
                -- M_071 dispatch-resolve flag: creation-snapshot
                -- semantics like evidence_rank/is_ca above — the flag
                -- is identity-derived (a function of the definition's
                -- bytes/type, byte-derived for verified creations),
                -- so the re-creating submission's value wins. The
                -- dispatch-raise upgrades go through the runtime
                -- persist_evidence_rank writers (COALESCE — the settle
                -- chokepoint passes NULL), not this upsert. Always
                -- non-NULL from this statement; NULL marks pre-071
                -- legacy rows only (recovery's degrade fallback).
                needs_resolve = EXCLUDED.needs_resolve,
                -- M_070 preserved stripped claim: superseded by a live
                -- (verifiable) hash on the re-creating submission —
                -- strictly better evidence — else carried forward
                -- (COALESCE keeps an older preserved claim when the
                -- re-creation is bare). Never copied into the live
                -- column by any writer.
                ca_modular_hash_stripped = CASE
                    WHEN EXCLUDED.ca_modular_hash IS NOT NULL THEN NULL
                    ELSE COALESCE(EXCLUDED.ca_modular_hash_stripped,
                                  derivations.ca_modular_hash_stripped) END,
                -- r[impl sched.merge.displaced-failure-reset+2]
                -- Definition-change reset: the prior creation was
                -- authoritative (bytes persisted) and the incoming
                -- creation's content differs (store-backed takeover or
                -- byte-different authoritative displacement) — failure
                -- attribution and reactive sizing must not cross the
                -- definition boundary, so zero the accumulators in the
                -- same statement (and therefore the same transaction).
                -- All other re-creations preserve them.
                poisoned_at = CASE
                    WHEN derivations.drv_content IS NOT NULL
                         AND EXCLUDED.drv_content IS DISTINCT FROM derivations.drv_content
                    THEN NULL ELSE derivations.poisoned_at END,
                failed_builders = CASE
                    WHEN derivations.drv_content IS NOT NULL
                         AND EXCLUDED.drv_content IS DISTINCT FROM derivations.drv_content
                    THEN '{}'::text[] ELSE derivations.failed_builders END,
                retry_count = CASE
                    WHEN derivations.drv_content IS NOT NULL
                         AND EXCLUDED.drv_content IS DISTINCT FROM derivations.drv_content
                    THEN 0 ELSE derivations.retry_count END,
                resubmit_cycles = CASE
                    WHEN derivations.drv_content IS NOT NULL
                         AND EXCLUDED.drv_content IS DISTINCT FROM derivations.drv_content
                    THEN 0 ELSE derivations.resubmit_cycles END,
                floor_mem_bytes = CASE
                    WHEN derivations.drv_content IS NOT NULL
                         AND EXCLUDED.drv_content IS DISTINCT FROM derivations.drv_content
                    THEN 0 ELSE derivations.floor_mem_bytes END,
                floor_disk_bytes = CASE
                    WHEN derivations.drv_content IS NOT NULL
                         AND EXCLUDED.drv_content IS DISTINCT FROM derivations.drv_content
                    THEN 0 ELSE derivations.floor_disk_bytes END,
                floor_deadline_secs = CASE
                    WHEN derivations.drv_content IS NOT NULL
                         AND EXCLUDED.drv_content IS DISTINCT FROM derivations.drv_content
                    THEN 0 ELSE derivations.floor_deadline_secs END
            -- r[impl sched.persist.settled-identity-freeze+3]
            -- Defense-in-depth twin of the pre-merge settled-identity
            -- check (actor/settled.rs): a SETTLED row (completed/
            -- skipped — the durable record of a successful build) whose
            -- public identity conflicts with the incoming re-creation
            -- is left completely untouched by this upsert. The row then
            -- does not appear in RETURNING, so the merge's link/edge
            -- persistence fails loudly (MissingDbId → Internal →
            -- cleanup) instead of silently rewriting settled history.
            -- Matching-identity re-creations (legitimate rebuild after
            -- store GC) update normally. Primary enforcement is the
            -- pre-merge check; this guard only matters if that check is
            -- bypassed (bug) or a racing writer settles the row between
            -- check and upsert.
            --
            -- AXIS PARITY with settled_row_identity_matches (round-16
            -- merged_bug_087; pinned by the differential conformance
            -- test in db/tests/batch.rs — a divergence in either
            -- direction is a bug):
            --   * output_names as SORTED sets (raw IS DISTINCT FROM was
            --     order-sensitive: a set-equal reordered resubmission
            --     passed the merge check, then died here with an opaque
            --     Internal);
            --   * expected_output_paths per output name where BOTH
            --     sides declare one (omission let a four-axis-matching
            --     re-creation silently overwrite the settled row's
            --     paths in exactly the bypass/race scenarios the guard
            --     documents itself as existing for);
            --   * live ca_modular_hash present on both sides but
            --     differing vetoes (same omission consequence). The
            --     EXCLUDED hash is NULLIF-normalized in the source
            --     SELECT, so NULL = "no claim" on either side never
            --     conflicts (one-sided evidence is not a contradiction
            --     — same as ModularHashEvidence's one-sided arm).
            --
            -- r[impl sched.merge.store-evidence-displacement+3]
            -- The $19 carve-out is the merge's FULL approved
            -- definition-change set: DAG-level displacements and
            -- authority takeovers (arbitrated by
            -- sched.merge.authoritative-conflict) plus row-only
            -- store-evidence displacements (verified by the pre-merge
            -- check against ingress-byte-bound rank or the store's own
            -- text-CA .drv bytes,
            -- sched.merge.store-evidence-displacement+3). Every
            -- legitimately arbitrated definition change is admitted
            -- EXPLICITLY by this list — never by an axis gap in the
            -- conflict predicate (pre-+3, the missing path/hash axes
            -- accidentally admitted resident-displacement re-creations
            -- whose only conflict was the path; the axis alignment
            -- exposed that the resident arm was never carved out). The
            -- hash list is per-merge and threaded through the one
            -- transaction, so the guard stays unconditional for every
            -- other writer.
            WHERE derivations.drv_hash = ANY($19)
               OR NOT (
                derivations.status IN ('completed', 'skipped')
                AND (
                    derivations.system IS DISTINCT FROM EXCLUDED.system
                    OR ARRAY(SELECT unnest(derivations.output_names) ORDER BY 1)
                       IS DISTINCT FROM
                       ARRAY(SELECT unnest(EXCLUDED.output_names) ORDER BY 1)
                    OR derivations.is_fixed_output IS DISTINCT FROM EXCLUDED.is_fixed_output
                    OR derivations.is_ca IS DISTINCT FROM EXCLUDED.is_ca
                    OR (derivations.ca_modular_hash IS NOT NULL
                        AND EXCLUDED.ca_modular_hash IS NOT NULL
                        AND derivations.ca_modular_hash <> EXCLUDED.ca_modular_hash)
                    OR EXISTS (
                        SELECT 1
                        FROM unnest(derivations.output_names,
                                    derivations.expected_output_paths) AS r(name, path)
                        JOIN unnest(EXCLUDED.output_names,
                                    EXCLUDED.expected_output_paths) AS e(name, path)
                          ON r.name = e.name
                        WHERE r.path <> '' AND e.path <> '' AND r.path <> e.path
                    )
                )
            )
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
        .bind(&ca_modular_hash)
        .bind(&evidence_rank)
        .bind(&ca_modular_hash_stripped)
        .bind(&needs_resolve)
        .bind(definition_changed)
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
    ///
    /// `adjust_total`: when the displaced node's result had NOT been
    /// received by those builds (not `Completed`/`Skipped`), the same
    /// statement decrements each pruned build's `builds.total_drvs` by
    /// the number of links it lost — the durable mirror of the
    /// in-memory `total_count` decrement, so recovery (which re-seeds
    /// totals from that column) cannot resurrect a slot the build no
    /// longer waits on. Pass `false` when the result was already
    /// received: the build keeps the credit and the total.
    /// Returns the number of links removed. Plain runtime query — no
    /// `.sqlx/` impact.
    pub(crate) async fn delete_displaced_build_links(
        tx: &mut PgConnection,
        derivation_id: Uuid,
        prior_build_ids: &[Uuid],
        adjust_total: bool,
    ) -> Result<u64, sqlx::Error> {
        if prior_build_ids.is_empty() {
            return Ok(0);
        }
        if !adjust_total {
            let res = sqlx::query(
                "DELETE FROM build_derivations WHERE derivation_id = $1 AND build_id = ANY($2)",
            )
            .bind(derivation_id)
            .bind(prior_build_ids)
            .execute(&mut *tx)
            .await?;
            return Ok(res.rows_affected());
        }
        // GREATEST(.., 0): defensive clamp — totals are display/recovery
        // accounting, and a clamp is strictly better than a negative
        // total if a future caller double-prunes.
        let pruned: i64 = sqlx::query_scalar(
            r#"
            WITH pruned AS (
                DELETE FROM build_derivations
                WHERE derivation_id = $1 AND build_id = ANY($2)
                RETURNING build_id
            ),
            adjusted AS (
                UPDATE builds b
                SET total_drvs = GREATEST(b.total_drvs - p.cnt, 0)
                FROM (SELECT build_id, COUNT(*)::int AS cnt FROM pruned GROUP BY build_id) p
                WHERE b.build_id = p.build_id
            )
            SELECT COUNT(*) FROM pruned
            "#,
        )
        .bind(derivation_id)
        .bind(prior_build_ids)
        .fetch_one(&mut *tx)
        .await?;
        Ok(pruned.max(0) as u64)
    }

    /// Durable half of the displaced-edge scrub
    /// (`sched.merge.displaced-edge-scrub`): delete every
    /// `derivation_edges` row whose PARENT is a displaced or taken-over
    /// (authority-flip) derivation, inside the same transaction as its
    /// recreate-refresh and strictly before this merge's own edges are
    /// inserted. The removed node's row keeps its `derivation_id`, so
    /// without this delete a leader failover would reload the squatter's
    /// dependency edges onto the replacing definition (seeding it
    /// `DependencyFailed` or parking it behind a child it never
    /// declared). Child-side rows — edges where the removed derivation is
    /// the dependency of someone else — are preserved. Returns the number
    /// of edges removed. Plain runtime query — no `.sqlx/` impact.
    pub(crate) async fn delete_displaced_parent_edges(
        tx: &mut PgConnection,
        derivation_ids: &[Uuid],
    ) -> Result<u64, sqlx::Error> {
        if derivation_ids.is_empty() {
            return Ok(0);
        }
        let res = sqlx::query("DELETE FROM derivation_edges WHERE parent_id = ANY($1)")
            .bind(derivation_ids)
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

    /// Transaction-scoped `topdown_pruned` stamp for kept nodes a pruned
    /// merge merely JOINED. The derivations upsert is creation-scoped
    /// (`sched.persist.creation-scoped`): only nodes the merge (re)creates
    /// reach Batch 1, so a pre-existing node kept by a top-down prune
    /// never gets the row-bind stamp — without this statement the
    /// demand-set guard (`sched.merge.substitute-topdown`) would be
    /// memory-only for exactly those nodes and vanish on leader failover.
    /// Runs inside the merge transaction (Batch 1b in
    /// `persist_merge_to_db`), so the stamp commits or rolls back with the
    /// rest of the merge. The caller applies the same gate as the row
    /// bind (parents of the pruned submission's edges, minus nodes whose
    /// existing children the closure classifier vouches for). The
    /// born-holed flag + witness rows for the same parents are written
    /// by the paired [`Self::set_closure_holes_tx`] in the same
    /// transaction (round-16 bug_045: this stamp is mark-only and must
    /// never be the population's sole writer). Clearing is unchanged
    /// (`clear_topdown_pruned_by_hashes` and friends).
    /// Plain runtime query — no `.sqlx/` impact.
    pub(crate) async fn stamp_topdown_pruned_tx(
        tx: &mut PgConnection,
        drv_hashes: &[String],
    ) -> Result<u64, sqlx::Error> {
        if drv_hashes.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            r#"
            UPDATE derivations
            SET topdown_pruned = true, updated_at = now()
            WHERE drv_hash = ANY($1) AND NOT topdown_pruned
            "#,
        )
        .bind(drv_hashes)
        .execute(&mut *tx)
        .await?;
        Ok(result.rows_affected())
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
    ///
    /// One transaction with the 069 witness-row DELETE (round-15
    /// C6c1): every clear of the `closure_hole` flag clears its
    /// witness set — the flag ⇔ side-rows invariant the recovery
    /// hydration debug-asserts holds across THIS writer too.
    pub(crate) async fn clear_topdown_pruned_by_hashes(
        &self,
        drv_hashes: &[String],
    ) -> Result<u64, sqlx::Error> {
        if drv_hashes.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            r#"
            UPDATE derivations
            SET topdown_pruned = false, closure_hole = false, updated_at = now()
            WHERE drv_hash = ANY($1) AND (topdown_pruned OR closure_hole)
            "#,
        )
        .bind(drv_hashes)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            DELETE FROM derivation_closure_missing WHERE drv_hash = ANY($1)
            "#,
        )
        .bind(drv_hashes)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
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
    pub(crate) async fn set_closure_holes(
        &self,
        holes: &[(String, Vec<String>)],
    ) -> Result<u64, sqlx::Error> {
        if holes.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await?;
        let result = Self::set_closure_holes_tx(&mut tx, holes).await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Transaction-scoped paired writer for the closure-hole invariant:
    /// the M_064 flag and its 069 witness rows are written together and
    /// are never observable apart (the recovery debug-assert and the
    /// LOST_WITNESS sentinel's "impossible by construction" claim both
    /// rest on this). This is THE flag-true writer — every stamping
    /// site (the pool wrapper [`Self::set_closure_holes`] and the
    /// merge-transaction Batch 1b for born-holed pruned parents, joined
    /// and newly created alike) routes through it; per fix-discipline
    /// R2-PAIRED-WRITERS, two writers with different population scopes
    /// is exactly the round-16 bug_045 signature this helper retires.
    ///
    /// The flag update carries no `AND NOT closure_hole` filter: a
    /// SECOND truncation of an already-holed parent must append its
    /// children (ON CONFLICT DO NOTHING dedups), not be filtered out by
    /// the bool. ON CONFLICT DO NOTHING also makes a re-pruned parent
    /// append only new children. Plain runtime queries — no `.sqlx/`
    /// impact.
    pub(crate) async fn set_closure_holes_tx(
        tx: &mut PgConnection,
        holes: &[(String, Vec<String>)],
    ) -> Result<u64, sqlx::Error> {
        if holes.is_empty() {
            return Ok(0);
        }
        let parents: Vec<String> = holes.iter().map(|(p, _)| p.clone()).collect();
        let (side_parents, side_children): (Vec<String>, Vec<String>) = holes
            .iter()
            .flat_map(|(p, cs)| cs.iter().map(move |c| (p.clone(), c.clone())))
            .unzip();
        let result = sqlx::query(
            r#"
            UPDATE derivations SET closure_hole = true, updated_at = now()
            WHERE drv_hash = ANY($1)
            "#,
        )
        .bind(&parents)
        .execute(&mut *tx)
        .await?;
        if !side_parents.is_empty() {
            sqlx::query(
                r#"
                INSERT INTO derivation_closure_missing (drv_hash, missing_child)
                SELECT * FROM UNNEST($1::text[], $2::text[])
                ON CONFLICT DO NOTHING
                "#,
            )
            .bind(&side_parents)
            .bind(&side_children)
            .execute(&mut *tx)
            .await?;
        }
        Ok(result.rows_affected())
    }

    /// Transaction-scoped paired CLEAR of the closure-hole invariant:
    /// flag false + witness-row DELETE in the caller's transaction.
    /// Two callers: the pool wrapper [`Self::clear_closure_hole_by_hashes`]
    /// (merge-time heal) and the merge persist's definition-change
    /// clear (`sched.closure.witness-epoch`): displaced nodes,
    /// authority takeovers, and row-only store-evidence displacements
    /// all replace the definition the witness testified about, and the
    /// creation upsert's `closure_hole: OR` semantics would otherwise
    /// preserve the dead epoch's flag+rows for recovery to resurrect
    /// (round-16 bug_011). At the merge site this runs after Batch 1
    /// and BEFORE the born-holed stamp (Batch 1b), so a re-creation
    /// that is itself a pruned stamping parent ends the transaction
    /// with ITS OWN epoch's witness, not a union of eras.
    pub(crate) async fn clear_closure_holes_tx(
        tx: &mut PgConnection,
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
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            DELETE FROM derivation_closure_missing WHERE drv_hash = ANY($1)
            "#,
        )
        .bind(drv_hashes)
        .execute(&mut *tx)
        .await?;
        Ok(result.rows_affected())
    }

    /// Best-effort batched `closure_hole` clear keyed by `drv_hash`, on
    /// the pool (outside any transaction). Sole caller: the merge-time
    /// heal in `handle_merge_dag`, for the coverage-HEALED parents of a
    /// full merge only (`MergeResult::healed_parents` — accepted
    /// trigger ∧ witness coverage; see its defining field doc). The
    /// heal clears the hole even when the `topdown_pruned`
    /// mark stays, so it cannot ride the mark-clear helpers above; it
    /// is total over that healed set — not keyed on the in-memory bit —
    /// because the
    /// persisted copy can be stale when the in-memory one was cleared
    /// elsewhere or lost, and the `AND closure_hole` WHERE keeps the
    /// statement a no-op for clean rows. Returns the number of rows
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
        // Same-transaction DELETE of the witness rows (069): a healed
        // parent's stale missing-set must not survive to poison the
        // NEXT hole's coverage check (subset over a union of eras).
        // Shares the paired statement body with the merge-transaction
        // definition-change clear (`clear_closure_holes_tx`).
        let mut tx = self.pool.begin().await?;
        let result = Self::clear_closure_holes_tx(&mut tx, drv_hashes).await?;
        tx.commit().await?;
        Ok(result)
    }
}
