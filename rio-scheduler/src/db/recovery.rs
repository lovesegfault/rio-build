//! Phase 3b: state recovery read queries.
//!
//! Called by recover_from_pg() on LeaderAcquired transition. Loads
//! all non-terminal builds + derivations + edges + build_derivations +
//! assignments, from which the actor rebuilds its in-mem DAG.
//!
//! FromRow structs (not tuples): recovery needs ~10 fields per
//! derivation, tuples at that arity are error-prone (wrong-field
//! assignment). #[derive(FromRow)] + named columns is safer.

use uuid::Uuid;

use super::{
    GraphEdgeRow, GraphNodeRow, RecoveryBuildRow, RecoveryDerivationRow, SchedulerDb,
    terminal_status_sql,
};

/// The per-child PRODUCED half of the strict criterion
/// (T-D2.2/PD-D4). `e`/`c` are the edge/child aliases of the enclosing
/// query.
const CHILD_PRODUCED_SQL: &str = "c.status IN ('completed', 'skipped')";

/// The per-child LIVE CO-OWNING VOUCHER half (the third conjunct —
/// RS-1's load-bearing protection): a `'pending'`/`'active'` build
/// links BOTH the child and the parent. PG retains a terminal build's
/// completed children indefinitely, so without live-build scoping a
/// previous-generation node classifies Vouched and launders a stale
/// closure into a doomed from-source dispatch.
const CHILD_LIVE_VOUCHER_SQL: &str = "EXISTS (SELECT 1 FROM build_derivations bd \
     JOIN builds b ON b.build_id = bd.build_id \
     JOIN build_derivations bdp \
       ON bdp.build_id = bd.build_id \
      AND bdp.derivation_id = e.parent_id \
     WHERE bd.derivation_id = c.derivation_id \
       AND b.status IN ('pending', 'active'))";

/// The durable-classifier query (T-D2.2), assembled once from the
/// shared fragments (a `LazyLock` so sqlx sees a `'static` string).
static CLASSIFY_EVIDENCE_SQL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "SELECT count(*), \
                bool_and(({CHILD_PRODUCED_SQL}) AND ({CHILD_LIVE_VOUCHER_SQL})), \
                bool_or(({CHILD_PRODUCED_SQL}) AND NOT ({CHILD_LIVE_VOUCHER_SQL})) \
         FROM derivation_edges e \
         JOIN derivations c ON c.derivation_id = e.child_id \
         WHERE e.parent_id = $1",
    )
});

