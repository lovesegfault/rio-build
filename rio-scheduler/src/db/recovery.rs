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
    TERMINAL_STATUS_SQL,
};

impl SchedulerDb {
    /// Load all non-terminal builds. Terminal builds (succeeded/
    /// failed/cancelled) don't need recovery — they're done, any
    /// WatchBuild subscriber has already received the terminal event
    /// (or will time out waiting, which is the same as "scheduler
    /// restarted and forgot").
    pub async fn load_nonterminal_builds(&self) -> Result<Vec<RecoveryBuildRow>, sqlx::Error> {
        sqlx::query_as(
            r#"
            SELECT build_id, tenant_id, status, priority_class,
                   keep_going, options_json
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
    pub async fn load_nonterminal_derivations(
        &self,
    ) -> Result<Vec<RecoveryDerivationRow>, sqlx::Error> {
        sqlx::query_as(&format!(
            r"
            SELECT derivation_id, drv_hash, drv_path, pname, system, status,
                   required_features, assigned_worker_id, retry_count,
                   expected_output_paths, output_names, is_fixed_output,
                   is_ca, failed_workers
            FROM derivations
            WHERE status NOT IN {TERMINAL_STATUS_SQL}
            "
        ))
        .fetch_all(&self.pool)
        .await
    }

    /// Load edges for a set of derivation IDs. Only loads edges
    /// where BOTH endpoints are in the set — an edge to a completed
    /// derivation (not in `derivation_ids`) is dropped; the in-mem
    /// DAG treats "no edge" = "no incomplete dependency" = "ready"
    /// (compute_initial_states). This is correct: a completed
    /// dependency IS satisfied.
    ///
    /// ANY($1): PG unnest-style array comparison. Scales to ~100k
    /// IDs before the planner starts preferring a temp table; recovery
    /// DAGs are typically <10k nodes.
    pub async fn load_edges_for_derivations(
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

    /// Load (build_id, derivation_id) links for a set of builds.
    /// `recover_from_pg` uses this to rebuild `interested_builds`
    /// on each DerivationState and `derivation_hashes` on BuildInfo.
    pub async fn load_build_derivations(
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
    pub async fn load_build_graph(
        &self,
        build_id: Uuid,
        limit: u32,
    ) -> Result<(Vec<GraphNodeRow>, Vec<GraphEdgeRow>, u32), sqlx::Error> {
        let total: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM build_derivations WHERE build_id = $1")
                .bind(build_id)
                .fetch_one(&self.pool)
                .await?;

        // COALESCE for nullable columns (pname, assigned_worker_id) →
        // proto3 non-optional string is empty-string-for-null.
        // derivation_id is carried so the edge query below can filter
        // to THIS returned set (not the whole build).
        let nodes: Vec<GraphNodeRow> = sqlx::query_as(
            r#"
            SELECT d.derivation_id,
                   d.drv_path,
                   COALESCE(d.pname, '') AS pname,
                   d.system,
                   d.status,
                   COALESCE(d.assigned_worker_id, '') AS assigned_worker_id
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
pub async fn read_event_log(
    pool: &PgPool,
    build_id: Uuid,
    since: u64,
    until: u64,
) -> Result<Vec<(u64, Vec<u8>)>, sqlx::Error> {
    let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT sequence, event_bytes FROM build_event_log \
         WHERE build_id = $1 AND sequence > $2 AND sequence <= $3 \
         ORDER BY sequence",
    )
    .bind(build_id)
    .bind(since as i64)
    .bind(until as i64)
    .fetch_all(pool)
    .await?;
    // i64 → u64: rows were written with `seq as i64` from a u64, so
    // they round-trip exactly. No values < 0 exist (seq starts at 1).
    Ok(rows.into_iter().map(|(s, b)| (s as u64, b)).collect())
}
