//! Phase 3b: state recovery read queries.
//!
//! Called by recover_from_pg() on LeaderAcquired transition. Loads
//! all non-terminal builds + derivations + edges + build_derivations +
//! assignments, from which the actor rebuilds its in-mem DAG.
//!
//! FromRow structs (not tuples): recovery needs ~10 fields per
//! derivation, tuples at that arity are error-prone (wrong-field
//! assignment). #[derive(FromRow)] + named columns is safer.

use sqlx::PgPool;
use uuid::Uuid;

use super::{
    GraphEdgeRow, GraphNodeRow, RecoveryBuildRow, RecoveryDerivationRow, SchedulerDb,
    terminal_status_sql,
};

impl SchedulerDb {
    /// Load all non-terminal builds. Terminal builds (succeeded/
    /// failed/cancelled) don't need recovery — they're done, any
    /// WatchBuild subscriber has already received the terminal event
    /// (or will time out waiting, which is the same as "scheduler
    /// restarted and forgot").
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
    /// `record_assignment`); this mirrors the LogBuffers restamp gate's
    /// `Assigned|Running` filter in `load_dag_from_rows`. Full harm
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
                   d.retry_count, d.resubmit_cycles,
                   d.expected_output_paths, d.output_names,
                   d.wanted_output_names, d.is_fixed_output,
                   d.is_ca, d.topdown_pruned, d.closure_hole,
                   d.failed_builders,
                   d.floor_mem_bytes, d.floor_disk_bytes, d.floor_deadline_secs,
                   a.exec_id
            FROM derivations d
            LEFT JOIN assignments a ON a.derivation_id = d.derivation_id
                                   AND a.status IN ('pending', 'acknowledged')
                                   AND d.assigned_builder_id IS NOT NULL
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
    /// Edges to `poisoned`/`dependency_failed`/`cancelled` children are
    /// ALSO dropped here (they're terminal too) — parents whose failed
    /// child is vouched for by a live co-owning build are returned by
    /// [`Self::load_parents_with_failed_deps`] and short-circuited to
    /// `DependencyFailed` in `seed_ready_queue` BEFORE
    /// `compute_initial_states` runs, so the missing edge can't cause a
    /// wrong `all_deps_completed() == true` promotion; the rest
    /// deliberately recover childless and are re-discovered at dispatch
    /// time (see that query's doc for the evidence rule).
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

    // r[impl sched.recovery.failed-dep-cascade]
    /// Recovered parents with at least one terminal-**failure**
    /// dependency (`poisoned`/`dependency_failed`/`cancelled`) that is
    /// vouched for by a LIVE build co-owning the parent: the failed
    /// child must carry a `build_derivations` link to a
    /// `'pending'`/`'active'` build that ALSO links the parent.
    ///
    /// [`Self::load_edges_for_derivations`] drops edges to ALL terminal
    /// children, so `any_dep_terminally_failed` walks an empty
    /// `children` map for these parents and `all_deps_completed`
    /// returns `true` → wrong Ready. This query lets `seed_ready_queue`
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
    /// under-cascaded parent recovers childless and is re-discovered at
    /// dispatch time — the must-substitute guards keep a marked node
    /// off the doomed from-source path, and an unmarked node's
    /// from-source dispatch is exactly what its own live builds
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

    /// Recovered parents whose persisted children are ALL produced
    /// (`'completed' | 'skipped'`) AND vouched for by a live build that
    /// also owns the parent: each child must carry a
    /// `build_derivations` link to a `'pending'`/`'active'` build that
    /// ALSO links the parent. This is the PG mirror of the in-memory
    /// closure-evidence judgment
    /// ([`crate::dag::ClosureEvidence::Vouched`], computed by
    /// [`crate::dag::DerivationDag::closure_evidence`] and surfaced to
    /// the actor as `closure_vouched`) every other `topdown_pruned`
    /// clear site routes through — that judgment is computed over the
    /// live graph, so its PG mirror must not accept produced rows whose
    /// only evidence is a long-terminal build or a build that never
    /// owned the parent.
    ///
    /// Consumed by the recovery-time `topdown_pruned` gate in
    /// `load_dag_from_rows`: produced children are excluded from
    /// [`Self::load_nonterminal_derivations`] and their edges are
    /// dropped by [`Self::load_edges_for_derivations`], so this check
    /// can only be answered against the persisted graph. Childless rows
    /// are never returned (no `derivation_edges` rows → no GROUP BY
    /// group), so a genuine never-merged pruned root keeps its restored
    /// flag. The live-build scoping exists for the previous-generation
    /// case: a drv fully built by an old build keeps its edges, its
    /// `completed` children and that build's `build_derivations` links
    /// in PG indefinitely after the build goes terminal, while the
    /// store may GC the actual outputs at any point — when a later
    /// build re-requests the drv via the prune (no new edges, mark
    /// stamped), those historical rows are stale evidence and must NOT
    /// clear the restored mark. The co-ownership requirement closes the
    /// cross-build half of the same hole: a pruning build links only
    /// its kept roots, never the children, so only a full-merge owner
    /// of the parent can vouch — produced children belonging to some
    /// unrelated live build must not launder a clear for a parent that
    /// build never owned. Such parents keep the flag and at worst take
    /// the bounded resubmit-directing fail-fast — never the doomed
    /// from-source dispatch of a closure that was never merged. Any
    /// unbuilt / `failed` / `cancelled` / `poisoned` /
    /// `dependency_failed` child — and any produced child without a
    /// live co-owning voucher — fails the `bool_and`, so the
    /// must-substitute guard is kept for those parents too.
    pub(crate) async fn load_parents_with_all_children_produced(
        &self,
        derivation_ids: &[Uuid],
    ) -> Result<Vec<Uuid>, sqlx::Error> {
        if derivation_ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_scalar(
            r#"
            SELECT e.parent_id
            FROM derivation_edges e
            JOIN derivations c ON c.derivation_id = e.child_id
            WHERE e.parent_id = ANY($1)
            GROUP BY e.parent_id
            HAVING bool_and(
                c.status IN ('completed', 'skipped')
                AND EXISTS (SELECT 1 FROM build_derivations bd
                            JOIN builds b ON b.build_id = bd.build_id
                            JOIN build_derivations bdp
                              ON bdp.build_id = bd.build_id
                             AND bdp.derivation_id = e.parent_id
                            WHERE bd.derivation_id = c.derivation_id
                              AND b.status IN ('pending', 'active'))
            )
            "#,
        )
        .bind(derivation_ids)
        .fetch_all(&self.pool)
        .await
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

    /// Max assignment generation ever written. recover_from_pg()
    /// seeds its generation counter from this + 1 — defensive
    /// monotonicity guard in case the Lease's generation (in its
    /// annotation) was lost/reset (e.g., someone `kubectl delete
    /// lease`). Workers with a stale generation reject assignments;
    /// if we accidentally reused a gen, workers that received old
    /// assignments would ALSO accept new ones from that gen —
    /// dual-processing. Seeding from PG's high-water mark prevents
    /// that regardless of Lease state.
    ///
    /// BIGINT → i64 → u64 cast at the caller. `None` = no
    /// assignments ever (fresh cluster).
    pub async fn max_assignment_generation(&self) -> Result<Option<i64>, sqlx::Error> {
        let row: (Option<i64>,) = sqlx::query_as("SELECT MAX(generation) FROM assignments")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0)
    }

