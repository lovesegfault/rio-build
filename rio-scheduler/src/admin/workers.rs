//! `AdminService.ListWorkers` implementation.

use std::time::{Instant, SystemTime};

use rio_proto::types::{ListWorkersResponse, WorkerInfo};
use tonic::Status;

use crate::actor::{ActorCommand, ActorHandle, WorkerSnapshot};

/// Query the actor for all workers, filter by status, convert to proto.
pub(super) async fn list_workers(
    actor: &ActorHandle,
    status_filter: &str,
) -> Result<ListWorkersResponse, Status> {
    let snapshots = actor
        .query_unchecked(|reply| ActorCommand::ListWorkers { reply })
        .await?;

    let workers: Vec<WorkerInfo> = snapshots
        .into_iter()
        .filter(|w| match status_filter {
            "" => true,
            // "alive" = registered (has heartbeat, i.e. systems
            // non-empty) and not draining. Unregistered workers
            // (stream-only or heartbeat-only) show as neither alive
            // nor draining — they're connecting.
            "alive" => !w.systems.is_empty() && !w.draining,
            "draining" => w.draining,
            // Unknown filter → show all (lenient — operator typos
            // shouldn't hide workers).
            _ => true,
        })
        .map(snapshot_to_proto)
        .collect();

    Ok(ListWorkersResponse { workers })
}

fn snapshot_to_proto(s: WorkerSnapshot) -> WorkerInfo {
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
    let status = if s.draining {
        "draining"
    } else if s.systems.is_empty() {
        // systems empty = no heartbeat yet = not fully registered
        "connecting"
    } else {
        "alive"
    }
    .to_string();
    WorkerInfo {
        worker_id: s.worker_id.to_string(),
        systems: s.systems,
        supported_features: s.supported_features,
        max_builds: s.max_builds,
        running_builds: s.running_builds,
        status,
        resources: s.last_resources,
        last_heartbeat: instant_to_ts(s.last_heartbeat),
        connected_since: instant_to_ts(s.connected_since),
        size_class: s.size_class.unwrap_or_default(),
    }
}
