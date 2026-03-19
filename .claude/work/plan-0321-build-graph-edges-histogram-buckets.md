# Plan 0321: rio_scheduler_build_graph_edges histogram — missing bucket config

[P0312](plan-0312-bounded-edge-query-truncation-correctness.md) added the `rio_scheduler_build_graph_edges` histogram at [`db.rs:1367`](../../rio-scheduler/src/db.rs) with a comment at [`:1363-1366`](../../rio-scheduler/src/db.rs) saying "Operators spot builds approaching the implicit edge bound via the p99 of this histogram." The spec at [`observability.md:117`](../../docs/src/observability.md) documents suggested buckets `[100, 500, 1000, 5000, 10000, 20000]`.

But [`init_metrics()`](../../rio-common/src/observability.rs) at [`observability.rs:278-298`](../../rio-common/src/observability.rs) has only **five** `Matcher::Full` entries — all for duration/ratio metrics. No entry for `build_graph_edges`. The [`metrics-exporter-prometheus`](https://docs.rs/metrics-exporter-prometheus) default buckets are `[0.005, 0.01, 0.025, ..., 10.0]` — tuned for HTTP latencies in seconds. Edge counts range 0–20K (capped by the induced subgraph over 5000 nodes at 4× density). **Every single sample lands in the `+Inf` bucket.** The p99 the operators-watch comment promises is unusable: `histogram_quantile(0.99, ...)` over a single populated `+Inf` bucket returns `+Inf`.

This is the **first count-type histogram** in the codebase — every prior histogram measured durations or ratios. The [`metrics_registered.rs:91`](../../rio-scheduler/tests/metrics_registered.rs) test checks `describe_histogram` was called (it was — the metric is in `SPEC_METRICS` at [`:61`](../../rio-scheduler/tests/metrics_registered.rs)), but has no mechanism to catch missing bucket config. The test-gap is structural: any future histogram would hit the same silent-default-buckets failure.

## Entry criteria

- [P0312](plan-0312-bounded-edge-query-truncation-correctness.md) merged (histogram emit site at [`db.rs:1367`](../../rio-scheduler/src/db.rs) exists) — **DONE**

## Tasks

### T1 — `fix(common):` add GRAPH_EDGES_BUCKETS + 6th Matcher::Full

MODIFY [`rio-common/src/observability.rs`](../../rio-common/src/observability.rs) — add a new const after [`ASSIGNMENT_LATENCY_BUCKETS`](../../rio-common/src/observability.rs) at `:260`:

```rust
/// Histogram bucket boundaries for `rio_scheduler_build_graph_edges`.
///
/// Edge COUNT (not seconds) per GetBuildGraph response — first count-type
/// histogram in this file. Range is 0..~20K (induced subgraph over the
/// 5000-node cap at realistic 4× edge density). Default Prometheus buckets
/// [0.005..10.0] are useless here — every sample lands in +Inf. These
/// match the suggested buckets at observability.md:117.
const GRAPH_EDGES_BUCKETS: &[f64] = &[100.0, 500.0, 1000.0, 5000.0, 10000.0, 20000.0];
```

Then add a sixth `.set_buckets_for_metric` call in `init_metrics()` after `:298`:

```rust
        .set_buckets_for_metric(
            Matcher::Full("rio_scheduler_build_graph_edges".to_string()),
            GRAPH_EDGES_BUCKETS,
        )?
```

### T2 — `refactor(common):` extract HISTOGRAM_BUCKET_MAP — iterate instead of chain

The 5-soon-6 chained `.set_buckets_for_metric` calls are a maintenance trap — adding a histogram means remembering to add both a `describe_histogram!` call AND a Matcher::Full entry, and only the former is test-guarded. Collapse to a table:

MODIFY [`rio-common/src/observability.rs`](../../rio-common/src/observability.rs) — replace the chain at `:278-298` with iteration:

```rust
/// Every histogram that needs non-default bucket boundaries.
///
/// The metrics-exporter-prometheus default [0.005..10.0] only works for
/// HTTP-latency-shaped metrics. Everything rio emits is seconds-to-hours
/// (builds), sub-second-to-seconds (reconciles, assignment), dimensionless
/// ratio (critical-path accuracy), or counts (graph edges) — none fit.
///
/// Kept as a pub const so the per-crate metrics_registered tests can
/// assert every describe_histogram! has an entry here. Missing entry =
/// silent +Inf-only histogram; metrics_registered doesn't catch it.
pub const HISTOGRAM_BUCKET_MAP: &[(&str, &[f64])] = &[
    ("rio_scheduler_build_duration_seconds", BUILD_DURATION_BUCKETS),
    ("rio_worker_build_duration_seconds", BUILD_DURATION_BUCKETS),
    ("rio_scheduler_critical_path_accuracy", CRITICAL_PATH_ACCURACY_BUCKETS),
    ("rio_controller_reconcile_duration_seconds", RECONCILE_DURATION_BUCKETS),
    ("rio_scheduler_assignment_latency_seconds", ASSIGNMENT_LATENCY_BUCKETS),
    ("rio_scheduler_build_graph_edges", GRAPH_EDGES_BUCKETS),
    // recovery_duration_seconds: DELIBERATELY default-bucketed — recovery
    // is a cold-start PG scan, 10ms-10s range fits the default. Note it
    // here so the T3 test knows to exempt it.
];

pub fn init_metrics(addr: std::net::SocketAddr) -> anyhow::Result<()> {
    use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};

    let mut builder = PrometheusBuilder::new();
    for (name, buckets) in HISTOGRAM_BUCKET_MAP {
        builder = builder.set_buckets_for_metric(Matcher::Full((*name).to_string()), buckets)?;
    }
    builder
        .with_http_listener(addr)
        .install()
        .map_err(|e| anyhow::anyhow!("failed to install Prometheus exporter: {e}"))?;

    tracing::info!(addr = %addr, "Prometheus metrics exporter started");
    Ok(())
}
```

**Check at dispatch:** `rio_scheduler_recovery_duration_seconds` at [`observability.md:112`](../../docs/src/observability.md) — grep whether it has a `describe_histogram!` call. If it does and genuinely fits default buckets, add a `DEFAULT_BUCKETS_OK` const slice for the T3 test to consult. If it should have custom buckets too, add a 7th entry. The spec row doesn't give suggested buckets, so default-OK is plausible.

### T3 — `test(scheduler):` every scheduler histogram has a HISTOGRAM_BUCKET_MAP entry

> **P0336 landed first** — `metrics_registered.rs` collapsed to helper calls via `rio-test-support/src/metrics.rs`. The test below should land as a **third helper** in `rio-test-support/src/metrics.rs` (`assert_histograms_have_buckets(describe_fn, bucket_map, exempt)`), called from scheduler's `metrics_registered.rs` — NOT inlined here. The file is ~87 lines now; `all_spec_metrics_have_describe_call` ends at `:76`.

MODIFY [`rio-scheduler/tests/metrics_registered.rs`](../../rio-scheduler/tests/metrics_registered.rs) — add a new test after [`all_spec_metrics_have_describe_call`](../../rio-scheduler/tests/metrics_registered.rs) at `:76`:

```rust
// r[verify obs.metric.scheduler]
// Every histogram in SPEC_METRICS must have a HISTOGRAM_BUCKET_MAP entry
// (or be in DEFAULT_BUCKETS_OK). The existing describe-only test catches
// "forgot to describe" but NOT "forgot to configure buckets" — a histogram
// with only describe_histogram! and no Matcher::Full gets default
// [0.005..10.0] buckets. For count-type or long-duration metrics, every
// sample lands in +Inf and p99 is unusable. This caught
// rio_scheduler_build_graph_edges (P0321): described, emitted,
// spec'd with suggested buckets — but init_metrics had no Matcher for it.
#[test]
fn all_histograms_have_bucket_config() {
    use rio_common::observability::HISTOGRAM_BUCKET_MAP;

    // Histograms that genuinely fit [0.005..10.0]. Deliberately empty for
    // now — if recovery_duration_seconds is confirmed default-OK (T2
    // dispatch note), add it here.
    const DEFAULT_BUCKETS_OK: &[&str] = &[];

    // Which SPEC_METRICS are histograms? Reuse the DescribedNames recorder,
    // but track type this time.
    let histograms = /* ... collect via a type-aware Recorder ... */;

    let configured: std::collections::HashSet<&str> =
        HISTOGRAM_BUCKET_MAP.iter().map(|(n, _)| *n).collect();

    for h in &histograms {
        assert!(
            configured.contains(h.as_str()) || DEFAULT_BUCKETS_OK.contains(&h.as_str()),
            "histogram {h:?} has no HISTOGRAM_BUCKET_MAP entry — \
             every sample will land in the default +Inf bucket. \
             Either add an entry in observability.rs or add to DEFAULT_BUCKETS_OK \
             if [0.005..10.0] genuinely fits."
        );
    }
}
```

The existing `DescribedNames` recorder at `:67-89` captures all three metric types into one vec — need a variant that separates by type, OR filter `SPEC_METRICS` by a `_HISTOGRAM_SUFFIXES` heuristic (`_seconds`, `_accuracy`, `_edges` — fragile), OR maintain a `SPEC_HISTOGRAMS` const alongside `SPEC_METRICS`. The type-aware recorder is cleanest: extend `DescribedNames` to `Arc<Mutex<(Vec<String>, Vec<String>, Vec<String>)>>` for counter/gauge/histogram.

**Replicate to other crates:** [`rio-controller/tests/metrics_registered.rs`](../../rio-controller/tests/metrics_registered.rs), [`rio-worker/tests/metrics_registered.rs`](../../rio-worker/tests/metrics_registered.rs), [`rio-store/tests/metrics_registered.rs`](../../rio-store/tests/metrics_registered.rs), [`rio-gateway/tests/metrics_registered.rs`](../../rio-gateway/tests/metrics_registered.rs) — same test shape, each filters `HISTOGRAM_BUCKET_MAP` to its crate's prefix. Optional — scheduler is the motivating case; the structural guard value is highest there. Implementer's call on whether to sweep all five or just scheduler.

## Exit criteria

- `/nbr .#ci` green
- `grep -c 'GRAPH_EDGES_BUCKETS' rio-common/src/observability.rs` ≥ 2 (T1: const defined + referenced)
- `grep 'rio_scheduler_build_graph_edges' rio-common/src/observability.rs` → ≥1 hit in `HISTOGRAM_BUCKET_MAP` (T1+T2: the metric name appears in the table)
- `grep 'pub const HISTOGRAM_BUCKET_MAP' rio-common/src/observability.rs` → 1 hit (T2: table is pub for the test)
- `grep -c 'set_buckets_for_metric' rio-common/src/observability.rs` → 1 (T2: the chain collapsed to a loop — only one call site)
- `cargo nextest run -p rio-scheduler all_histograms_have_bucket_config` → passes (T3)
- **Precondition self-check:** T3's test asserts its own precondition — `assert!(!histograms.is_empty(), "test collected zero histograms — recorder broken")` before the main loop. Without it, a broken recorder that collects nothing would vacuously pass.
- **Mutation criterion:** comment out the `("rio_scheduler_build_graph_edges", ...)` row in `HISTOGRAM_BUCKET_MAP` → T3 test fails with the "`no HISTOGRAM_BUCKET_MAP entry`" message. Restore the row → passes. (Proves the test catches the original bug.)

## Tracey

References existing markers:
- `r[obs.metric.scheduler]` — T1 fixes the bucket config for a metric in the [`observability.md:79`](../../docs/src/observability.md) scheduler table; T3 adds `r[verify obs.metric.scheduler]` on the bucket-coverage test

No new markers. The existing marker covers the scheduler metric table; bucket config is an implementation detail of making the spec'd buckets real.

## Files

```json files
[
  {"path": "rio-common/src/observability.rs", "action": "MODIFY", "note": "T1: +GRAPH_EDGES_BUCKETS const after :260. T2: +pub HISTOGRAM_BUCKET_MAP const, collapse .set_buckets_for_metric chain :278-298 to loop"},
  {"path": "rio-scheduler/tests/metrics_registered.rs", "action": "MODIFY", "note": "T3: +all_histograms_have_bucket_config test + type-aware recorder (extend DescribedNames to split by counter/gauge/histogram)"}
]
```

```
rio-common/src/
└── observability.rs              # T1+T2: +GRAPH_EDGES_BUCKETS, +HISTOGRAM_BUCKET_MAP, loop
rio-scheduler/tests/
└── metrics_registered.rs         # T3: bucket-coverage test
```

## Dependencies

```json deps
{"deps": [312], "soft_deps": [], "note": "P0312 DONE (histogram emit at db.rs:1367 exists). No file conflicts: observability.rs not in top-50 collisions; metrics_registered.rs test-only. Self-contained ~40-line fix. T2 refactor is optional-but-recommended — T1 alone fixes the immediate bug; T2+T3 prevent the NEXT count-type histogram from silently hitting the same trap. If time-constrained, T1 standalone + a TODO(P-later) for T2/T3 is acceptable — but the structural guard is the real value here (P0312 already proved the describe-only test doesn't catch this)."}
```

**Depends on:** [P0312](plan-0312-bounded-edge-query-truncation-correctness.md) — **DONE** at [`209bca09`](https://github.com/search?q=209bca09&type=commits). The `metrics::histogram!` emit site at [`db.rs:1367`](../../rio-scheduler/src/db.rs) is what T1's bucket config serves.

**Conflicts with:** None. [`observability.rs`](../../rio-common/src/observability.rs) is not in the top-50 collision table. [`metrics_registered.rs`](../../rio-scheduler/tests/metrics_registered.rs) is a test file with one existing test; T3 adds a sibling — purely additive.
