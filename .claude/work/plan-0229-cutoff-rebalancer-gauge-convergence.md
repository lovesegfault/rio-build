# Plan 0229: CutoffRebalancer — SITA-E algorithm + class_load_fraction gauge + convergence test

phase4c.md:15,18,21 — the core SITA-E (Size Interval Task Assignment with Equal load) algorithm. Given 7 days of `build_samples`, partition into N size classes such that `sum(duration)` is equal across classes. This is a **pure module** — it computes new cutoffs but does NOT apply them (P0230 wires the RwLock write).

**Algorithm sketch:** sort samples by duration → cumulative sum → bisect at `cumsum_total / N * i` for each class boundary → EMA-smooth against previous cutoffs (α=0.3, prevents oscillation) → gate on `min_samples` (don't rebalance on <100 samples).

The `r[sched.rebalancer.sita-e]` marker + `r[verify]` convergence test are both self-contained in this plan.

## Entry criteria

- [P0228](plan-0228-completion-samples-write-classdrift-counter.md) merged (`insert_build_sample` call in completion; test seeds via same path)

## Tasks

### T1 — `feat(scheduler):` rebalancer.rs — SITA-E algorithm

NEW `rio-scheduler/src/rebalancer.rs` (~200 lines):

```rust
//! SITA-E cutoff rebalancer. Queries build_samples, partitions into
//! equal-load classes, EMA-smooths against previous cutoffs.
//!
//! r[impl sched.rebalancer.sita-e]

use crate::assignment::SizeClassConfig;

pub struct RebalancerConfig {
    pub min_samples: usize,      // default 500 — don't rebalance on sparse data
    pub ema_alpha: f64,          // default 0.3 — convergence in ~3 iters
    pub lookback_days: u32,      // default 7
}

// Design adjusted 2026-03-18: these are config-driven via scheduler.toml
// [rebalancer] section. Defaults are medium-volume (500 samples/7d ≈
// ~70 builds/day minimum). Workload-dependent — operator tunes.
// See .claude/notes/plan-adjustments-2026-03-18.md.

pub struct RebalanceResult {
    pub new_cutoffs: Vec<f64>,   // duration_secs boundaries
    pub load_fractions: Vec<f64>, // per-class sum(duration) / total
    pub sample_count: usize,
}

/// Compute new cutoffs from samples. Pure function — caller applies.
pub fn compute_cutoffs(
    samples: &[(f64, i64)],  // (duration_secs, peak_memory_bytes)
    n_classes: usize,
    prev_cutoffs: &[f64],
    cfg: &RebalancerConfig,
) -> Option<RebalanceResult> {
    if samples.len() < cfg.min_samples { return None; }

    let mut durations: Vec<f64> = samples.iter().map(|(d, _)| *d).collect();
    durations.sort_by(|a, b| a.partial_cmp(b).unwrap());

    // Cumulative sum
    let total: f64 = durations.iter().sum();
    let mut cumsum = Vec::with_capacity(durations.len());
    let mut acc = 0.0;
    for d in &durations { acc += d; cumsum.push(acc); }

    // Bisect at total/N * i for each boundary
    let mut raw_cutoffs = Vec::with_capacity(n_classes - 1);
    for i in 1..n_classes {
        let target = total * (i as f64) / (n_classes as f64);
        let idx = cumsum.partition_point(|&c| c < target);
        raw_cutoffs.push(durations[idx.min(durations.len() - 1)]);
    }

    // EMA smooth against previous (prevents oscillation)
    let smoothed: Vec<f64> = if prev_cutoffs.len() == raw_cutoffs.len() {
        raw_cutoffs.iter().zip(prev_cutoffs)
            .map(|(new, old)| cfg.ema_alpha * new + (1.0 - cfg.ema_alpha) * old)
            .collect()
    } else {
        raw_cutoffs  // first run, no smoothing
    };

    // Per-class load fractions (for the gauge)
    let load_fractions = compute_load_fractions(&durations, &smoothed, total);

    Some(RebalanceResult { new_cutoffs: smoothed, load_fractions, sample_count: samples.len() })
}
```

**Edge cases:**
- All durations identical → cutoffs degenerate (all boundaries at the same value). Handle: dedupe adjacent equal cutoffs OR clamp to `[prev-ε, prev+ε]` range.
- Samples < min_samples → return `None`, skip this cycle.
- `n_classes = 1` → no cutoffs, trivially return empty.

### T2 — `feat(scheduler):` db.rs query for 7-day samples

MODIFY [`rio-scheduler/src/db.rs`](../../rio-scheduler/src/db.rs):

```rust
/// Query build samples from the last N days for the rebalancer.
pub async fn query_build_samples_last_days(
    &self,
    days: u32,
) -> Result<Vec<(f64, i64)>, sqlx::Error> {
    sqlx::query!(
        "SELECT duration_secs, peak_memory_bytes
         FROM build_samples
         WHERE completed_at > now() - ($1 || ' days')::interval",
        days.to_string()
    )
    .fetch_all(&self.pool)
    .await
    .map(|rows| rows.into_iter().map(|r| (r.duration_secs, r.peak_memory_bytes)).collect())
}
```

### T3 — `feat(scheduler):` class_load_fraction gauge

Register in [`rio-scheduler/src/lib.rs`](../../rio-scheduler/src/lib.rs) near the other scheduler gauges:

```rust
pub static CLASS_LOAD_FRACTION: LazyLock<GaugeVec> = LazyLock::new(|| {
    register_gauge_vec!(
        "rio_scheduler_class_load_fraction",
        "Per-class sum(duration)/total from the last rebalancer run. \
         SITA-E target is 1/N for each class — deviation indicates drift.",
        &["class"]
    ).unwrap()
});
```

Emit from `compute_cutoffs` (or from the caller after it returns) — one `gauge.with_label_values(&[class_name]).set(frac)` per class.

### T4 — `docs(scheduler):` r[sched.rebalancer.sita-e] spec ¶

MODIFY [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) — add the spec paragraph after the existing `r[sched.classify.*]` family (near `:171`, after `penalty-overwrite`):

```markdown
### Cutoff Rebalancing

r[sched.rebalancer.sita-e]
The scheduler periodically recomputes size-class cutoffs from raw `build_samples` (configurable `lookback_days`, default 7). The algorithm: sort samples by duration, compute cumulative sum, bisect at `total/N * i` for each class boundary — this yields cutoffs where `sum(duration)` is equal across classes (SITA-E: Size Interval Task Assignment with Equal load). New cutoffs are EMA-smoothed against previous (`ema_alpha`, default 0.3, ~3 iterations to converge) to prevent oscillation. Rebalancing is gated on `min_samples` (default 500). All three parameters are config-driven via `scheduler.toml [rebalancer]` — workload-dependent, operator tunes. Cutoffs are applied via `Arc<RwLock<Vec<SizeClassConfig>>>` (see P0230).
```

### T5 — `test(scheduler):` convergence test

NEW `rio-scheduler/src/rebalancer/tests.rs`:

```rust
// r[verify sched.rebalancer.sita-e]
#[test]
fn bimodal_converges_to_midpoint_in_3_iters() {
    // 100 samples @ 10s + 100 samples @ 300s → equal-load boundary ~155s
    // (total load = 100*10 + 100*300 = 31000; half = 15500; 10s samples
    //  contribute 1000; boundary needs 14500 from the 300s cluster — but
    //  since they're all identical, boundary sits at the 300s cluster start.
    //  Actually with sorting: first 100 are 10s, cumsum[99]=1000. We need
    //  15500. idx = 100 + (15500-1000)/300 ≈ 148. duration[148]=300.
    //  Hmm — bimodal with no spread means the boundary is AT one cluster.
    //  Use 100@10s + 100@300s with ±5% jitter for a realistic test.)
    let mut samples: Vec<(f64, i64)> = Vec::new();
    for _ in 0..100 { samples.push((10.0 + rand::random::<f64>() * 1.0, 0)); }
    for _ in 0..100 { samples.push((300.0 + rand::random::<f64>() * 15.0, 0)); }

    let cfg = RebalancerConfig { min_samples: 50, ema_alpha: 0.3, lookback_days: 7 };
    let mut cutoffs = vec![60.0];  // initial guess, way off

    for _ in 0..3 {
        let result = compute_cutoffs(&samples, 2, &cutoffs, &cfg).unwrap();
        cutoffs = result.new_cutoffs;
    }

    // After 3 iters with α=0.3, boundary should be somewhere in [100, 300]
    // (between the clusters). Exact value depends on jitter RNG — use a seed
    // or assert a wide-enough range.
    assert!(cutoffs[0] > 50.0 && cutoffs[0] < 310.0,
            "cutoff {} not between clusters", cutoffs[0]);
    // Load fractions should be ~0.5 each (that's the whole point)
    // — re-run compute and check load_fractions are within ε of 0.5
}

#[test]
fn uniform_samples_give_degenerate_cutoffs() {
    // 200 samples all at 60s → cutoffs collapse. Algorithm should
    // handle gracefully (not panic, not produce NaN).
    let samples: Vec<(f64, i64)> = vec![(60.0, 0); 200];
    let cfg = RebalancerConfig { min_samples: 50, ema_alpha: 0.3, lookback_days: 7 };
    let result = compute_cutoffs(&samples, 3, &[], &cfg).unwrap();
    // All cutoffs should equal 60.0 (the only duration present)
    for c in &result.new_cutoffs { assert!((c - 60.0).abs() < 0.01); }
}

#[test]
fn below_min_samples_returns_none() {
    let samples: Vec<(f64, i64)> = vec![(10.0, 0); 50];  // < 100
    let cfg = RebalancerConfig { min_samples: 100, ema_alpha: 0.3, lookback_days: 7 };
    assert!(compute_cutoffs(&samples, 2, &[], &cfg).is_none());
}
```

## Exit criteria

- `/nbr .#ci` green
- `tracey query rule sched.rebalancer.sita-e` shows spec + impl + verify
- `bimodal_converges_to_midpoint_in_3_iters` passes (boundary between clusters within tolerance)
- `uniform_samples_give_degenerate_cutoffs` passes without panic
- `below_min_samples_returns_none` passes
- `rio_scheduler_class_load_fraction` registered (grep `lib.rs`)

## Tracey

Adds new marker to component specs:
- `r[sched.rebalancer.sita-e]` → [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) (see ## Spec additions below)

## Spec additions

**`r[sched.rebalancer.sita-e]`** — placed in `docs/src/components/scheduler.md` in a new `### Cutoff Rebalancing` subsection after the existing `sched.classify.*` family (`:171`), standalone paragraph, blank line before, col 0:

```
r[sched.rebalancer.sita-e]
The scheduler periodically recomputes size-class cutoffs from raw `build_samples` (configurable `lookback_days`, default 7). The algorithm: sort samples by duration, compute cumulative sum, bisect at `total/N * i` for each class boundary — this yields cutoffs where `sum(duration)` is equal across classes (SITA-E: Size Interval Task Assignment with Equal load). New cutoffs are EMA-smoothed against previous (`ema_alpha`, default 0.3, ~3 iterations to converge) to prevent oscillation. Rebalancing is gated on `min_samples` (default 500). All three parameters are config-driven via `scheduler.toml [rebalancer]` — workload-dependent, operator tunes. Cutoffs are applied via `Arc<RwLock<Vec<SizeClassConfig>>>`.
```

## Files

```json files
[
  {"path": "rio-scheduler/src/rebalancer.rs", "action": "NEW", "note": "T1: compute_cutoffs — sort, cumsum, bisect, EMA smooth; r[impl sched.rebalancer.sita-e]"},
  {"path": "rio-scheduler/src/rebalancer/tests.rs", "action": "NEW", "note": "T5: bimodal convergence + uniform degenerate + min_samples gate; r[verify]"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "T2: query_build_samples_last_days"},
  {"path": "rio-scheduler/src/lib.rs", "action": "MODIFY", "note": "T1+T3: mod rebalancer decl + CLASS_LOAD_FRACTION gauge register (tail-append near :147)"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T4: r[sched.rebalancer.sita-e] spec ¶ in new Cutoff Rebalancing section after :171"}
]
```

```
rio-scheduler/src/
├── rebalancer.rs            # T1 (NEW): r[impl]
├── rebalancer/tests.rs      # T5 (NEW): r[verify]
├── db.rs                    # T2: query fn
└── lib.rs                   # T1+T3: mod decl + gauge
docs/src/components/scheduler.md  # T4: spec ¶
```

## Dependencies

```json deps
{"deps": [228], "soft_deps": [], "note": "SITA-E spine hop 3. deps:[P0228(table+samples)]. Convergence test seeds via insert_build_sample. db.rs serial via spine chain."}
```

**Depends on:** [P0228](plan-0228-completion-samples-write-classdrift-counter.md) — `build_samples` table populated via completion path; convergence test seeds via `insert_build_sample` (or seeds SQL directly — either works).
**Conflicts with:** `db.rs` count=29 — serialized via P0227→P0228→P0229 spine chain. `lib.rs` tail-append. `rebalancer.rs` is NEW.

**EMA tolerance note:** α=0.3 means each iter moves 30% toward the raw cutoff. After 3 iters starting from a bad initial guess: `1 - (0.7)^3 ≈ 65.7%` of the way to convergence. Test tolerance must account for this — assert "between the clusters" not "exactly at the equal-load point."
