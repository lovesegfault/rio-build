//! `AdminService.GetBuildGraph` implementation.
//!
//! PG-backed DAG snapshot for dashboard viz. NOT actor-snapshot —
//! completed builds have no actor state (CleanupTerminalBuild removes
//! them ~30s after terminal), but PG persists the full graph. Dashboard
//! polls at 5s for live status colors.

use rio_proto::dag::{GetBuildGraphResponse, GraphEdge, GraphNode};
use tonic::Status;

use crate::db::{GraphEdgeRow, GraphNodeRow, SchedulerDb};

/// Server-side node cap. The dashboard would choke on 50k-node
/// force-directed layout anyway; 5000 covers the 99th percentile of
/// real nixpkgs closures. `truncated=true` in the response tells the
/// client to switch to a summarized view (top-N by status, etc).
const DASHBOARD_GRAPH_NODE_LIMIT: u32 = 5000;

/// Load the build's derivation subgraph and convert to proto.
///
/// `limit_override`: tests can pass `Some(2)` to exercise the
/// truncation path without seeding 5001 rows. `None` = production
/// default (`DASHBOARD_GRAPH_NODE_LIMIT`).
pub(super) async fn get_build_graph(
    db: &SchedulerDb,
    build_id: &str,
    limit_override: Option<u32>,
) -> Result<GetBuildGraphResponse, Status> {
    let uuid = crate::grpc::SchedulerGrpc::parse_build_id(build_id)?;
    let limit = limit_override.unwrap_or(DASHBOARD_GRAPH_NODE_LIMIT);

    let (nodes, edges, total) = db
        .load_build_graph(uuid, limit)
        .await
        .map_err(|e| Status::internal(format!("load_build_graph: {e}")))?;

    Ok(GetBuildGraphResponse {
        nodes: nodes.into_iter().map(node_row_to_proto).collect(),
        edges: edges.into_iter().map(edge_row_to_proto).collect(),
        truncated: total > limit,
        total_nodes: total,
    })
}

fn node_row_to_proto(r: GraphNodeRow) -> GraphNode {
    GraphNode {
        drv_path: r.drv_path,
        pname: r.pname,
        system: r.system,
        status: r.status,
        assigned_executor_id: r.assigned_builder_id,
    }
}

fn edge_row_to_proto(r: GraphEdgeRow) -> GraphEdge {
    GraphEdge {
        parent_drv_path: r.parent_drv_path,
        child_drv_path: r.child_drv_path,
    }
}
