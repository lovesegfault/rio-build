//! `rio-cli builds|cancel-build` — build listing + cancellation.
//!
//! Separate module (not inline in `main.rs`) — keep `main.rs` deltas to
//! enum variant + match arm + mod decl only.

use anyhow::anyhow;
use rio_proto::AdminServiceClient;
use rio_proto::types::{BuildInfo, CancelBuildRequest, ListBuildsRequest};
use serde::Serialize;
use tonic::transport::Channel;

use crate::{json, rpc};

pub(crate) async fn run_list(
    as_json: bool,
    client: &mut AdminServiceClient<Channel>,
    status: Option<String>,
    limit: u32,
    cursor: Option<String>,
) -> anyhow::Result<()> {
    let req = ListBuildsRequest {
        status_filter: status.unwrap_or_default(),
        limit,
        cursor,
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
    build_id: String,
    reason: String,
) -> anyhow::Result<()> {
    let mut sched = rio_proto::client::connect_scheduler(scheduler_addr)
        .await
        .map_err(|e| anyhow!("connect to scheduler at {scheduler_addr}: {e}"))?;
    let req = CancelBuildRequest {
        build_id: build_id.clone(),
        reason,
    };
    let resp = rpc("CancelBuild", async || {
        sched.cancel_build(req.clone()).await
    })
    .await?;
    if as_json {
        #[derive(Serialize)]
        struct CancelJson<'a> {
            build_id: &'a str,
            cancelled: bool,
        }
        json(&CancelJson {
            build_id: &build_id,
            cancelled: resp.cancelled,
        })?;
    } else if resp.cancelled {
        println!("cancelled build {build_id}");
    } else {
        // Server returns false for already-terminal AND
        // BuildNotFound (the latter as a tonic NotFound error,
        // which `rpc()` would have surfaced above — so false
        // here means "found but already terminal").
        println!("{build_id}: already terminal (nothing to cancel)");
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
