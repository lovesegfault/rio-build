//! SITA-E cutoff rebalancer. Queries `build_samples`, partitions into
//! equal-load size classes, EMA-smooths against previous cutoffs.
//!
//! **Builder-only.** Per [ADR-019](../../docs/src/decisions/
//! 019-builder-fetcher-split.md), fetchers have no size class (fetches
//! are network-bound, not CPU-predictable) so they don't appear in the
//! `size_classes` vec this module rewrites. FOD completions skip
//! `insert_build_sample` (see `actor/completion.rs`), so fetch
//! durations never pollute the SITA-E partition. Fetcher replica
//! scaling is the controller's concern (`FetcherPool.spec.replicas` or
//! queue-depth HPA), not the scheduler's.
//!
//! [`compute_cutoffs`] is pure; [`apply_pass`] writes through the
//! shared `Arc<RwLock<Vec<SizeClassConfig>>>`. The actor spawns a
//! background task ([`spawn_task`]) that calls [`apply_pass`] hourly.
//!
//! ## Algorithm
//!
//! 1. Sort samples by duration
//! 2. Cumulative sum
//! 3. Bisect at `total/N * i` for each of the `N-1` class boundaries
//!    — this yields cutoffs where `sum(duration)` is equal across
//!    classes (SITA-E: Size Interval Task Assignment with Equal load)
//! 4. EMA-smooth against previous cutoffs (prevents oscillation when
//!    the sample set shifts between runs)
//! 5. Gate on `min_samples` (don't rebalance on sparse data)
//!
//! ## Edge cases
//!
//! - All durations identical → all raw cutoffs collapse to that value.
//!   Passed through unchanged (NOT deduped). `assignment::classify`
//!   already tolerates equal cutoffs via stable sort + first-match.
//!   Deduping would break P0230's zip-with-prev (truncates, leaves
//!   stale entries in the config vec).
//! - Samples < `min_samples` → `None`, skip this cycle.
//! - `n_classes <= 1` → no boundaries, empty cutoffs.
//
// r[impl sched.rebalancer.sita-e]

use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tracing::{debug, info, warn};

use crate::assignment::SizeClassConfig;
use crate::db::SchedulerDb;

/// Tunables for the rebalancer. All three are config-driven via
/// `scheduler.toml [rebalancer]` — defaults assume medium-volume
/// (~70 builds/day minimum at 500 samples / 7d). Workload-dependent;
/// operator tunes.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct RebalancerConfig {
    /// Below this sample count, skip rebalancing (return `None`).
    /// Sparse data produces noisy cutoffs that thrash the EMA.
    pub min_samples: usize,
    /// EMA blend weight for new vs previous cutoffs. 0.3 ≈ 3
    /// iterations to ~66% convergence (`1 - 0.7^3`).
    pub ema_alpha: f64,
    /// Sample window, passed to `query_build_samples_last_days`.
    /// Not used by `compute_cutoffs` itself (caller does the query).
    pub lookback_days: u32,
}

impl Default for RebalancerConfig {
    fn default() -> Self {
        Self {
            min_samples: 500,
            ema_alpha: 0.3,
            lookback_days: 7,
        }
    }
}

/// Output of a rebalancer pass.
#[derive(Debug, Clone)]
pub struct RebalanceResult {
    /// `n_classes - 1` duration boundaries (seconds), smoothed.
    /// Class `i` covers `[new_cutoffs[i-1], new_cutoffs[i])`.
    pub new_cutoffs: Vec<f64>,
    /// Per-class `sum(duration) / total` under the SMOOTHED cutoffs.
    /// `n_classes` entries. SITA-E's target is `1/N` each — deviation
    /// indicates the EMA hasn't converged (or the distribution shifted).
    /// Feeds `rio_scheduler_class_load_fraction`.
    pub load_fractions: Vec<f64>,
    /// How many samples fed this pass. Useful for logging; also lets
    /// the caller distinguish "barely over threshold" from "well fed".
    pub sample_count: usize,
}

