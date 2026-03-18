# Plan 0276: GetBuildGraph RPC — PG-backed, thin message

**De-risks R4, R7.** DAG viz needs graph structure. **Does NOT reuse `DerivationNode`** — that carries `bytes drv_content` ([`types.proto:32`](../../rio-proto/proto/types.proto), ≤64KB/node). 2000 nodes × 64KB = 128MB > `max_encoding_message_size`. Thin `GraphNode` (5 strings, ~200B) instead. PG-backed via 3-table JOIN on [`migrations/001_scheduler.sql:39-100`](../../migrations/001_scheduler.sql) schema — not actor-snapshot.

**USER Open-Q decision:** dispatch immediately (`deps=[]`), advisory-serialize vs [P0231](plan-0231-actor-drain-mailbox.md)/[P0248](plan-0248-types-proto-is-ca-field.md). Both are EOF-appends of distinct messages — textually parallel-safe. At dispatch: `git log --oneline -5 -- rio-proto/proto/types.proto`.

## Entry criteria

none — Wave 0a root. Advisory-serialize (not hard dep) after P0231/P0248 on `types.proto`.

## Tasks

### T1 — `feat(proto):` GraphNode/GraphEdge thin messages

EOF-append to [`rio-proto/proto/types.proto`](../../rio-proto/proto/types.proto):

```protobuf
// Dashboard DAG — deliberately THIN (~200B/node). DerivationNode is ≤64KB.
message GraphNode {
  string drv_path = 1;
  string pname = 2;
  string system = 3;
  string status = 4;  // derivations.status CHECK constraint values
  string assigned_worker_id = 5;  // empty if unassigned
}
message GraphEdge {
  string parent_drv_path = 1;
  string child_drv_path = 2;
  bool is_cutoff = 3;  // phase5 core wires this (P0252 Skipped cascade)
}
message GetBuildGraphRequest { string build_id = 1; }
message GetBuildGraphResponse {
  repeated GraphNode nodes = 1;
  repeated GraphEdge edges = 2;
  bool truncated = 3;       // server caps at DASHBOARD_GRAPH_NODE_LIMIT=5000
  uint32 total_nodes = 4;
}
```

### T2 — `feat(proto):` GetBuildGraph RPC in admin.proto

MODIFY [`rio-proto/proto/admin.proto`](../../rio-proto/proto/admin.proto) — after `CreateTenant`:
```protobuf
// PG-backed snapshot (derivations + derivation_edges + build_derivations JOIN).
// Dashboard polls 5s for live status colors.
rpc GetBuildGraph(rio.types.GetBuildGraphRequest) returns (rio.types.GetBuildGraphResponse);
```

### T3 — `feat(scheduler):` load_build_graph db query

MODIFY [`rio-scheduler/src/db.rs`](../../rio-scheduler/src/db.rs) — NEW fn at EOF:

```rust
pub async fn load_build_graph(
    &self, build_id: Uuid, limit: u32,
) -> sqlx::Result<(Vec<GraphNodeRow>, Vec<GraphEdgeRow>, u32)> {
    let total: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM build_derivations WHERE build_id = $1", build_id
    ).fetch_one(&self.pool).await?.unwrap_or(0);
    let nodes = sqlx::query_as!(GraphNodeRow, r#"
        SELECT d.drv_path, d.pname, d.system, d.status,
               COALESCE(d.assigned_worker_id, '') AS "assigned_worker_id!"
        FROM derivations d
        JOIN build_derivations bd ON bd.derivation_id = d.derivation_id
        WHERE bd.build_id = $1 LIMIT $2
    "#, build_id, limit as i64).fetch_all(&self.pool).await?;
    let edges = sqlx::query_as!(GraphEdgeRow, r#"
        SELECT dp.drv_path AS parent_drv_path, dc.drv_path AS child_drv_path, e.is_cutoff
        FROM derivation_edges e
        JOIN derivations dp ON dp.derivation_id = e.parent_id
        JOIN derivations dc ON dc.derivation_id = e.child_id
        WHERE e.parent_id IN (SELECT derivation_id FROM build_derivations WHERE build_id = $1)
          AND e.child_id  IN (SELECT derivation_id FROM build_derivations WHERE build_id = $1)
    "#, build_id).fetch_all(&self.pool).await?;
    Ok((nodes, edges, total as u32))
}
```

