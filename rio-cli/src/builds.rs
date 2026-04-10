//! `rio-cli builds|cancel-build` — build listing + cancellation.

use anyhow::anyhow;
use rio_proto::AdminServiceClient;
use rio_proto::types::{BuildInfo, CancelBuildRequest, ListBuildsRequest};
use tonic::transport::Channel;

use crate::{json, rpc};

#[derive(clap::Args, Clone)]
pub(crate) struct ListArgs {
    /// Filter by build status: "pending", "active", "succeeded",
    /// "failed", "cancelled".
    #[arg(long)]
    status: Option<String>,
    /// Max results (server capped).
    #[arg(long, default_value = "50")]
    limit: u32,
    /// Opaque keyset cursor from a prior page's `next_cursor`.
    /// Stable under concurrent inserts, unlike offset.
    #[arg(long)]
    cursor: Option<String>,
}

#[derive(clap::Args, Clone)]
pub(crate) struct CancelArgs {
    /// Build UUID (as shown by `rio-cli builds` or `status`).
    build_id: String,
    /// Free-form reason (recorded in the BuildCancelled event +
    /// scheduler logs). Defaults to "operator_request".
    #[arg(long, default_value = "operator_request")]
    reason: String,
}

pub(crate) async fn run_list(
    as_json: bool,
    client: &mut AdminServiceClient<Channel>,
    a: ListArgs,
) -> anyhow::Result<()> {
    let req = ListBuildsRequest {
        status_filter: a.status.unwrap_or_default(),
        limit: a.limit,
        cursor: a.cursor,
        ..Default::default()
    };
    let resp = rpc("ListBuilds", async || client.list_builds(req.clone()).await).await?;
    if as_json {
        json(&resp)?;
    } else if resp.builds.is_empty() {
        println!("(no builds — {} total matching filter)", resp.total_count);
    } else {
        println!("{} builds ({} total):", resp.builds.len(), resp.total_count);
        for b in &resp.builds {
            print_build(b);
        }
        if let Some(c) = &resp.next_cursor {
            println!("(next page: --cursor {c})");
        }
    }
    Ok(())
}

/// CancelBuild lives on SchedulerService, not AdminService — it's the
/// same RPC the gateway calls on client disconnect. Same address
/// (scheduler hosts both services on one port), separate client.
pub(crate) async fn run_cancel(
    as_json: bool,
    scheduler_addr: &str,
    a: CancelArgs,
) -> anyhow::Result<()> {
    let mut sched: rio_proto::SchedulerServiceClient<_> =
        rio_proto::client::connect_single(scheduler_addr)
            .await
            .map_err(|e| anyhow!("connect to scheduler at {scheduler_addr}: {e}"))?;
    let req = CancelBuildRequest {
        build_id: a.build_id.clone(),
        reason: a.reason,
    };
    let resp = rpc("CancelBuild", async || {
        sched.cancel_build(req.clone()).await
    })
    .await?;
    if as_json {
        return json(&serde_json::json!({ "build_id": a.build_id, "cancelled": resp.cancelled }));
    }
    if resp.cancelled {
        println!("cancelled build {}", a.build_id);
    } else {
        // Server returns false for already-terminal AND
        // BuildNotFound (the latter as a tonic NotFound error,
        // which `rpc()` would have surfaced above — so false
        // here means "found but already terminal").
        println!("{}: already terminal (nothing to cancel)", a.build_id);
    }
    Ok(())
}

fn print_build(b: &BuildInfo) {
    println!(
        "  build {} [{:?}] {}/{} drv ({} cached) tenant={} prio={}",
        b.build_id,
        b.state(),
        b.completed_derivations,
        b.total_derivations,
        b.cached_derivations,
        b.tenant_id,
        b.priority_class,
    );
    if !b.error_summary.is_empty() {
        println!("    error: {}", b.error_summary);
    }
}