    /// Max sequence number per build_id from build_event_log.
    /// recover_from_pg() seeds `build_sequences` from this so new
    /// events continue from where the old leader left off — a
    /// reconnecting WatchBuild client with `since_sequence=N` would
    /// miss events if we reset to 0 and emitted new events with
    /// seq=1 (<N → filtered by the client).
    ///
    /// Only for builds that are still active (caller filters by
    /// build_ids from load_nonterminal_builds).
    pub async fn max_sequence_per_build(
        &self,
        build_ids: &[Uuid],
    ) -> Result<Vec<(Uuid, i64)>, sqlx::Error> {
        if build_ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as(
            r#"
            SELECT build_id, MAX(sequence) FROM build_event_log
            WHERE build_id = ANY($1) GROUP BY build_id
            "#,
        )
        .bind(build_ids)
        .fetch_all(&self.pool)
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
    // NOTE(fault-line): load_build_graph + read_event_log live here
    // because they were physically inside the pre-split :1123 "recovery"
    // banner. Neither is actually a recovery-on-LeaderAcquired query —
    // load_build_graph serves AdminService.GetBuildGraph (dashboard viz),
    // read_event_log serves grpc bridge replay. If a future plan touches
    // both, consider extracting to db/admin_reads.rs. The
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
        // r[sched.merge.exec-correlation+7]. It comes from the JOIN'd
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

/// Read persisted build events for since_sequence replay.
///
/// Returns events in the half-open range `(since, until]` — strictly
/// after `since` (the gateway's last-seen seq), at most `until`
/// (the actor's last-emitted seq at subscribe time). `until` bounds
/// the replay so we don't duplicate what the broadcast will carry:
/// everything with seq > until was emitted AFTER subscribe and is
/// guaranteed to be on the broadcast channel.
///
/// Free fn (not `SchedulerDb` method): `bridge_build_events` is a
/// free fn in grpc/mod.rs that only has a `PgPool`, not a
/// `SchedulerDb`. Adding `SchedulerDb` to `SchedulerGrpc` would
/// drag the whole db module into grpc; a bare pool is cheaper.
///
/// u64 → i64 cast: PG BIGINT is signed. See event_log.rs for the
/// same rationale (2^63 events per build is not a real concern).
///
/// Returns a row stream (`.fetch`, not `.fetch_all`): a fresh
/// `WatchBuild{since_sequence:0}` against a 153k-node DAG would
/// otherwise materialize ≥300k rows × ~400B into a `Vec` per
/// concurrent watcher BEFORE the bridge's mpsc(256) backpressure can
/// apply. The caller pins the stream and forwards row-by-row.
pub fn read_event_log(
    pool: &PgPool,
    build_id: Uuid,
    since: u64,
    until: u64,
) -> impl futures_util::Stream<Item = Result<(u64, Vec<u8>), sqlx::Error>> + '_ {
    use futures_util::TryStreamExt;
    sqlx::query_as::<_, (i64, Vec<u8>)>(
        "SELECT sequence, event_bytes FROM build_event_log \
         WHERE build_id = $1 AND sequence > $2 AND sequence <= $3 \
         ORDER BY sequence",
    )
    .bind(build_id)
    .bind(since as i64)
    .bind(until as i64)
    .fetch(pool)
    // i64 → u64: rows were written with `seq as i64` from a u64, so
    // they round-trip exactly. No values < 0 exist (seq starts at 1).
    .map_ok(|(s, b)| (s as u64, b))
}