**Regenerate `.sqlx/`:** `nix develop -c cargo sqlx prepare --workspace`.

### T4 — `feat(scheduler):` admin/graph.rs handler

NEW [`rio-scheduler/src/admin/graph.rs`](../../rio-scheduler/src/admin/graph.rs):

```rust
const DASHBOARD_GRAPH_NODE_LIMIT: u32 = 5000;
pub(super) async fn get_build_graph(pool: &PgPool, build_id: &str) -> Result<GetBuildGraphResponse, Status> {
    let uuid = Uuid::parse_str(build_id).map_err(|_| Status::invalid_argument("bad UUID"))?;
    let (nodes, edges, total) = db.load_build_graph(uuid, DASHBOARD_GRAPH_NODE_LIMIT).await
        .map_err(|e| Status::internal(format!("db: {e}")))?;
    Ok(GetBuildGraphResponse {
        nodes: nodes.into_iter().map(Into::into).collect(),
        edges: edges.into_iter().map(Into::into).collect(),
        truncated: total > DASHBOARD_GRAPH_NODE_LIMIT,
        total_nodes: total,
    })
}
```

MODIFY [`rio-scheduler/src/admin/mod.rs`](../../rio-scheduler/src/admin/mod.rs) — `mod graph;` + wire into trait impl.

### T5 — `test(scheduler):` graph scoping + truncation

`rio-test-support` ephemeral PG:
- 3 nodes, 2 edges, one build → correct shape, `truncated=false`
- Two builds sharing a derivation → each sees only its subgraph
- `limit=2` on 3-node build → `truncated=true`, `total_nodes=3`

## Exit criteria

- `/nbr .#ci` green
- `grpcurl -plaintext scheduler:<port> rio.admin.AdminService/GetBuildGraph -d '{"build_id":"..."}' ` returns nodes+edges

## Tracey

none — dashboard support RPC. If spec text gets added to `scheduler.md` documenting the subgraph-projection semantics, it would be `r[sched.admin.build-graph]` — P0284 decides whether to seed it.

## Files

```json files
[
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "T1: EOF-append GraphNode/Edge/Request/Response (advisory-serial vs P0231/P0248)"},
  {"path": "rio-proto/proto/admin.proto", "action": "MODIFY", "note": "T2: GetBuildGraph RPC line"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "T3: load_build_graph EOF-append (count=34; .sqlx/ regen)"},
  {"path": "rio-scheduler/src/admin/graph.rs", "action": "NEW", "note": "T4: handler"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "T4: mod decl + trait wire"},
  {"path": "rio-scheduler/src/admin/tests.rs", "action": "MODIFY", "note": "T5: subgraph+trunc tests"},
  {"path": "rio-scheduler/.sqlx/placeholder", "action": "MODIFY", "note": "T3: cargo sqlx prepare regen (proxy entry for workspace .sqlx/)"}
]
```

```
rio-proto/proto/
├── types.proto                   # T1: GraphNode etc EOF
└── admin.proto                   # T2: RPC line
rio-scheduler/src/
├── db.rs                         # T3: load_build_graph EOF
└── admin/
    ├── graph.rs                  # T4: NEW
    ├── mod.rs                    # T4: wire
    └── tests.rs                  # T5
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [231, 248], "note": "USER Open-Q: dispatch immediately, advisory-serial vs P0231+P0248 on types.proto (all EOF-append distinct messages — textually parallel-safe). db.rs count=34 — new fn at EOF. .sqlx/ regen REQUIRED."}
```

**Depends on:** none (hard). Advisory-serialize after P0231/P0248 on `types.proto` — merge-queue resolves.
**Conflicts with:** `types.proto` count=29 — advisory-serial (all EOF). `db.rs` count=34 — new fn at EOF, no edit to existing queries. `admin/mod.rs` disjoint from any P0273 touch (Envoy → no scheduler touch per A1).
