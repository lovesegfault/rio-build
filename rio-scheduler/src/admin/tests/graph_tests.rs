//! `GetBuildGraph` RPC tests + `seed_drv`/`seed_build`/`link`/`edge`
//! PG-seeding helpers.
//!
//! Split from the 1732L monolithic `admin/tests.rs` (P0386) to mirror the
//! `admin/graph.rs` submodule seam. Largest standalone cluster in the
//! original monolith (~330L, ~20% of file).

use super::*;

/// Seed a derivation row directly. Returns `derivation_id`.
/// Raw INSERT — the graph query only needs 5 columns (drv_path, pname,
/// system, status, assigned_builder_id). Everything else defaults.
async fn seed_drv(
    pool: &sqlx::PgPool,
    drv_hash: &str,
    drv_path: &str,
    status: &str,
    worker: Option<&str>,
) -> anyhow::Result<uuid::Uuid> {
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO derivations (drv_hash, drv_path, pname, system, status, assigned_builder_id)
         VALUES ($1, $2, 'pkg', 'x86_64-linux', $3, $4)
         RETURNING derivation_id",
    )
    .bind(drv_hash)
    .bind(drv_path)
    .bind(status)
    .bind(worker)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

async fn seed_build(pool: &sqlx::PgPool) -> anyhow::Result<uuid::Uuid> {
    let id = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO builds (build_id, status) VALUES ($1, 'active')")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn link(pool: &sqlx::PgPool, build_id: uuid::Uuid, drv_id: uuid::Uuid) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
        .bind(build_id)
        .bind(drv_id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn edge(pool: &sqlx::PgPool, parent: uuid::Uuid, child: uuid::Uuid) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO derivation_edges (parent_id, child_id) VALUES ($1, $2)")
        .bind(parent)
        .bind(child)
        .execute(pool)
        .await?;
    Ok(())
}

/// 3 nodes, 2 edges, one build → correct shape, truncated=false.
/// Verifies: node count, edge count, status round-trip, assigned_builder_id
/// COALESCE, edge direction.
#[tokio::test]
async fn get_build_graph_basic_shape() -> anyhow::Result<()> {
    let (svc, _actor, _task, db) = setup_svc_default().await;
    let pool = &db.pool;

    //   a
    //  / \     a depends on b and c (parent=a, child=b/c)
    // b   c
    let build = seed_build(pool).await?;
    let a = seed_drv(
        pool,
        "hash-a",
        "/nix/store/aaa-a.drv",
        "running",
        Some("w1"),
    )
    .await?;
    let b = seed_drv(pool, "hash-b", "/nix/store/bbb-b.drv", "completed", None).await?;
    let c = seed_drv(pool, "hash-c", "/nix/store/ccc-c.drv", "queued", None).await?;
    link(pool, build, a).await?;
    link(pool, build, b).await?;
    link(pool, build, c).await?;
    edge(pool, a, b).await?;
    edge(pool, a, c).await?;

    let resp = svc
        .get_build_graph(Request::new(rio_proto::types::GetBuildGraphRequest {
            build_id: build.to_string(),
        }))
        .await?
        .into_inner();

    assert_eq!(resp.nodes.len(), 3);
    assert_eq!(resp.edges.len(), 2);
    assert_eq!(resp.total_nodes, 3);
    assert!(!resp.truncated);

    // ORDER BY drv_path → a, b, c.
    assert_eq!(resp.nodes[0].drv_path, "/nix/store/aaa-a.drv");
    assert_eq!(resp.nodes[0].status, "running");
    assert_eq!(resp.nodes[0].assigned_executor_id, "w1");
    assert_eq!(resp.nodes[0].pname, "pkg");
    assert_eq!(resp.nodes[0].system, "x86_64-linux");

    assert_eq!(resp.nodes[1].drv_path, "/nix/store/bbb-b.drv");
    assert_eq!(resp.nodes[1].status, "completed");
    assert_eq!(
        resp.nodes[1].assigned_executor_id, "",
        "NULL assigned_builder_id → COALESCE to empty string"
    );

    // Both edges have parent=a. Find each.
    let to_b = resp
        .edges
        .iter()
        .find(|e| e.child_drv_path == "/nix/store/bbb-b.drv")
        .expect("edge a→b");
    assert_eq!(to_b.parent_drv_path, "/nix/store/aaa-a.drv");
    let to_c = resp
        .edges
        .iter()
        .find(|e| e.child_drv_path == "/nix/store/ccc-c.drv")
        .expect("edge a→c");
    assert_eq!(to_c.parent_drv_path, "/nix/store/aaa-a.drv");

    Ok(())
}

/// Two builds sharing a derivation → each sees only its subgraph.
/// The shared drv appears in both node sets, but edges to drvs owned
/// ONLY by the other build are excluded (both-endpoints-in-build rule).
#[tokio::test]
async fn get_build_graph_subgraph_scoping() -> anyhow::Result<()> {
    let (svc, _actor, _task, db) = setup_svc_default().await;
    let pool = &db.pool;

    // build1: {a, shared}   edge a→shared
    // build2: {b, shared}   edge b→shared
    // Global DAG also has edge a→b (cross-build). Build1 should NOT see it.
    let build1 = seed_build(pool).await?;
    let build2 = seed_build(pool).await?;
    let a = seed_drv(pool, "hash-a", "/nix/store/aaa-a.drv", "queued", None).await?;
    let b = seed_drv(pool, "hash-b", "/nix/store/bbb-b.drv", "queued", None).await?;
    let shared = seed_drv(
        pool,
        "hash-s",
        "/nix/store/sss-shared.drv",
        "completed",
        None,
    )
    .await?;

    link(pool, build1, a).await?;
    link(pool, build1, shared).await?;
    link(pool, build2, b).await?;
    link(pool, build2, shared).await?;

    edge(pool, a, shared).await?; // in build1's subgraph
    edge(pool, b, shared).await?; // in build2's subgraph
    edge(pool, a, b).await?; // CROSS-BUILD: a in build1, b in build2 only

    // Build1: sees {a, shared}, edge a→shared. NOT edge a→b (b ∉ build1).
    let r1 = svc
        .get_build_graph(Request::new(rio_proto::types::GetBuildGraphRequest {
            build_id: build1.to_string(),
        }))
        .await?
        .into_inner();
    assert_eq!(r1.nodes.len(), 2, "build1: a + shared");
    assert_eq!(r1.edges.len(), 1, "build1: only a→shared (a→b excluded)");
    assert_eq!(r1.edges[0].parent_drv_path, "/nix/store/aaa-a.drv");
    assert_eq!(r1.edges[0].child_drv_path, "/nix/store/sss-shared.drv");
    // Shared drv appears in build1's nodes.
    assert!(
        r1.nodes
            .iter()
            .any(|n| n.drv_path == "/nix/store/sss-shared.drv")
    );

    // Build2: sees {b, shared}, edge b→shared. NOT edge a→b (a ∉ build2).
    let r2 = svc
        .get_build_graph(Request::new(rio_proto::types::GetBuildGraphRequest {
            build_id: build2.to_string(),
        }))
        .await?
        .into_inner();
    assert_eq!(r2.nodes.len(), 2, "build2: b + shared");
    assert_eq!(r2.edges.len(), 1, "build2: only b→shared");
    assert_eq!(r2.edges[0].parent_drv_path, "/nix/store/bbb-b.drv");

    Ok(())
}

/// limit=2 on 3-node build → truncated=true, total_nodes=3, len(nodes)=2.
/// Uses graph::get_build_graph directly (limit_override) rather than
/// seeding 5001 rows to trip DASHBOARD_GRAPH_NODE_LIMIT.
#[tokio::test]
async fn get_build_graph_truncation() -> anyhow::Result<()> {
    let db = TestDb::new(&crate::MIGRATOR).await;
    let pool = &db.pool;

    let build = seed_build(pool).await?;
    let a = seed_drv(pool, "hash-a", "/nix/store/aaa-a.drv", "created", None).await?;
    let b = seed_drv(pool, "hash-b", "/nix/store/bbb-b.drv", "created", None).await?;
    let c = seed_drv(pool, "hash-c", "/nix/store/ccc-c.drv", "created", None).await?;
    link(pool, build, a).await?;
    link(pool, build, b).await?;
    link(pool, build, c).await?;

    let sched_db = crate::db::SchedulerDb::new(pool.clone());
    let resp = crate::admin::graph::get_build_graph(&sched_db, &build.to_string(), Some(2)).await?;

    assert_eq!(resp.nodes.len(), 2, "limit=2 caps returned nodes");
    assert_eq!(resp.total_nodes, 3, "total_nodes reports un-limited count");
    assert!(resp.truncated, "total(3) > limit(2) → truncated");

    // Boundary: limit == total → NOT truncated.
    let resp = crate::admin::graph::get_build_graph(&sched_db, &build.to_string(), Some(3)).await?;
    assert_eq!(resp.nodes.len(), 3);
    assert_eq!(resp.total_nodes, 3);
    assert!(!resp.truncated, "total(3) == limit(3) → not truncated");

    Ok(())
}

// r[verify dash.graph.degrade-threshold]
/// Truncation WITH edges seeded: edges referencing truncated nodes
/// MUST NOT appear in the response (no dangling references).
///
/// The plain `get_build_graph_truncation` test above seeds zero edges,
/// so the dangling-edge case — edge query was both-endpoints-in-build
/// but NOT both-endpoints-in-returned-set — was invisible to the suite.
///
/// 3-node chain a→b→c, limit=2. ORDER BY drv_path returns {a, b}, drops c.
/// Edge a→b: both endpoints in returned set → kept.
/// Edge b→c: c NOT in returned set → MUST be filtered.
/// Expect exactly 1 edge. If the edge query still filters on build_id
/// (not returned-set), this test gets 2 edges and the dangling assertion
/// fires on b→c.
#[tokio::test]
async fn get_build_graph_truncated_no_dangling_edges() -> anyhow::Result<()> {
    let db = TestDb::new(&crate::MIGRATOR).await;
    let pool = &db.pool;

    let build = seed_build(pool).await?;
    // drv_paths chosen so ORDER BY drv_path is deterministic: a < b < c.
    let a = seed_drv(pool, "hash-a", "/nix/store/aaa-a.drv", "created", None).await?;
    let b = seed_drv(pool, "hash-b", "/nix/store/bbb-b.drv", "created", None).await?;
    let c = seed_drv(pool, "hash-c", "/nix/store/ccc-c.drv", "created", None).await?;
    link(pool, build, a).await?;
    link(pool, build, b).await?;
    link(pool, build, c).await?;
    // Chain: a→b, b→c. 3 nodes, 2 edges.
    edge(pool, a, b).await?;
    edge(pool, b, c).await?;

    let sched_db = crate::db::SchedulerDb::new(pool.clone());
    let resp = crate::admin::graph::get_build_graph(&sched_db, &build.to_string(), Some(2)).await?;

    assert!(resp.truncated);
    assert_eq!(resp.total_nodes, 3);
    assert_eq!(resp.nodes.len(), 2, "limit=2 caps returned nodes");

    // Every edge endpoint is in the returned node set — no dangling.
    let returned: std::collections::HashSet<&str> =
        resp.nodes.iter().map(|n| n.drv_path.as_str()).collect();
    for e in &resp.edges {
        assert!(
            returned.contains(e.parent_drv_path.as_str()),
            "dangling edge: parent {} not in returned nodes {returned:?}",
            e.parent_drv_path
        );
        assert!(
            returned.contains(e.child_drv_path.as_str()),
            "dangling edge: child {} not in returned nodes {returned:?}",
            e.child_drv_path
        );
    }

    // Exact count proves the fix filters ONLY the dangling edge, no more.
    // Chain of N nodes has N-1 edges; truncate 1 node → exactly 1 edge
    // touches the excluded node → N-2 edges returned. Here: 3-1-1 = 1.
    // Under the old query this would be 2 (both a→b and b→c, since both
    // pairs are in the BUILD even though c isn't in the RESPONSE).
    assert_eq!(
        resp.edges.len(),
        1,
        "chain of 3, limit 2: edge b→c is dangling (c excluded), \
         edge a→b survives. If this is 2, the edge query is still \
         filtering on build_id not returned-node-set."
    );
    assert_eq!(resp.edges[0].parent_drv_path, "/nix/store/aaa-a.drv");
    assert_eq!(resp.edges[0].child_drv_path, "/nix/store/bbb-b.drv");

    // Boundary: limit == total → full chain returned, no filtering.
    let resp = crate::admin::graph::get_build_graph(&sched_db, &build.to_string(), Some(3)).await?;
    assert!(!resp.truncated);
    assert_eq!(
        resp.edges.len(),
        2,
        "no truncation → node_ids IS the full build set → \
         all in-build edges returned (same as old behavior)"
    );

    Ok(())
}

#[tokio::test]
async fn get_build_graph_bad_uuid() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;
    let err = svc
        .get_build_graph(Request::new(rio_proto::types::GetBuildGraphRequest {
            build_id: "not-a-uuid".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    Ok(())
}

#[tokio::test]
async fn get_build_graph_unknown_build_empty() -> anyhow::Result<()> {
    // Unknown build_id → empty graph, not NotFound. The query just
    // returns zero rows. Dashboard interprets empty-with-total=0 as
    // "build doesn't exist or has no derivations" — same rendering.
    let (svc, _actor, _task, _db) = setup_svc_default().await;
    let resp = svc
        .get_build_graph(Request::new(rio_proto::types::GetBuildGraphRequest {
            build_id: uuid::Uuid::new_v4().to_string(),
        }))
        .await?
        .into_inner();
    assert!(resp.nodes.is_empty());
    assert!(resp.edges.is_empty());
    assert_eq!(resp.total_nodes, 0);
    assert!(!resp.truncated);
    Ok(())
}