/// Compute new cutoffs from raw samples. Pure — caller applies.
///
/// `samples`: `(duration_secs, peak_memory_bytes)` pairs. Memory is
/// currently ignored (SITA-E partitions on duration alone; memory
/// feeds `assignment::classify`'s mem-bump separately). Kept in the
/// signature so P0230's caller passes the full query result without
/// a reshape — and so a future 2-D partition has the data on hand.
///
/// `prev_cutoffs`: last pass's smoothed cutoffs. If length doesn't
/// match `n_classes - 1` (first run, or operator changed `n_classes`),
/// smoothing is skipped — raw cutoffs returned directly.
///
/// Returns `None` if `samples.len() < cfg.min_samples`.
pub fn compute_cutoffs(
    samples: &[(f64, i64)],
    n_classes: usize,
    prev_cutoffs: &[f64],
    cfg: &RebalancerConfig,
) -> Option<RebalanceResult> {
    if samples.len() < cfg.min_samples {
        return None;
    }

    // Sort by duration. Memory column ignored for partitioning.
    let mut durations: Vec<f64> = samples.iter().map(|(d, _)| *d).collect();

    // After the min_samples gate passes, samples.len() > 0 is
    // guaranteed IFF min_samples >= 1. If a caller passes min_samples=0
    // (possible now that RebalancerConfig is Deserialize-able from TOML),
    // durations can be empty here and len()-1 below wraps. Guard explicitly.
    if durations.is_empty() {
        return None;
    }
    // total_cmp: defines NaN ordering (NaN sorts last under IEEE 754
    // total order) — same defense as assignment.rs:99. Samples come
    // from PG DOUBLE PRECISION NOT NULL so NaN is unlikely, but a
    // panic in a background task on one bad row is not a good trade.
    durations.sort_by(|a, b| a.total_cmp(b));

    // n_classes <= 1 → no boundaries. Still return a result (with
    // one load fraction = 1.0) so the gauge gets a value.
    if n_classes <= 1 {
        return Some(RebalanceResult {
            new_cutoffs: Vec::new(),
            load_fractions: vec![1.0],
            sample_count: samples.len(),
        });
    }

    // Cumulative sum. cumsum[i] = sum of durations[0..=i].
    let total: f64 = durations.iter().sum();
    let mut cumsum = Vec::with_capacity(durations.len());
    let mut acc = 0.0;
    for d in &durations {
        acc += d;
        cumsum.push(acc);
    }

    // Bisect at total/N * i for each boundary. partition_point is
    // binary search for "first index where predicate is false" —
    // here, first index where cumsum >= target. That index's duration
    // is the cutoff: everything at-or-before contributes <= target
    // load to the left of this boundary.
    let mut raw_cutoffs = Vec::with_capacity(n_classes - 1);
    for i in 1..n_classes {
        let target = total * (i as f64) / (n_classes as f64);
        let idx = cumsum.partition_point(|&c| c < target);
        // idx can equal len() if all cumsums are < target (only when
        // total is 0, i.e., all durations are zero). Clamp to last.
        raw_cutoffs.push(durations[idx.min(durations.len() - 1)]);
    }

    // EMA smooth against previous. Skip if shapes don't match (first
    // run or n_classes changed) — there's no meaningful "previous" to
    // blend against.
    let smoothed: Vec<f64> = if prev_cutoffs.len() == raw_cutoffs.len() {
        raw_cutoffs
            .iter()
            .zip(prev_cutoffs)
            .map(|(new, old)| cfg.ema_alpha * new + (1.0 - cfg.ema_alpha) * old)
            .collect()
    } else {
        raw_cutoffs
    };

    // Per-class load fractions under the SMOOTHED cutoffs. Computed
    // on smoothed (not raw) so the gauge reflects what the new config
    // will actually produce — that's the operator-relevant number.
    let load_fractions = compute_load_fractions(&durations, &smoothed, total);

    Some(RebalanceResult {
        new_cutoffs: smoothed,
        load_fractions,
        sample_count: samples.len(),
    })
}

