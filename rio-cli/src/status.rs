//! `rio-cli status` — cluster summary + worker/build rollup.
//!
//! Calls `ClusterStatus`, `ListExecutors`, and `ListBuilds` (limit 10),
//! then prints all three — summary header, per-worker one-liners,
//! recent-build rollup. The flattened-JSON shape is what cli.nix
//! asserts against (`jq -e '.total_executors'`).

use crate::AdminClient;
use rio_proto::types::{
    BuildInfo, ClusterStatusResponse, ExecutorInfo, ListBuildsRequest, ListExecutorsRequest,
};
use serde::Serialize;

use crate::{json, rpc};

/// Run the `status` subcommand.
pub(crate) async fn run(as_json: bool, client: &mut AdminClient) -> anyhow::Result<()> {
    // Three sequential RPCs. Gather ALL results before printing
    // anything — if `list_executors` or `list_builds` fails after
    // `cluster_status` succeeds, we'd otherwise print the summary
    // header and then bail, leaving output that looks truncated
    // rather than failed. `?` on each gather is fine: nothing
    // has been printed yet, so the error message is the only
    // output.
    let cs = rpc("ClusterStatus", async || client.cluster_status(()).await).await?;
    let workers = rpc("ListExecutors", async || {
        client.list_executors(ListExecutorsRequest::default()).await
    })
    .await?;
    let builds_req = ListBuildsRequest {
        limit: 10,
        ..Default::default()
    };
    let builds = rpc("ListBuilds", async || {
        client.list_builds(builds_req.clone()).await
    })
    .await?;

    // All data in hand — now print. No `?` below this line in
    // the human path (the JSON path's one `?` is serde encode,
    // which only fails on unrepresentable floats — not a
    // partial-output hazard).
    if as_json {
        #[derive(Serialize)]
        struct StatusFull<'a> {
            #[serde(flatten)]
            summary: &'a ClusterStatusResponse,
            executors: &'a [ExecutorInfo],
            builds: &'a [BuildInfo],
        }
        return json(&StatusFull {
            summary: &cs,
            executors: &workers.executors,
            builds: &builds.builds,
        });
    }

    print_status(&cs);
    // Worker and build detail lines below the summary — this is
    // what `docs/src/phases/phase4.md` means by "rio-cli status":
    // enough to eyeball that workers registered and builds landed.
    for w in &workers.executors {
        println!(
            "  worker {} [{}] {}, systems={}",
            w.executor_id,
            w.status,
            if w.busy { "busy" } else { "idle" },
            w.systems.join(",")
        );
    }
    if builds.total_count > 0 {
        println!("recent builds ({} total):", builds.total_count);
        for b in &builds.builds {
            println!(
                "  build {} [{:?}] {}/{} drv ({} cached)",
                b.build_id,
                b.state(),
                b.completed_derivations,
                b.total_derivations,
                b.cached_derivations
            );
        }
    }
    Ok(())
}

fn print_status(s: &ClusterStatusResponse) {
    println!(
        "executors: {} total, {} active, {} draining",
        s.total_executors, s.active_executors, s.draining_executors
    );
    println!(
        "builds:  {} pending, {} active",
        s.pending_builds, s.active_builds
    );
    println!(
        "queue:   {} queued, {} running, {} substituting",
        s.queued_derivations, s.running_derivations, s.substituting_derivations
    );
    println!("store:   {} bytes", s.store_size_bytes);
}
