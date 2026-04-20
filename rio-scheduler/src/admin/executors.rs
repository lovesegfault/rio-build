//! `AdminService.ListExecutors` implementation.

use std::time::{Instant, SystemTime};

use rio_proto::types::{ExecutorInfo, ListExecutorsResponse};
use tonic::Status;

use crate::actor::{ActorCommand, ActorHandle, AdminQuery, ExecutorSnapshot};

/// Exhaustive list of values [`executor_status`] can return. The filter
/// match below references this directly so the producer and consumer
/// can't drift — adding a fifth status to `executor_status` without
/// updating this array trips the `debug_assert!` in `list_executors`.
const KNOWN_STATUSES: [&str; 4] = ["alive", "draining", "degraded", "connecting"];

/// 3-bool → 4-state. `systems.is_empty()` = no heartbeat yet = not
/// fully registered (stream-only or heartbeat-only) → "connecting".
/// Draining wins over degraded: an operator who drained doesn't care
/// the worker also reports a sick store. Degraded wins over alive:
/// `has_capacity()` is false either way, but degraded names the cause.
fn executor_status(s: &ExecutorSnapshot) -> &'static str {
    if s.draining {
        "draining"
    } else if s.store_degraded {
        "degraded"
    } else if s.systems.is_empty() {
        "connecting"
    } else {
        "alive"
    }
}

/// Query the actor for all executors, filter by status, convert to proto.
pub(super) async fn list_executors(
    actor: &ActorHandle,
    status_filter: &str,
) -> Result<ListExecutorsResponse, Status> {
    let snapshots = super::query_actor(actor, |reply| {
        ActorCommand::Admin(AdminQuery::ListExecutors { reply })
    })
    .await?;

    let executors: Vec<ExecutorInfo> = snapshots
        .into_iter()
        // Empty filter = all. Known status = exact match. Unknown filter
        // = all (lenient — operator typos shouldn't hide executors).
        // The KNOWN_STATUSES indirection makes producer/consumer drift
        // unrepresentable: previously "degraded" hit the lenient `_` arm
        // and returned ALL executors precisely when the filter was needed
        // (store incident, I-056b).
        .filter(|w| match status_filter {
            "" => true,
            s if KNOWN_STATUSES.contains(&s) => executor_status(w) == s,
            _ => true,
        })
        .inspect(|w| {
            debug_assert!(
                KNOWN_STATUSES.contains(&executor_status(w)),
                "executor_status() returned {:?} not in KNOWN_STATUSES — update the array",
                executor_status(w)
            );
        })
        .map(snapshot_to_proto)
        .collect();

    Ok(ListExecutorsResponse { executors })
}

fn snapshot_to_proto(s: ExecutorSnapshot) -> ExecutorInfo {
    // Instant → SystemTime: compute "when in CURRENT wall-clock
    // terms" by subtracting elapsed from SystemTime::now(). Same
    // pattern as ClusterStatus.uptime_since. checked_sub for clock
    // jumps — UNIX_EPOCH is less-wrong than panicking.
    let now_sys = SystemTime::now();
    let now_inst = Instant::now();
    let instant_to_ts = |i: Instant| {
        now_sys
            .checked_sub(now_inst.saturating_duration_since(i))
            .map(prost_types::Timestamp::from)
    };
    let status = executor_status(&s).to_string();
    ExecutorInfo {
        executor_id: s.executor_id.to_string(),
        systems: s.systems,
        supported_features: s.supported_features,
        busy: s.busy,
        status,
        resources: s.last_resources,
        last_heartbeat: instant_to_ts(s.last_heartbeat),
        connected_since: instant_to_ts(s.connected_since),
        kind: s.kind as i32,
    }
}