/// Partition sorted durations by cutoffs, sum each bucket, normalize.
///
/// `durations` must be sorted ascending. `cutoffs` need not be —
/// an in-flight EMA can briefly produce non-monotone boundaries when
/// `prev_cutoffs` was far off. We sort a local copy before
/// partitioning so the buckets are contiguous.
///
/// Semantics match `assignment::classify`: class `i` is the smallest
/// class whose cutoff >= duration. Last class is open-ended (catches
/// everything above the largest cutoff).
///
/// Separate function primarily so the degenerate-cutoff test can
/// exercise it in isolation — it's the fraction math that's most
/// likely to NaN (total == 0 → 0/0).
fn compute_load_fractions(durations: &[f64], cutoffs: &[f64], total: f64) -> Vec<f64> {
    let n_classes = cutoffs.len() + 1;

    // All-zero durations → total is 0 → every fraction would be NaN.
    // Return uniform 1/N instead: degenerate, but the gauge gets a
    // finite value and alerting doesn't fire spuriously.
    if total == 0.0 {
        return vec![1.0 / n_classes as f64; n_classes];
    }

    // Sort cutoffs locally. See doc comment — EMA can transiently
    // produce non-monotone cutoffs; the partition walk below assumes
    // monotone.
    let mut sorted_cutoffs: Vec<f64> = cutoffs.to_vec();
    sorted_cutoffs.sort_by(|a, b| a.total_cmp(b));

    let mut fractions = vec![0.0; n_classes];
    let mut class_idx = 0;
    for &d in durations {
        // Advance to the covering class. `<` not `<=` so a duration
        // exactly at a cutoff lands in the class WITH that cutoff
        // (matches assignment::classify's `est_dur > cutoff` continue
        // — i.e., `<=` routes here).
        while class_idx < sorted_cutoffs.len() && sorted_cutoffs[class_idx] < d {
            class_idx += 1;
        }
        fractions[class_idx] += d;
    }

    for f in &mut fractions {
        *f /= total;
    }
    fractions
}

/// Rebalancer tick interval. Hourly — the sample query is bounded by
/// `cfg.lookback_days` (default 7), so workload drift faster than
/// 1/hour isn't captured by the query anyway. Writes to the RwLock
/// are correspondingly rare → near-zero contention with dispatch
/// reads.
///
/// cfg(test) shadow: 100ms so integration tests can observe one pass
/// without waiting an hour. Same pattern as actor's RECONCILE_DELAY.
#[cfg(not(test))]
const REBALANCE_INTERVAL: Duration = Duration::from_secs(3600);
#[cfg(test)]
const REBALANCE_INTERVAL: Duration = Duration::from_millis(100);

/// One rebalancer pass: query samples, compute new cutoffs, write
/// through the shared `size_classes` RwLock.
///
/// Returns the [`RebalanceResult`] if cutoffs were updated, `None` if
/// skipped (too few samples, `size_classes` empty, or query error).
/// Re-emits `rio_scheduler_cutoff_seconds` just after the write (reads
/// the same post-write state `classify()` will see). [`spawn_task`]
/// layers `class_load_fraction` on top from the returned
/// `load_fractions`.
///
/// ## N classes → N cutoffs
///
/// Each `SizeClassConfig` has its own `cutoff_secs` — the UPPER bound
/// of that class. N classes = N upper bounds = N cutoffs. We pass
/// `n = N + 1` to [`compute_cutoffs`] so it partitions samples into
/// N+1 buckets and returns N boundaries. The (N+1)th "virtual" bucket
/// is the overflow catch-all that `classify()` routes to the last
/// class anyway. This lets the rebalancer resize ALL cutoffs,
/// including the largest.
///
/// `zip` naturally writes all N (both iterators have N elements).
pub async fn apply_pass(
    db: &SchedulerDb,
    size_classes: &Arc<RwLock<Vec<SizeClassConfig>>>,
    cfg: &RebalancerConfig,
) -> Option<RebalanceResult> {
    // Snapshot under read lock. Quick: N clones of f64, then drop
    // the guard BEFORE the async query so we don't hold a sync lock
    // across .await (that would block the executor thread).
    let (n, prev): (usize, Vec<f64>) = {
        let guard = size_classes.read();
        if guard.is_empty() {
            // Size-classes disabled. Nothing to rebalance.
            return None;
        }
        (guard.len(), guard.iter().map(|c| c.cutoff_secs).collect())
    };

    let samples = match db.query_build_samples_last_days(cfg.lookback_days).await {
        Ok(s) => s,
        Err(e) => {
            warn!(?e, "rebalancer sample query failed; skipping this pass");
            return None;
        }
    };

    // n+1 → compute_cutoffs returns n boundaries (see doc comment).
    let result = compute_cutoffs(&samples, n + 1, &prev, cfg)?;

    // Write through the lock. Short critical section: N f64
    // assignments. Readers (dispatch's classify() path) see a
    // consistent snapshot — either all-old or all-new cutoffs,
    // never a half-written vec.
    {
        let mut guard = size_classes.write();
        for (cls, cutoff) in guard.iter_mut().zip(&result.new_cutoffs) {
            cls.cutoff_secs = *cutoff;
        }
    }

    // r[impl sched.rebalancer.sita-e]
    // Re-emit cutoff_seconds — the RwLock was just written. Without
    // this, the gauge stays at the TOML-config value main.rs set at
    // startup, while classify() routes against the rebalanced value.
    // Operators correlating class_queue_depth with cutoff_seconds
    // would chase a ghost (see P0366 operator-workflow note).
    //
    // Reads the post-write state (not result.new_cutoffs directly) —
    // this is what classify() will see. The extra read-lock acquire
    // is hourly for microseconds.
    {
        let guard = size_classes.read();
        for cls in guard.iter() {
            metrics::gauge!("rio_scheduler_cutoff_seconds", "class" => cls.name.clone())
                .set(cls.cutoff_secs);
        }
    }

    debug!(
        sample_count = result.sample_count,
        new_cutoffs = ?result.new_cutoffs,
        "rebalancer pass: cutoffs updated"
    );
    Some(result)
}

