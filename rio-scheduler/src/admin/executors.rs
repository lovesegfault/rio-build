//! `AdminService.ListExecutors` implementation.
//!
//! Re-implemented over the durable open-attempt view when the stream
//! session machinery (and with it the in-memory executors map this RPC
//! used to snapshot) was deleted: every open pull-mode attempt is one
//! busy executor — the pod that pulled it (P0537: at most one build per
//! pod). Spawned-but-not-yet-pulled pods are not listed (the scheduler
//! holds no registration state for them; the controller's Job census is
//! that view), and there is no draining/degraded/connecting state to
//! report. `ListOpenAttempts` is the richer, attempt-keyed form of the
//! same view; this RPC stays so existing CLI/dashboard/controller
//! callers keep a working endpoint until the 1d proto sweep.

use std::time::{Duration, SystemTime};

use rio_common::grpc::StatusExt;
use rio_proto::types::{ExecutorInfo, ListExecutorsResponse};
use tonic::Status;

use crate::db::SchedulerDb;
use crate::db::open_attempts::OpenAttemptRow;

/// Status values this surface can produce. Every open attempt is an
/// actively-building pod, so the only producible status is "alive";
/// the other historical filter values ("draining", "degraded",
/// "connecting") are kept recognized so an explicit filter for them
/// returns an empty list rather than leniently returning everything.
const KNOWN_STATUSES: [&str; 4] = ["alive", "draining", "degraded", "connecting"];

/// Query the open-attempt view, filter by status, convert to proto.
///
/// `leader_for`: elapsed since this replica acquired leadership
/// (`LeaderState::leader_for()`). Populates `leader_for_secs` so the
/// controller's `orphan_reap_gate` can fail-closed right after a
/// failover. `None` is unreachable here (`ensure_leader()` is checked
/// first); treated as 0 (young).
// r[impl sched.admin.list-executors-leader-age+3]
// r[impl sched.admin.list-executors+2]
pub(super) async fn list_executors(
    db: &SchedulerDb,
    status_filter: &str,
    leader_for: Option<Duration>,
) -> Result<ListExecutorsResponse, Status> {
    let rows = db
        .list_open_pull_attempts()
        .await
        .status_internal("list_open_pull_attempts")?;

    // Empty filter = all. "alive" = all (every entry is alive). Any
    // other known status = none (not producible here). Unknown filter
    // = all (lenient — operator typos shouldn't hide executors).
    let matches_filter = match status_filter {
        "" | "alive" => true,
        s if KNOWN_STATUSES.contains(&s) => false,
        _ => true,
    };
    let executors: Vec<ExecutorInfo> = if matches_filter {
        rows.into_iter().map(row_to_proto).collect()
    } else {
        Vec::new()
    };

    Ok(ListExecutorsResponse {
        executors,
        leader_for_secs: leader_for.map_or(0, |d| d.as_secs()),
    })
}

fn row_to_proto(r: OpenAttemptRow) -> ExecutorInfo {
    // The pull that opened the attempt is both "when this pod bound
    // to the scheduler" and the last protocol contact the scheduler
    // has had from it (pull-mode pods send nothing between the pull
    // and the report) — populate both timestamp fields from it. The
    // stream-era 30s heartbeat-staleness reading does not apply to
    // these values; per-pod liveness is the Job/pod phase plus
    // attempt age (the OA5 successor view).
    let attempt_opened = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs_f64(r.assigned_at_epoch_secs.max(0.0)))
        .map(prost_types::Timestamp::from);
    ExecutorInfo {
        executor_id: r.executor_id,
        // The system the attempt's derivation targets — what the pod
        // was spawned to build.
        systems: vec![r.system],
        supported_features: Vec::new(),
        busy: true,
        status: "alive".to_string(),
        resources: None,
        last_heartbeat: attempt_opened,
        connected_since: attempt_opened,
        kind: crate::state::kind_for_drv(r.is_fixed_output) as i32,
    }
}
