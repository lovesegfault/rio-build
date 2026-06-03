//! `rio-cli workers` — the scheduler's busy-executor list.
//!
//! `ListExecutors` is backed by the durable open-attempt view: one
//! entry per open pull-mode attempt (the pod that pulled it), so the
//! list is "what is building right now". Spawned-but-not-yet-pulled
//! pods are not listed — `kubectl get jobs/pods` (the Job census) is
//! that view. The stream-era `--actor`/`--diff` modes retired with
//! `DebugListExecutors` (there is no in-memory executor map left to
//! diff against).

use crate::AdminClient;
use rio_proto::types::{ExecutorInfo, ListExecutorsRequest};

use crate::{json, rpc};

/// `rio-cli workers` arguments.
#[derive(clap::Args, Clone)]
pub(crate) struct Args {
    /// Filter by status. Every open attempt reports "alive"; any other
    /// known status returns an empty list.
    #[arg(long)]
    status: Option<String>,
}

pub(crate) async fn run(as_json: bool, client: &mut AdminClient, a: Args) -> anyhow::Result<()> {
    run_pg(as_json, client, a.status).await
}

/// Open-attempt-backed worker list (`ListExecutors`).
pub(crate) async fn run_pg(
    as_json: bool,
    client: &mut AdminClient,
    status: Option<String>,
) -> anyhow::Result<()> {
    let req = ListExecutorsRequest {
        status_filter: status.unwrap_or_default(),
    };
    let resp = rpc("ListExecutors", async || {
        client.list_executors(req.clone()).await
    })
    .await?;
    if as_json {
        json(&resp)?;
    } else if resp.executors.is_empty() {
        println!("(no executors building — no open pull-mode attempts)");
    } else {
        for w in &resp.executors {
            print_worker(w);
        }
    }
    Ok(())
}

/// Detailed per-worker view for `rio-cli workers`. `Status` prints a
/// compact one-liner; this is the "show me everything" form for
/// drilling into one in-flight attempt's executor.
fn print_worker(w: &ExecutorInfo) {
    println!("worker {} [{}]", w.executor_id, w.status);
    println!("  state:    {}", if w.busy { "busy" } else { "idle" });
    // Attempt-open time (the pull). Plain age — a long-running pull is
    // a long build, not a dead pod (liveness = Job/pod phase + the
    // OA2 wedge alert).
    println!(
        "  pulled:   {}",
        crate::fmt_ts_ago(w.attempt_opened.as_ref().map(|t| t.seconds)),
    );
    println!("  systems:  {}", w.systems.join(", "));
    if !w.supported_features.is_empty() {
        println!("  features: {}", w.supported_features.join(", "));
    }
    if let Some(r) = &w.resources {
        println!(
            "  cpu={:.2}  mem={}/{}  disk={}/{}",
            r.cpu_fraction,
            r.memory_used_bytes,
            r.memory_total_bytes,
            r.disk_used_bytes,
            r.disk_total_bytes
        );
    }
}