impl SchedulerDb {
    /// Load all non-terminal builds. Terminal builds (succeeded/
    /// failed/cancelled) don't need recovery — they're done, and a
    /// late `WatchBuild` against a terminal build PG still knows about
    /// is answered from the `builds` row (snapshot), not from recovered
    /// actor state.
    pub(crate) async fn load_nonterminal_builds(
        &self,
    ) -> Result<Vec<RecoveryBuildRow>, sqlx::Error> {
        sqlx::query_as(
            r#"
            SELECT build_id, tenant_id, status, priority_class,
                   keep_going, options_json,
                   total_drvs, completed_drvs, cached_drvs,
                   EXTRACT(EPOCH FROM (now() - submitted_at))::float8
                       AS submitted_age_secs
            FROM builds
            WHERE status IN ('pending', 'active')
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Load all non-terminal derivations. Literal `NOT IN` so the
    /// planner can prove the predicate implies the partial index
    /// predicate (`migrations/004_recovery.sql:85`). Same exclusion
    /// set as `sweep_stale_live_pins`.
    ///
    /// LEFT JOIN to the active `assignments` row pulls `exec_id` (the
    /// recovery carrier — `migrations/061`). `assignments_active_uq`
    /// (`migrations/001_scheduler.sql:98`, a partial UNIQUE index on
    /// `derivation_id WHERE status IN ('pending', 'acknowledged')`)
    /// guarantees at most one active assignment per derivation, so the
    /// join cannot fan out.
    ///
    /// The join additionally requires `d.assigned_builder_id IS NOT NULL`
    /// so `exec_id` is NULL for any drv that is not *currently*
    /// dispatched. A reset drv's `assignments` row stays open at
    /// `pending` (`terminal_assignment_status(Ready) == None` — see
    /// `db/derivations.rs`), so without the guard the dead execution's
    /// `exec_id` would re-stamp `state.exec_id` after failover, undoing
    /// `reset_to_ready()`'s documented clear. `assigned_builder_id IS
    /// NOT NULL` ⟺ currently dispatched (the only non-NULL writer is
    /// `record_assignment`); this mirrors the recovery load.s
    /// `Assigned|Running` filter. Full harm
    /// chain: `test_recovery_preserves_reset_exec_id_clear`.
    ///
    /// CAVEAT: this query has NO join to builds. A derivation whose
    /// own status is non-terminal loads even if every build that ever
    /// referenced it is terminal (failed/cancelled). Those orphans get
    /// `interested_builds = ∅` after the build_derivations join in
    /// recover_from_pg, and the I-058 transition pass at recovery.rs
    /// gates on that — DON'T remove that gate without adding a
    /// `WHERE EXISTS (... builds.status IN pending/active)` here. The
    /// gate is the cheaper invariant; this comment is the tripwire.
    pub(crate) async fn load_nonterminal_derivations(
        &self,
    ) -> Result<Vec<RecoveryDerivationRow>, sqlx::Error> {
        sqlx::query_as(terminal_status_sql!(
            r"
            SELECT d.derivation_id, d.drv_hash, d.drv_path, d.pname, d.system, d.status,
                   d.required_features,
                   d.assigned_builder_id,
                   d.expected_output_paths, d.output_names,
                   d.is_fixed_output, d.is_ca,
                   d.floor_mem_bytes, d.floor_disk_bytes, d.floor_deadline_secs,
                   a.exec_id,
                   e.attempt_kind
            FROM derivations d
            LEFT JOIN assignments a ON a.derivation_id = d.derivation_id
                                   AND a.status IN ('pending', 'acknowledged')
                                   AND d.assigned_builder_id IS NOT NULL
            LEFT JOIN drv_executions e ON e.exec_id = a.exec_id
            WHERE d.status NOT IN "
        ))
        .fetch_all(&self.pool)
        .await
    }

    /// Load edges for a set of derivation IDs. Only loads edges where
    /// BOTH endpoints are in the set — an edge to a `completed`/
    /// `skipped` derivation (not in `derivation_ids`) is dropped; the
    /// in-mem DAG treats "no edge" = "no incomplete dependency" =
    /// "ready" (compute_initial_states). This is correct for those
    /// two: a completed/skipped dependency IS satisfied.
    ///
    /// Edges to expired-at-load `poisoned`, `dependency_failed`, and
    /// `cancelled` children are ALSO dropped here (terminal, so not in
    /// the loaded id set; within-TTL `poisoned` children are reloaded
    /// for TTL tracking and keep their edges) — parents whose failed
    /// child is vouched for by a live co-owning build are returned by
    /// [`Self::load_parents_with_failed_deps`] and short-circuited to
    /// `DependencyFailed` in `recompute_recovered_states` BEFORE
    /// `compute_initial_states` runs, so the missing edge can't cause a
    /// wrong `all_deps_completed() == true` promotion; the rest are not
    /// condemned — they keep whatever non-terminal children survive this
    /// load (possibly none) and are re-discovered at dispatch time (see
    /// that query's doc for the evidence rule). The consumption
    /// routing's classifier ([`Self::classify_durable_evidence`]) reads
    /// the persisted graph directly, so the dropped in-memory edges
    /// never launder a routing verdict.
    ///
    /// ANY($1): PG unnest-style array comparison. Scales to ~100k
    /// IDs before the planner starts preferring a temp table; recovery
    /// DAGs are typically <10k nodes.
    pub(crate) async fn load_edges_for_derivations(
        &self,
        derivation_ids: &[Uuid],
    ) -> Result<Vec<(Uuid, Uuid)>, sqlx::Error> {
        if derivation_ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as(
            r#"
            SELECT parent_id, child_id FROM derivation_edges
            WHERE parent_id = ANY($1) AND child_id = ANY($1)
            "#,
        )
        .bind(derivation_ids)
        .fetch_all(&self.pool)
        .await
    }

    // r[impl sched.recovery.failed-dep-cascade+2]
    /// Recovered parents with at least one terminal-**failure**
    /// dependency (`poisoned`/`dependency_failed`/`cancelled`) that is
    /// vouched for by a LIVE build co-owning the parent: the failed
    /// child must carry a `build_derivations` link to a
    /// `'pending'`/`'active'` build that ALSO links the parent.
    ///
    /// [`Self::load_edges_for_derivations`] drops edges to expired-at-load
    /// `poisoned` / `dependency_failed` / `cancelled` children (within-TTL
    /// `poisoned` children are reloaded for TTL tracking, keep their
    /// edges, and are visible to the walk directly), so for the dropped
    /// ones `any_dep_terminally_failed` finds no failed child in the
    /// `children` map and `all_deps_completed` can return `true` →
    /// wrong Ready. This query lets `recompute_recovered_states`
    /// transition them directly to `DependencyFailed` without loading
    /// stub nodes for the failed children. `'skipped'` is NOT a
    /// failure (CA-cutoff — `all_deps_completed` treats it as
    /// satisfied) and `'completed'` is the genuine satisfied case;
    /// neither is matched here.
    ///
    /// The co-ownership + liveness scoping exists because a persisted
    /// failure edge is not, by itself, evidence that THIS parent's
    /// dependency cascade was interrupted mid-crash: a pruning build is
    /// interested in its kept root but never in the root's closure, so
    /// a shared root can carry an edge to a child that went `cancelled`
    /// purely because a DIFFERENT build that owned it was cancelled
    /// (bug_009's shape). Condemning the recovered parent on that
    /// dead-build / cross-build evidence terminally fails a healthy
    /// build whose own substitution or rebuild would have succeeded.
    /// Only a failed child that a still-live owner of the parent
    /// demanded counts.
    ///
    /// Liveness is read at this query's instant, while
    /// `interested_builds` is rebuilt from `builds`/`build_derivations`
    /// reads later in recovery — a build flipping terminal in between
    /// makes the two views disagree (read skew). Both directions err
    /// conservatively: a parent cascaded on a voucher that died moments
    /// later is usually just a terminal row no live build is interested
    /// in (reaped/GC'd as usual; a second live build co-owning the
    /// parent does inherit the verdict, but it is the same
    /// `DependencyFailed` the live in-memory cascade would have handed
    /// it when the vouched child terminally failed), and an
    /// under-cascaded parent keeps only its surviving non-terminal
    /// children (possibly none) and is re-discovered at dispatch time —
    /// a node still carrying an unresolved materialization job is
    /// settled by the job's own consumption routing, and any other
    /// node's from-source dispatch is exactly what its live builds
    /// submitted it for.
    pub(crate) async fn load_parents_with_failed_deps(
        &self,
        derivation_ids: &[Uuid],
    ) -> Result<Vec<Uuid>, sqlx::Error> {
        if derivation_ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_scalar(
            r#"
            SELECT DISTINCT e.parent_id
            FROM derivation_edges e
            JOIN derivations d ON d.derivation_id = e.child_id
            WHERE e.parent_id = ANY($1)
              AND d.status IN ('poisoned', 'dependency_failed', 'cancelled')
              AND EXISTS (SELECT 1 FROM build_derivations bd
                          JOIN builds b ON b.build_id = bd.build_id
                          JOIN build_derivations bdp
                            ON bdp.build_id = bd.build_id
                           AND bdp.derivation_id = e.parent_id
                          WHERE bd.derivation_id = e.child_id
                            AND b.status IN ('pending', 'active'))
            "#,
        )
        .bind(derivation_ids)
        .fetch_all(&self.pool)
        .await
    }

    /// THE durable closure-evidence classifier (T-D2.2 / PD-D4): the
    /// strict three-part criterion — pg.edges + pg.status + LIVE
    /// co-owning build links — as a tri-state classification consumed
    /// by the §2.4 consumption routing and the PD-20 park
    /// re-evaluation. ONE criterion, shared SQL fragments
    /// ([`CHILD_PRODUCED_SQL`]/[`CHILD_LIVE_VOUCHER_SQL`]). (The
    /// walk-era recovery gate evaluated the same criterion as a
    /// batched produced-parents query; it died with the evidence
    /// columns — the classifier is the criterion's only home now.)
    ///
    /// The cell map (design §4's successor to the walk-era hole
    /// breadcrumb, all three conjuncts):
    /// - `Vouched` — ≥1 child ∧ every child produced
    ///   (`completed`/`skipped`) ∧ vouched by a live co-owning build
    ///   (the `EXISTS` conjunct inside the `bool_and`, exactly as the
    ///   recovery gate has it).
    /// - `ChildlessLeaf` — zero durable children: a structural leaf
    ///   (from-source-viable for a non-pruned origin; the origin
    ///   conjunct discriminates the pruned root — merged_bug_301).
    /// - `Holed` — the produced-without-a-live-voucher cell: the
    ///   previous-generation shape (a long-terminal build's completed
    ///   children persist in PG indefinitely; classifying them Vouched
    ///   would launder a stale closure into a doomed from-source
    ///   dispatch — the F9 hazard the third conjunct closes).
    /// - `Pending` — children exist, none of them stale-produced, not
    ///   all produced (the buildable-closure case; normal dep gating).
    ///
    /// NO hole input — the load-bearing protection is the THIRD
    /// conjunct, not append-onlyness: pg.edges IS truncated by
    /// `gc_orphan_terminal_derivations`, and PG's retention of stale
    /// terminal-build rows is precisely why live-build scoping is
    /// required.
    pub(crate) async fn classify_durable_evidence(
        &self,
        derivation_id: Uuid,
    ) -> Result<rio_evidence_kernel::ClosureEvidence, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        Self::classify_durable_evidence_in_tx(&mut conn, derivation_id).await
    }

    /// The in-transaction form (bug_390): the merge-time pruned-origin
    /// gate classifies INSIDE the merge transaction so the verdict and
    /// the job-creation row are crash-atomic — and over the durable
    /// relation, never the reap-truncatable in-memory child set.
    ///
    /// The 4-cell map (merged_bug_301): zero children →
    /// `ChildlessLeaf` (a structural leaf — from-source-viable for a
    /// non-pruned origin); all produced + live-vouched → `Vouched`;
    /// produced-without-a-live-voucher (the previous-generation shape)
    /// → `Holed`; otherwise `Pending`.
    pub(crate) async fn classify_durable_evidence_in_tx(
        conn: &mut sqlx::PgConnection,
        derivation_id: Uuid,
    ) -> Result<rio_evidence_kernel::ClosureEvidence, sqlx::Error> {
        let (n_children, all_strict, stale_produced): (i64, Option<bool>, Option<bool>) =
            sqlx::query_as(CLASSIFY_EVIDENCE_SQL.as_str())
                .bind(derivation_id)
                .fetch_one(conn)
                .await?;
        use rio_evidence_kernel::ClosureEvidence;
        Ok(if n_children == 0 {
            ClosureEvidence::ChildlessLeaf
        } else if all_strict == Some(true) {
            ClosureEvidence::Vouched
        } else if stale_produced == Some(true) {
            ClosureEvidence::Holed
        } else {
            ClosureEvidence::Pending
        })
    }

    /// Load (build_id, derivation_id) links for a set of builds.
    /// `recover_from_pg` uses this to rebuild `interested_builds`
    /// on each DerivationState and `derivation_hashes` on BuildInfo.
    pub(crate) async fn load_build_derivations(
        &self,
        build_ids: &[Uuid],
    ) -> Result<Vec<(Uuid, Uuid)>, sqlx::Error> {
        if build_ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as(
            r#"
            SELECT build_id, derivation_id FROM build_derivations
            WHERE build_id = ANY($1)
            "#,
        )
        .bind(build_ids)
        .fetch_all(&self.pool)
        .await
    }

    /// PG's generation high-water mark: the max over every generation
    /// ever *persisted on an assignment* and every generation ever
    /// *claimed by a leader* — the durable floor that survives the
    /// Lease object (the primary generation source, via
    /// `leaseTransitions`) being deleted and recreated at zero.
    /// `handle_leader_acquired` raises its claim/seed target past this
    /// floor only when the floor exceeds the recovery-entry generation,
    /// or ties it without this holder's own claim row in the ledger; in
    /// every other case (floor below the entry generation, no floor at
    /// all, or our own claim row at the tie) the entry generation is
    /// retained — a same-epoch re-acquire does not burn a generation
    /// (the `sched.recovery.fetch-max-seed` rule in the scheduler spec
    /// is the normative statement).
    ///
    /// Two arms because neither alone is a reliable floor:
    /// - `assignments.generation` covers pre-claim history and
    ///   in-flight work, but it only advances when an assignment
    ///   persists (a leader deposed before its first dispatch leaves
    ///   no trace) and it *decays* — migration 034's `ON DELETE
    ///   CASCADE` plus the orphan-terminal-derivation sweep delete old
    ///   rows, so `MAX(generation)` regresses toward NULL on a
    ///   quiescent cluster.
    /// - `leader_generation_claims` is the append-only ledger of every
    ///   generation handed to dispatch, written at acquire time before
    ///   `recovery_complete` (see [`Self::claim_generation`]). It never
    ///   decays and covers generations that never reached an
    ///   assignment row.
    ///
    /// Workers with a stale generation reject assignments; if a
    /// generation were reused, workers that received old assignments
    /// would ALSO accept new ones from that generation —
    /// dual-processing. Exceeding the floor unless the claims ledger
    /// proves it is this holder's own current epoch bounds that damage
    /// regardless of Lease state (it cannot *prevent* it under a PG
    /// point-in-time restore, which regresses both arms together).
    ///
    /// `GREATEST` of two NULLs is NULL: BIGINT → i64 → u64 cast at the
    /// caller, `None` = fresh cluster (no assignments, no claims).
    pub(crate) async fn max_known_generation(&self) -> Result<Option<i64>, sqlx::Error> {
        let row: (Option<i64>,) = sqlx::query_as(
            r#"
            SELECT GREATEST(
                (SELECT MAX(generation) FROM assignments),
                (SELECT MAX(generation) FROM leader_generation_claims))
            "#,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Durably record that this leader is about to dispatch at
    /// `generation`. Returns `Ok(true)` if the claim row was inserted,
    /// `Ok(false)` if another holder already claimed this exact
    /// generation (the PRIMARY KEY conflict is the CAS — the caller
    /// bumps past [`Self::max_claimed_generation`] and retries).
    ///
    /// Called from `handle_leader_acquired` BEFORE
    /// `set_recovery_complete()` ungates dispatch, so a successor's
    /// [`Self::max_known_generation`] sees this generation even if this
    /// leader is deposed before persisting a single assignment. The
    /// Chubby-sequencer discipline: the epoch is durable before it is
    /// used.
    ///
    /// `holder_id` is the replica's pod identity and is LOAD-BEARING:
    /// the caller's pre-INSERT check and conflict read-back compare it
    /// to distinguish "this row is our own previous claim for the same
    /// epoch" (retain the generation, the claim is already durable)
    /// from "another holder claimed our generation" (the
    /// post-lease-deletion collision; exceed it). No two LIVE processes
    /// ever share a `holder_id` — a container restart within the same
    /// pod reuses `HOSTNAME`, but the predecessor is dead before the
    /// successor starts.
    // r[impl sched.lease.generation-claim+2]
    pub(crate) async fn claim_generation(
        &self,
        generation: i64,
        holder_id: &str,
    ) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(
            "INSERT INTO leader_generation_claims (generation, holder_id) \
             VALUES ($1, $2) ON CONFLICT (generation) DO NOTHING",
        )
        .bind(generation)
        .bind(holder_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// The highest claim row in the ledger: `(generation, holder_id)`,
    /// or `None` if no leader has ever claimed. Two callers, both in
    /// `handle_leader_acquired`'s claim path:
    ///
    /// - the pre-INSERT check — when the PG floor lands exactly on the
    ///   recovery-entry generation, the holder of the row at that
    ///   generation decides between "our own previous claim for this
    ///   same epoch; retain it, the claim is already durable" and
    ///   "anything else; exceed it" (another holder's row is a
    ///   post-lease-deletion collision, and an absent row at that
    ///   generation reads as foreign too — an assignments-only floor
    ///   has no holder identity to affirm);
    /// - the conflict-retry path — a `false` return from
    ///   [`Self::claim_generation`] means someone owns that exact
    ///   generation; if it is us the claim is idempotent, otherwise the
    ///   claimer re-targets past this row's generation.
    pub(crate) async fn max_claimed_generation(
        &self,
    ) -> Result<Option<(i64, String)>, sqlx::Error> {
        sqlx::query_as(
            "SELECT generation, holder_id FROM leader_generation_claims \
             ORDER BY generation DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// Load the build's derivation subgraph for dashboard DAG viz
    /// (`AdminService.GetBuildGraph`). PG-backed, not actor-snapshot —
    /// completed builds have no actor state, but PG persists the full graph.
    ///
    /// Returns `(nodes, edges, total_nodes)`. `total_nodes` is the
    /// un-limited count so the caller can compute `truncated = total > limit`.
    ///
    /// Subgraph projection: both endpoints of every returned edge are in
    /// THIS build's `build_derivations` set. A derivation shared between
    /// two builds appears in both builds' node sets, but an edge to a
    /// derivation owned by a DIFFERENT build is excluded. The dashboard
    /// sees only the DAG the user submitted, not the global DAG it was
    /// merged into.
    ///
    /// 3 roundtrips (count, nodes, edges) — no transaction. Worst case
    /// under concurrent writes: `total` is slightly stale (off by one
    /// if a derivation was added between COUNT and SELECT). Acceptable
    /// for a 5s-poll dashboard; builds don't add derivations post-submit
    /// anyway.
    // NOTE(fault-line): load_build_graph lives here because it was
    // physically inside the pre-split :1123 "recovery" banner. It is not
    // actually a recovery-on-LeaderAcquired query — it serves
    // AdminService.GetBuildGraph (dashboard viz). If a future plan grows
    // this family, consider extracting to db/admin_reads.rs. The
    // r[impl dash.graph.degrade-threshold] marker at the 5000-node cap
    // IS correctly placed (scheduler implements server-side cap
    // regardless of which db/ file hosts it).
    pub(crate) async fn load_build_graph(
        &self,
        build_id: Uuid,
        limit: u32,
    ) -> Result<(Vec<GraphNodeRow>, Vec<GraphEdgeRow>, u32), sqlx::Error> {
        let total: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM build_derivations WHERE build_id = $1")
                .bind(build_id)
                .fetch_one(&self.pool)
                .await?;

        // COALESCE for nullable columns (pname, assigned_builder_id) →
        // proto3 non-optional string is empty-string-for-null.
        // derivation_id is carried so the edge query below can filter
        // to THIS returned set (not the whole build).
        //
        // bd.exec_id is the build↔exec observation recorded by the
        // completion handler on terminal paths where an execution ran
        // (Completed, Poisoned, Cancelled from Assigned/Running, and any
        // terminal reached while a prior, reset execution's stamped log
        // buffer is retained — build-cancel sweep, failed-substitute
        // revert, or dependency-failure cascade) — see
        // r[sched.merge.exec-correlation+8]. It comes from the JOIN'd
        // `build_derivations` edge (already in the query), not a new
        // table; nullable, NOT COALESCE'd (the proto layer maps None →
        // empty string and the dashboard treats empty as "fall back to
        // latest exec").
        let nodes: Vec<GraphNodeRow> = sqlx::query_as(
            r#"
            SELECT d.derivation_id,
                   d.drv_path,
                   COALESCE(d.pname, '') AS pname,
                   d.system,
                   d.status,
                   COALESCE(d.assigned_builder_id, '') AS assigned_builder_id,
                   bd.exec_id
            FROM derivations d
            JOIN build_derivations bd ON bd.derivation_id = d.derivation_id
            WHERE bd.build_id = $1
            ORDER BY d.drv_path
            LIMIT $2
            "#,
        )
        .bind(build_id)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;

        // r[impl dash.graph.degrade-threshold]
        // Edge set MUST be a subgraph of the RETURNED node set, not the
        // whole build. When the node query truncates at LIMIT, edges
        // referencing truncated nodes would be dangling — the client's
        // lookup-by-drv_path misses, either crashing the renderer or
        // silently dropping the edge (both wrong).
        //
        // ANY($1) on both endpoints with node_ids = the actual returned
        // derivation_ids is both the correctness fix AND an implicit
        // bound: edge count is bounded by the induced subgraph over
        // ≤5000 nodes, not the whole-build DAG. A sparse build DAG
        // (typical fanout: 3-4× node count) caps naturally at ~20k.
        //
        // If nodes didn't truncate, node_ids IS the full build set —
        // same rows as the old `WHERE IN (SELECT ... WHERE build_id)`
        // subquery, but now it's impossible to return a dangling ref.
        //
        // retro P0027: dropped e.is_cutoff (always FALSE; Skipped is
        // a node status, carried in GraphNode.status).
        let node_ids: Vec<Uuid> = nodes.iter().map(|n| n.derivation_id).collect();
        let edges: Vec<GraphEdgeRow> = if node_ids.is_empty() {
            // Unknown build or zero-derivation build — skip the roundtrip.
            Vec::new()
        } else {
            sqlx::query_as(
                r#"
                SELECT dp.drv_path AS parent_drv_path,
                       dc.drv_path AS child_drv_path
                FROM derivation_edges e
                JOIN derivations dp ON dp.derivation_id = e.parent_id
                JOIN derivations dc ON dc.derivation_id = e.child_id
                WHERE e.parent_id = ANY($1)
                  AND e.child_id  = ANY($1)
                "#,
            )
            .bind(&node_ids)
            .fetch_all(&self.pool)
            .await?
        };

        // Operators spot builds approaching the implicit edge bound via
        // the p99 of this histogram. A build with consistently high edge
        // count (>10k) on a 5000-node cap is unusually dense and worth
        // a look — either a legitimately weird closure or a DAG-merge bug.
        metrics::histogram!("rio_scheduler_build_graph_edges").record(edges.len() as f64);

        Ok((nodes, edges, total as u32))
    }
}
