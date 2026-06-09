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

use std::time::Duration;

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
// r[impl sched.admin.list-executors+3]
pub(super) async fn list_executors(
    db: &SchedulerDb,
    status_filter: &str,
    leader_for: Option<Duration>,
) -> Result<ListExecutorsResponse, Status> {
    // A2.4 (bug_217): the executors view IS the builder fleet — a
    // store replica's materialization claim is not an executor (the
    // OA5 successor view sizes pods, and store pods are sized by their
    // own deployment). The materialization lane is visible via
    // ListOpenAttempts' kinded rows instead.
    let rows = db
        .list_open_pull_attempts()
        .await
        .status_internal("list_open_pull_attempts")?
        .build;

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
    // The pull that opened the attempt — the one timestamp this
    // surface carries (the redundant `connected_since` twin is
    // deleted; the field is renamed to what it always held). Per-pod
    // liveness is the Job/pod phase plus attempt age (the OA5
    // successor view + the OA2 wedge alert); consumers render plain
    // relative age.
    // `assigned_at` is an ABSOLUTE epoch: the EPOCH-domain constructor
    // carries it undistorted (the age clamp's 1-year ceiling relocated
    // every real stamp to 1971) and refuses poisoned values totally —
    // the field is optional on the wire, so a refused epoch is simply
    // absent rather than a fabricated 1970.
    let attempt_opened =
        rio_common::clamped::epoch_secs(r.assigned_at_epoch_secs).map(prost_types::Timestamp::from);
    ExecutorInfo {
        executor_id: r.executor_id,
        // The system the attempt's derivation targets — what the pod
        // was spawned to build.
        systems: vec![r.system],
        supported_features: Vec::new(),
        busy: true,
        status: "alive".to_string(),
        resources: None,
        attempt_opened,
        kind: crate::state::kind_for_drv(r.is_fixed_output) as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_attempts::OpenAttemptRow;
    use uuid::Uuid;

    fn row(assigned_at_epoch_secs: f64) -> OpenAttemptRow {
        OpenAttemptRow {
            derivation_id: Uuid::nil(),
            drv_hash: "h".into(),
            drv_path: "/nix/store/h-x.drv".into(),
            exec_id: Uuid::nil(),
            executor_id: "exec-1".into(),
            system: "x86_64-linux".into(),
            is_fixed_output: false,
            source_node: None,
            generation: 1,
            assigned_at_epoch_secs,
            age_secs: 1.0,
            deadline_secs: None,
            attempt_kind: "build".into(),
        }
    }

    /// `assigned_at` is an ABSOLUTE epoch: a real 2023 timestamp must
    /// survive the proto round-trip undistorted (the age clamp's
    /// 1-year ceiling relocated every real attempt-open stamp to 1971).
    #[test]
    fn attempt_opened_carries_the_real_epoch() {
        let info = row_to_proto(row(1_700_000_000.0));
        let ts = info.attempt_opened.expect("finite epoch must be present");
        assert_eq!(
            ts.seconds, 1_700_000_000,
            "absolute epoch distorted (31536000 = the age-clamp ceiling, 1971)"
        );
    }

    /// Poisoned epochs refuse (absent optional field) — never panic,
    /// never a fabricated 1970/1971 stamp.
    #[test]
    fn attempt_opened_refuses_poisoned_epochs() {
        for poisoned in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -5.0] {
            let info = row_to_proto(row(poisoned));
            assert!(
                info.attempt_opened.is_none(),
                "poisoned epoch {poisoned} must be absent, got {:?}",
                info.attempt_opened
            );
        }
    }
}