/// Spawn the rebalancer background task. Called from the actor's
/// `run_inner` at startup. The task runs [`apply_pass`] hourly
/// (`REBALANCE_INTERVAL`) until the actor's shutdown token cancels.
///
/// Emits `rio_scheduler_class_load_fraction` gauge per class after
/// each successful pass. The `rio_scheduler_cutoff_seconds` gauge is
/// re-emitted by [`apply_pass`] itself (just after the RwLock write).
///
/// No join handle returned — the task is fire-and-forget. If the
/// actor loop exits, the shutdown token cancels and this task drops
/// its Arc clone of size_classes.
pub fn spawn_task(
    db: SchedulerDb,
    size_classes: Arc<RwLock<Vec<SizeClassConfig>>>,
    shutdown: rio_common::signal::Token,
) {
    // Snapshot class names ONCE for the gauge labels. The rebalancer
    // only rewrites cutoff_secs, never names — so this snapshot stays
    // valid for the task's lifetime. Avoids re-reading the lock just
    // to fetch labels after every pass.
    let class_names: Vec<String> = size_classes.read().iter().map(|c| c.name.clone()).collect();
    if class_names.is_empty() {
        // Nothing to do. Don't spawn a task that'll no-op forever.
        return;
    }

    // Not spawn_periodic: skip-first-tick + post-pass gauge emit (using
    // captured class_names snapshot) makes the body non-trivial; the
    // biased; select below already satisfies r[common.task.periodic-biased].
    rio_common::task::spawn_monitored("rebalancer", async move {
        let cfg = RebalancerConfig::default();
        let mut interval = tokio::time::interval(REBALANCE_INTERVAL);
        // Skip the immediate first tick — on scheduler startup the
        // sample table is probably still empty. First real pass
        // happens after one interval.
        interval.tick().await;

        info!(
            min_samples = cfg.min_samples,
            ema_alpha = cfg.ema_alpha,
            lookback_days = cfg.lookback_days,
            "rebalancer task started"
        );

        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    debug!("rebalancer task shutting down");
                    return;
                }
                _ = interval.tick() => {}
            }

            if let Some(result) = apply_pass(&db, &size_classes, &cfg).await {
                // Gauge: per-class load fraction under the NEW cutoffs.
                // SITA-E's target is 1/N each; deviation means the
                // EMA hasn't converged or the workload shifted.
                for (name, frac) in class_names.iter().zip(&result.load_fractions) {
                    metrics::gauge!(
                        "rio_scheduler_class_load_fraction",
                        "class" => name.clone()
                    )
                    .set(*frac);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests;
