# Plan 0312: Bounded edge query + truncation correctness — P0276 follow-up

Promoted from perf-batch to standalone: [P0276](plan-0276-getbuildgraph-rpc-pg-backed.md)'s edge query at [`db.rs:1276`](../../rio-scheduler/src/db.rs) (p276 worktree) has two issues, one perf and one **correctness**.

**Perf:** The node query ([`db.rs:1261`](../../rio-scheduler/src/db.rs), p276) caps at `LIMIT $2` (5000 per `r[dash.graph.degrade-threshold]`). The edge query has **no LIMIT**. A dense build (chromium-scale: N nodes, ~N² potential edges, realistically 3-4N) returns unbounded edge rows. A 5000-node build with 4× edge density = 20k edges × ~100 bytes/row = 2MB; a pathological case with 50k edges in a single build = 5MB+. Defeats the thin-message goal.

**Correctness (promoted this to standalone):** When the node query truncates at 5000 but the edge query returns ALL edges for the build (both-endpoints-in-build, but NOT both-endpoints-in-returned-node-set), edges referencing truncated nodes become **dangling**: `edge.parent_drv_path` or `edge.child_drv_path` won't match any `GraphNode.drv_path` the client received. The dashboard's graph renderer either crashes (null deref on lookup) or silently drops the edge (misleading: looks like no dependency).

**The truncation test seeds zero edges** — the P0276 test harness inserts 5001 nodes to prove `truncated=true` but doesn't insert any `derivation_edges` rows, so the dangling-edge case is never exercised.

## Entry criteria

- [P0276](plan-0276-getbuildgraph-rpc-pg-backed.md) merged (`load_build_graph` with the node-LIMIT edge-unlimited structure exists)

## Tasks

### T1 — `fix(scheduler):` derive edge set from returned node set

MODIFY [`rio-scheduler/src/db.rs`](../../rio-scheduler/src/db.rs) — the edge query at `:1276` (p276 worktree; re-grep post-P0276-merge). Two approaches:

**Approach A (preferred — correctness + implicit bound):** Filter edges to the actually-returned node set, not the whole build:

```rust
// r[impl dash.graph.degrade-threshold]
// Edge set MUST be a subgraph of the returned node set. When nodes
// truncate at 5000, edges referencing truncated nodes would be
// dangling — client lookup by drv_path fails. Collect returned
// derivation_ids and filter edges to that set.
let node_ids: Vec<Uuid> = nodes.iter().map(|n| n.derivation_id).collect();
let edges: Vec<GraphEdgeRow> = sqlx::query_as(
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
.await?;
```

This is both the correctness fix AND an implicit bound: `|edges| ≤ |node_ids|² ≤ 5000²` in theory, but in practice DAGs are sparse (≤4N). If nodes didn't truncate, `node_ids` is the full build — same result as before.

**Approach B (belt-and-suspenders):** Keep Approach A and ALSO add `LIMIT 20000` (4× node cap). Defends against a genuinely dense 5000-node build.

Approach A alone is probably sufficient. Decide at impl time whether B's hard-cap is worth the extra `truncated_edges: bool` proto field.

### T2 — `test(scheduler):` truncation test WITH edges

MODIFY wherever P0276's truncation test lives (likely `rio-scheduler/src/db.rs` tests or `rio-scheduler/tests/`). The existing test seeds 5001 nodes and zero edges. Extend:

```rust
// r[verify dash.graph.degrade-threshold]
#[tokio::test]
async fn load_build_graph_truncated_edges_no_dangling() {
    // Seed 5001 nodes. Seed edges forming a chain: 0→1→2→...→5000
    // (every node except the first has one inbound edge).
    //
    // After LIMIT 5000, one node is excluded. Any edge touching that
    // excluded node MUST NOT appear in the returned edge set.
    let (nodes, edges, total) = db.load_build_graph(build_id, 5000).await?;
    assert_eq!(nodes.len(), 5000);
    assert_eq!(total, 5001);
    assert!(/* truncated flag */);

    // Collect returned drv_paths.
    let returned: HashSet<&str> = nodes.iter().map(|n| n.drv_path.as_str()).collect();

    // Every edge endpoint is in the returned node set — no dangling.
    for e in &edges {
        assert!(returned.contains(e.parent_drv_path.as_str()),
                "dangling edge: parent {} not in returned nodes", e.parent_drv_path);
        assert!(returned.contains(e.child_drv_path.as_str()),
                "dangling edge: child {} not in returned nodes", e.child_drv_path);
    }

    // Sanity: we DID get edges (not zero — the fix shouldn't over-filter).
    // Chain of 5001 → 5000 edges; one edge touches the excluded node →
    // expect 4999 edges.
    assert_eq!(edges.len(), 4999);
}
```

### T3 — `perf(scheduler):` edge-count metric for observability

Optional but cheap. Emit a histogram for edge count per `GetBuildGraph` call so operators can spot builds approaching the implicit bound:

```rust
metrics::histogram!("rio_scheduler_build_graph_edges")
    .record(edges.len() as f64);
```

Check [`observability.md`](../../docs/src/observability.md) naming conventions. Bucket boundaries: `[100, 500, 1000, 5000, 10000, 20000]`.

## Exit criteria

- `/nbr .#ci` green
- `nix develop -c tracey query rule dash.graph.degrade-threshold` — shows impl + verify (T1's annotation, T2's test)
- T2's `load_build_graph_truncated_edges_no_dangling` passes — specifically the `assert_eq!(edges.len(), 4999)` (proves the fix filters exactly the dangling edge, no more)
- `grep 'ANY.\$1.*ANY.\$1\|= ANY' rio-scheduler/src/db.rs` → edge query filters on returned node_ids (Approach A signature)

## Tracey

References existing markers:
- `r[dash.graph.degrade-threshold]` — T1 implements (server-half correctness: edge set is valid subgraph of returned node set), T2 verifies. The marker at [`dashboard.md:36`](../../docs/src/components/dashboard.md) says "The server separately caps responses at 5000 nodes (`GetBuildGraphResponse.truncated`)" — implicitly requires edge correctness under truncation.

## Files

```json files
[
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "T1: edge query filters on returned node_ids (ANY($1)) + T3 metric emit"},
  {"path": "rio-scheduler/tests/db_graph.rs", "action": "MODIFY", "note": "T2: truncation test WITH edges — no dangling (file may be inline #[cfg(test)]; check P0276's test location)"}
]
```

```
rio-scheduler/
├── src/db.rs                     # T1: edge ANY($1) filter + T3 metric
└── tests/db_graph.rs             # T2: no-dangling test (location per P0276)
```

## Dependencies

```json deps
{"deps": [276], "soft_deps": [], "note": "P0276 follow-up. Promoted from perf→standalone: dangling-edges-on-truncation is correctness (dashboard renderer crashes or silently drops). Node query LIMIT 5000 but edge query unbounded → dense build can return 20k+ edges. Approach A (filter to returned node_ids) fixes both. P0276 truncation test seeds zero edges — T2 adds edge-seeded case. discovered_from=P0276."}
```

**Depends on:** [P0276](plan-0276-getbuildgraph-rpc-pg-backed.md) — `load_build_graph` function + truncation test harness.
**Conflicts with:** [`db.rs`](../../rio-scheduler/src/db.rs) count=39 — hottest file in the repo. This edit is localized to the edge query (one function); serial after P0276. No other UNIMPL plan currently targets the `load_build_graph` region specifically.
