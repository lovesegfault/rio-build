//! `AdminService.GetEstimatorStats` implementation (I-124).
//!
//! Dumps the actor's in-memory `build_history` snapshot — per-
//! `(pname, system)` EMA duration/memory plus what `classify()` would
//! pick under the current effective cutoffs. `rio-cli estimator`
//! prints this as a table; the operator question is "why is X routed
//! to large?" / "is the EMA for X plausible?".
//!
//! Reflects the actor's last Tick refresh (~60s stale at worst), NOT
//! a live PG read — that's the point: this is what the scheduler
//! SEES, not what PG says.

use rio_proto::types::{EstimatorEntry, GetEstimatorStatsResponse};
use tonic::Status;

use crate::actor::{ActorCommand, ActorHandle, AdminQuery, EstimatorStatsEntry};

/// Query the actor for the full estimator snapshot, filter by
/// substring, sort by sample_count desc, convert to proto.
pub(super) async fn get_estimator_stats(
    actor: &ActorHandle,
    drv_name_filter: Option<&str>,
) -> Result<GetEstimatorStatsResponse, Status> {
    // send_unchecked: read-only diagnostic, but the operator reaches
    // for this when something is misrouting under load — same
    // "don't drop the diagnostic exactly when it's needed" reasoning
    // as ClusterStatus / GetSizeClassStatus.
    let mut entries = actor
        .query_unchecked(|reply| ActorCommand::Admin(AdminQuery::EstimatorStats { reply }))
        .await
        .map_err(crate::grpc::SchedulerGrpc::actor_error_to_status)?;

    // Substring filter on pname. Empty filter = keep all (proto
    // optional-string-unset and explicit "" both land here).
    if let Some(filter) = drv_name_filter.filter(|s| !s.is_empty()) {
        entries.retain(|e| e.pname.contains(filter));
    }

    // Most-observed first: high sample_count = the EMAs the scheduler
    // trusts most. Ties broken by pname for stable output (HashMap
    // iteration order is nondeterministic; without this the CLI table
    // reorders between runs).
    entries.sort_by(|a, b| {
        b.sample_count
            .cmp(&a.sample_count)
            .then_with(|| a.pname.cmp(&b.pname))
            .then_with(|| a.system.cmp(&b.system))
    });

    Ok(GetEstimatorStatsResponse {
        entries: entries.into_iter().map(to_proto).collect(),
    })
}

fn to_proto(e: EstimatorStatsEntry) -> EstimatorEntry {
    EstimatorEntry {
        drv_name: e.pname,
        system: e.system,
        // PG `INT` → i32; proto `uint64`. sample_count is a monotone
        // counter from 0, never negative — the .max(0) is defensive
        // against a hand-edited row.
        sample_count: e.sample_count.max(0) as u64,
        ema_duration_secs: e.ema_duration_secs,
        ema_peak_memory_bytes: e.ema_peak_memory_bytes.unwrap_or(0.0),
        size_class: e.size_class.unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pname: &str, samples: i32) -> EstimatorStatsEntry {
        EstimatorStatsEntry {
            pname: pname.to_string(),
            system: "x86_64-linux".to_string(),
            sample_count: samples,
            ema_duration_secs: 60.0,
            ema_peak_memory_bytes: Some(1e9),
            size_class: Some("small".to_string()),
        }
    }

    /// Sort: sample_count desc, then pname asc for determinism.
    /// HashMap iteration is random — without the secondary key the
    /// CLI table reorders between runs.
    #[test]
    fn sort_and_tiebreak() {
        let mut v = [entry("zzz", 10), entry("aaa", 10), entry("mid", 50)];
        v.sort_by(|a, b| {
            b.sample_count
                .cmp(&a.sample_count)
                .then_with(|| a.pname.cmp(&b.pname))
                .then_with(|| a.system.cmp(&b.system))
        });
        let names: Vec<_> = v.iter().map(|e| e.pname.as_str()).collect();
        assert_eq!(names, vec!["mid", "aaa", "zzz"]);
    }

    #[test]
    fn proto_mapping_none_mem_is_zero() {
        let mut e = entry("hello", 3);
        e.ema_peak_memory_bytes = None;
        e.size_class = None;
        let p = to_proto(e);
        assert_eq!(p.ema_peak_memory_bytes, 0.0);
        assert_eq!(p.size_class, "");
        assert_eq!(p.sample_count, 3);
    }
}
