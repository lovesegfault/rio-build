//! `AdminService.GetSizeClassStatus` implementation.
//!
//! Joins actor-state (effective/configured cutoffs, queued/running
//! counts) with DB-state (sample counts from `build_samples`).
//!
//! This is the Y-join hub for P0234 (WPS autoscaler reads
//! `effective_cutoff_secs` + `queued` to compute per-class replica
//! targets), P0236 (CLI prints the full table), and P0237 (CLI embeds
//! effective cutoffs in WPS describe).

use rio_proto::types::{GetSizeClassStatusResponse, SizeClassStatus};
use tonic::Status;

use crate::actor::{ActorCommand, ActorHandle, SizeClassSnapshot};
use crate::db::SchedulerDb;

/// Rebalancer lookback window for the sample-count query. Mirrors
/// `RebalancerConfig::default().lookback_days`. If the operator
/// overrides lookback in scheduler.toml, this RPC still reports a
/// 7-day window — the two are independent views. Making this
/// config-driven would need plumbing `RebalancerConfig` through
/// `AdminServiceImpl`; not worth it for a best-effort diagnostic
/// count.
const SAMPLE_LOOKBACK_DAYS: u32 = 7;

/// Query the actor for the size-class snapshot, join DB sample counts,
/// convert to proto.
pub(super) async fn get_size_class_status(
    actor: &ActorHandle,
    db: &SchedulerDb,
) -> Result<GetSizeClassStatusResponse, Status> {
    // send_unchecked: the WPS autoscaler (P0234) reads this to set
    // per-class replica targets. Dropping under backpressure blinds
    // the autoscaler exactly when it should scale up — same
    // reasoning as ClusterStatus.
    let snapshots = actor
        .query_unchecked(|reply| ActorCommand::GetSizeClassSnapshot { reply })
        .await
        .map_err(crate::grpc::SchedulerGrpc::actor_error_to_status)?;

    if snapshots.is_empty() {
        // Feature off (size_classes unconfigured). Empty response —
        // not an error. CLI renders "size-class routing disabled."
        return Ok(GetSizeClassStatusResponse {
            classes: Vec::new(),
        });
    }

    // Sample counts: best-effort. If PG is unreachable, fall back to
    // zeros rather than failing the whole RPC — the actor-supplied
    // fields (cutoffs, queued, running) are the load-bearing ones for
    // the autoscaler; sample_count is operator diagnostic.
    let sample_counts = count_samples_by_class(db, &snapshots)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(
                error = %e,
                "sample count query failed; reporting zeros (best-effort)"
            );
            vec![0; snapshots.len()]
        });

    let classes = snapshots
        .into_iter()
        .zip(sample_counts)
        .map(|(s, samples)| SizeClassStatus {
            name: s.name,
            effective_cutoff_secs: s.effective_cutoff_secs,
            configured_cutoff_secs: s.configured_cutoff_secs,
            queued: s.queued,
            running: s.running,
            sample_count: samples,
        })
        .collect();

    Ok(GetSizeClassStatusResponse { classes })
}

/// Partition `build_samples` rows from the last `SAMPLE_LOOKBACK_DAYS`
/// into per-class buckets by `duration_secs`.
///
/// Bucketing uses the EFFECTIVE cutoffs from `snapshots` (which the
/// actor has already sorted ascending). Class `i` counts samples in
/// `(cutoff[i-1], cutoff[i]]` — matching `classify()`'s "smallest
/// class whose cutoff ≥ duration" semantics. The last class catches
/// everything above its own cutoff (classify's overflow path).
///
/// Returns one count per snapshot entry, same order.
///
/// One DB roundtrip regardless of class count. The alternative
/// (`COUNT(*) WHERE duration BETWEEN ... GROUP BY CASE ...`) would
/// push bucketing to PG but requires building a dynamic CASE
/// expression from the cutoffs — not worth the complexity for an
/// admin RPC that runs at autoscaler-poll cadence (~30s).
async fn count_samples_by_class(
    db: &SchedulerDb,
    snapshots: &[SizeClassSnapshot],
) -> Result<Vec<u64>, sqlx::Error> {
    let samples = db
        .query_build_samples_last_days(SAMPLE_LOOKBACK_DAYS)
        .await?;

    let mut counts = vec![0u64; snapshots.len()];
    // snapshots is already sorted by effective_cutoff_secs ascending
    // (the actor sorts before returning). Walk cutoffs left-to-right
    // for each sample — first match wins (smallest covering class).
    for (dur, _peak_mem) in &samples {
        let mut placed = false;
        for (i, s) in snapshots.iter().enumerate() {
            if *dur <= s.effective_cutoff_secs {
                counts[i] += 1;
                placed = true;
                break;
            }
        }
        // Overflow: duration exceeds all cutoffs. classify() routes
        // these to the last (largest) class, so count them there.
        if !placed && let Some(last) = counts.last_mut() {
            *last += 1;
        }
    }

    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;

    fn snap(name: &str, cutoff: f64) -> SizeClassSnapshot {
        SizeClassSnapshot {
            name: name.to_string(),
            effective_cutoff_secs: cutoff,
            configured_cutoff_secs: cutoff,
            queued: 0,
            running: 0,
        }
    }

    /// Partition mechanics: smallest covering class wins; overflow to
    /// last. Mirrors classify()'s first-match + fallback. A bug here
    /// would misreport which class is sample-starved → operator tunes
    /// the wrong pool.
    #[tokio::test]
    async fn sample_partition_boundaries() {
        let test_db = TestDb::new(&crate::MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        // Seed: durations straddling the 60s and 300s boundaries.
        //
        //   small  (≤60):   10, 60           → 2
        //   medium (≤300):  61, 300          → 2
        //   large  (>300):  301, 9999        → 2 (overflow)
        //
        // 60 goes to small (≤ cutoff, not <). 61 goes to medium.
        // 301 exceeds medium's cutoff → overflow → large.
        for dur in [10.0, 60.0, 61.0, 300.0, 301.0, 9999.0] {
            db.insert_build_sample("test-pkg", "x86_64-linux", dur, 0)
                .await
                .unwrap();
        }

        let snapshots = vec![
            snap("small", 60.0),
            snap("medium", 300.0),
            snap("large", 1800.0),
        ];

        let counts = count_samples_by_class(&db, &snapshots).await.unwrap();
        assert_eq!(
            counts,
            vec![2, 2, 2],
            "small=10,60 medium=61,300 large=301,9999(overflow)"
        );
    }

    /// Empty DB → all zeros. The admin handler still returns the
    /// actor-supplied cutoffs/queued/running; sample_count is the only
    /// field that degrades.
    #[tokio::test]
    async fn sample_partition_empty_db() {
        let test_db = TestDb::new(&crate::MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        let snapshots = vec![snap("small", 60.0), snap("large", 600.0)];
        let counts = count_samples_by_class(&db, &snapshots).await.unwrap();
        assert_eq!(counts, vec![0, 0]);
    }
}
