//! SITA-E cutoff rebalancer. Queries `build_samples`, partitions into
//! equal-load size classes, EMA-smooths against previous cutoffs.
//!
//! Pure module: [`compute_cutoffs`] returns new cutoffs but does NOT
//! apply them. P0230 wires the `Arc<RwLock<Vec<SizeClassConfig>>>`
//! write + spawns the background task that calls this on a timer.
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

/// Tunables for the rebalancer. All three are config-driven via
/// `scheduler.toml [rebalancer]` — defaults assume medium-volume
/// (~70 builds/day minimum at 500 samples / 7d). Workload-dependent;
/// operator tunes.
#[derive(Debug, Clone)]
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

#[cfg(test)]
mod tests;
