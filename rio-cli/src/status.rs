//! `rio-cli status` — cluster summary + worker/build rollup.
//!
//! Calls `ClusterStatus`, `ListExecutors`, and `ListBuilds` (limit 10),
//! then prints all three — summary header, per-worker one-liners,
//! recent-build rollup. The flattened-JSON shape is what cli.nix
//! asserts against (`jq -e '.total_executors'`).
//!
//! Separate module (not inline in `main.rs`) — same convention as
//! `cutoffs.rs`/`wps.rs`: keep `main.rs` deltas to enum variant +
//! match arm + mod decl only.

use rio_proto::AdminServiceClient;
use rio_proto::types::{ClusterStatusResponse, ListBuildsRequest, ListExecutorsRequest};
use serde::Serialize;
use tonic::transport::Channel;

use crate::{BuildJson, ExecutorJson, StatusJson, json, rpc};

/// Run the `status` subcommand.
pub(crate) async fn run(
    as_json: bool,
    client: &mut AdminServiceClient<Channel>,
) -> anyhow::Result<()> {
    // Three sequential RPCs. Gather ALL results before printing
    // anything — if `list_executors` or `list_builds` fails after
    // `cluster_status` succeeds, we'd otherwise print the summary
    // header and then bail, leaving output that looks truncated
    // rather than failed. `?` on each gather is fine: nothing
    // has been printed yet, so the error message is the only
    // output.
    let cs = rpc("ClusterStatus", client.cluster_status(())).await?;
    let workers = rpc(
        "ListExecutors",
        client.list_executors(ListExecutorsRequest::default()),
    )
    .await?;
    let builds = rpc(
        "ListBuilds",
        client.list_builds(ListBuildsRequest {
            limit: 10,
            ..Default::default()
        }),
    )
    .await?;

    // All data in hand — now print. No `?` below this line in
    // the human path (the JSON path's one `?` is serde encode,
    // which only fails on unrepresentable floats — not a
    // partial-output hazard).
    if as_json {
        #[derive(Serialize)]
        struct StatusFull<'a> {
            #[serde(flatten)]
            summary: StatusJson,
            executors: Vec<ExecutorJson<'a>>,
            builds: Vec<BuildJson<'a>>,
        }
        return json(&StatusFull {
            summary: StatusJson::from(&cs),
            executors: workers.executors.iter().map(ExecutorJson::from).collect(),
            builds: builds.builds.iter().map(BuildJson::from).collect(),
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
            if w.running_builds > 0 { "busy" } else { "idle" },
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
        "queue:   {} queued derivations, {} running",
        s.queued_derivations, s.running_derivations
    );
    println!("store:   {} bytes", s.store_size_bytes);
}
