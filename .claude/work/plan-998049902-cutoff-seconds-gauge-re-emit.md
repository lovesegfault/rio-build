# Plan 998049902: `rio_scheduler_cutoff_seconds` gauge re-emit on rebalance

Post-PASS review of [P0230](plan-0230-rwlock-wire-cpu-bump-classify.md). The `rio_scheduler_cutoff_seconds` gauge is emitted exactly once at startup ([`main.rs:301`](../../rio-scheduler/src/main.rs), inside the `for class in &cfg.size_classes` loop) — before `spawn_with_leader` wires the rebalancer. P0230 wired `apply_pass` to rewrite cutoffs hourly through the `Arc<RwLock<Vec<SizeClassConfig>>>`, but never re-emits the gauge. An operator querying `rio_scheduler_cutoff_seconds{class="small"}` sees the TOML config value (e.g., `30.0`) while `classify()` routes against the rebalanced value (e.g., `47.3` after SITA-E converged on a workload where 30s was too aggressive).

**Three places LIE post-P0230:**

1. [`main.rs:285-288`](../../rio-scheduler/src/main.rs) comment: "Static config — set once, never changes." False — `apply_pass` writes through the RwLock hourly.
2. [`lib.rs:165`](../../rio-scheduler/src/lib.rs) `describe_gauge!` text: "set once at config load, static". Same lie.
3. [`observability.md:106`](../../docs/src/observability.md) Scheduler Metrics table row: "set once at config load, static". Same lie, now in the spec.

**The operator workflow this breaks:** `rio_scheduler_class_queue_depth{class="small"}` spikes → operator checks `cutoff_seconds{class="small"}` to understand the boundary → sees `30.0` → concludes "30s boundary is too tight" → edits TOML → restarts scheduler → rebalancer immediately rewrites back to ~47s. The gauge told them nothing useful; they chased a ghost. Worse: `rio_scheduler_class_load_fraction` IS re-emitted in `spawn_task` ([`rebalancer.rs:354-360`](../../rio-scheduler/src/rebalancer.rs)) — so one half of the rebalancer's Prometheus surface is live-updated and the other half is frozen.

## Entry criteria

- [P0230](plan-0230-rwlock-wire-cpu-bump-classify.md) merged (`Arc<RwLock>` + `spawn_task` + `apply_pass` exist; cutoffs are live-rewritten)

## Tasks

### T1 — `fix(scheduler):` re-emit `cutoff_seconds` gauge after each successful `apply_pass`

MODIFY [`rio-scheduler/src/rebalancer.rs`](../../rio-scheduler/src/rebalancer.rs) in `spawn_task`, inside the `if let Some(result) = apply_pass(...)` block at `:350-361`. The existing `class_load_fraction` emit loop has exactly the right shape (zips `class_names` with `result.load_fractions`); add a second loop that zips `class_names` with `result.new_cutoffs`:

```rust
if let Some(result) = apply_pass(&db, &size_classes, &cfg).await {
    // Gauge: per-class load fraction under the NEW cutoffs.
    for (name, frac) in class_names.iter().zip(&result.load_fractions) {
        metrics::gauge!("rio_scheduler_class_load_fraction", "class" => name.clone())
            .set(*frac);
    }
    // r[impl sched.rebalancer.sita-e]
    // Re-emit cutoff_seconds — the RwLock was just written, this
    // gauge was last set at startup from TOML config. Without this,
    // operators see the config value while classify() routes
    // against the rebalanced value. load_fractions has N entries;
    // new_cutoffs has N (see apply_pass doc comment — one upper
    // bound per class).
    for (name, cutoff) in class_names.iter().zip(&result.new_cutoffs) {
        metrics::gauge!("rio_scheduler_cutoff_seconds", "class" => name.clone())
            .set(*cutoff);
    }
}
```

**Shape check at dispatch:** `RebalanceResult.new_cutoffs` has `n_classes` entries (per the `apply_pass` N+1-bucket comment at [`rebalancer.rs:244-252`](../../rio-scheduler/src/rebalancer.rs)) — same length as `class_names`. The `zip` is clean. If the compute-cutoffs call actually returns `n_classes - 1` (check `compute_cutoffs` callers — P0230's T1 passed `n = prev.len() + 1`), the zip stops early and the last class's gauge stays at startup value. Fix by reading through the RwLock after the write (which has all N `cutoff_secs` fields updated):

```rust
// Alternative — read the definitive post-write state:
{
    let guard = size_classes.read();
    for cls in guard.iter() {
        metrics::gauge!("rio_scheduler_cutoff_seconds", "class" => cls.name.clone())
            .set(cls.cutoff_secs);
    }
}
```

This is marginally more correct (reads what `classify()` will see) at the cost of an extra read-lock acquire. Prefer this form; the lock is hourly-acquired for microseconds.

### T2 — `fix(scheduler):` update `main.rs` startup comment — not static

MODIFY [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) at `:285-288`:

```rust
// Before:
// Emit cutoff gauges BEFORE spawn. Static config — set once,
// never changes. Operators correlate with class_queue_depth:

// After:
// Emit cutoff gauges at startup with the CONFIG values. The
// rebalancer (spawn_task in rebalancer.rs) re-emits hourly
// after each apply_pass writes new cutoffs through the RwLock.
// Operators correlate with class_queue_depth:
```

### T3 — `fix(scheduler):` update `describe_gauge!` text

MODIFY [`rio-scheduler/src/lib.rs`](../../rio-scheduler/src/lib.rs) at `:163-166`:

```rust
describe_gauge!(
    "rio_scheduler_cutoff_seconds",
    "Duration cutoff per class (labeled by class; initialized from \
     config, live-updated hourly by the SITA-E rebalancer)"
);
```

### T4 — `docs(obs):` update observability.md Scheduler Metrics row

MODIFY [`docs/src/observability.md`](../../docs/src/observability.md) at `:106` (the `rio_scheduler_cutoff_seconds` table row):

```markdown
| `rio_scheduler_cutoff_seconds` | Gauge | Duration cutoff per class (labeled by class; initialized from config, re-emitted hourly after each rebalancer pass — see `r[sched.rebalancer.sita-e]`) |
```

### T5 — `test(scheduler):` gauge updates after `apply_pass`

MODIFY [`rio-scheduler/src/rebalancer/tests.rs`](../../rio-scheduler/src/rebalancer/tests.rs). The existing `apply_pass_writes_cutoffs_through_rwlock` test at `:227` proves the RwLock write; add a sibling that proves the gauge emit. Needs a test-side metrics recorder — check if `CountingRecorder` from [`rio-test-support/src/metrics.rs`](../../rio-test-support/src/metrics.rs) captures gauge values (it captures counts; may need a `GaugeRecorder` variant — check at dispatch, or use `metrics::with_local_recorder` directly).

```rust
/// Regression: cutoff_seconds gauge re-emits after apply_pass.
/// Before this fix, main.rs:301 emitted once at startup and
/// spawn_task only emitted class_load_fraction. Operator saw
/// config values while classify() used rebalanced values.
// r[verify sched.rebalancer.sita-e]
#[tokio::test]
async fn cutoff_seconds_gauge_re_emitted_after_apply_pass() {
    let (_pg, db) = test_db_with_200_bimodal_samples().await;
    let size_classes = Arc::new(RwLock::new(vec![
        SizeClassConfig { name: "small".into(), cutoff_secs: 10.0, ..Default::default() },
        SizeClassConfig { name: "large".into(), cutoff_secs: 1000.0, ..Default::default() },
    ]));
    let cfg = RebalancerConfig { min_samples: 50, ..Default::default() };

    let snapshotter = install_test_recorder();
    // Run the same loop-body spawn_task does (can't use spawn_task
    // directly — it's a background task; extract loop body or
    // inline the gauge-emit block under test).
    let result = apply_pass(&db, &size_classes, &cfg).await
        .expect("200 samples > min_samples=50");
    // Simulate spawn_task's gauge emit:
    {
        let guard = size_classes.read();
        for cls in guard.iter() {
            metrics::gauge!("rio_scheduler_cutoff_seconds", "class" => cls.name.clone())
                .set(cls.cutoff_secs);
        }
    }

    let gauges = snapshotter.gauges();
    let small_cutoff = gauges.get("rio_scheduler_cutoff_seconds{class=small}")
        .expect("gauge emitted for small");
    assert_ne!(*small_cutoff, 10.0,
        "cutoff should have changed from config value — apply_pass wrote {}",
        result.new_cutoffs[0]);
}
```

**Simpler alternative** if the recorder plumbing is heavy: move T1's gauge-emit into `apply_pass` itself (emit just after the write-lock section at `:290`). Then `apply_pass_writes_cutoffs_through_rwlock` can assert on the gauge directly without simulating `spawn_task`'s loop body. Downside: `apply_pass` gains a side effect beyond the RwLock write (metrics emit). Acceptable — it already has tracing side effects (`debug!` at `:292`).

## Exit criteria

- `/nbr .#ci` green
- `grep 'rio_scheduler_cutoff_seconds' rio-scheduler/src/rebalancer.rs` → ≥1 hit (T1: gauge emit in spawn_task or apply_pass)
- `grep 'static\|set once' rio-scheduler/src/main.rs` — no match at `:285-288` (T2: comment updated)
- `grep 'static' rio-scheduler/src/lib.rs | grep cutoff_seconds` → empty (T3: describe text updated)
- `grep 'set once at config load, static' docs/src/observability.md` → empty (T4: table row updated)
- `cargo nextest run -p rio-scheduler cutoff_seconds_gauge_re_emitted_after_apply_pass` → pass (T5)
- **T1 mutation:** comment out the gauge-emit loop in spawn_task/apply_pass → T5 fails (gauge stays at config value `10.0`)
- `nix develop -c tracey query rule sched.rebalancer.sita-e` → shows ≥2 `verify` sites (T5 adds one to the existing `tests.rs:18` site)

## Tracey

References existing markers:
- `r[sched.rebalancer.sita-e]` — T1 serves (the gauge is the operator-facing surface of the rebalancer's output); T5 adds a second `r[verify]`
- `r[obs.metric.scheduler]` — T3+T4 update the metric description

No new markers — this fixes an existing gauge's behavior to match its spec.

## Files

```json files
[
  {"path": "rio-scheduler/src/rebalancer.rs", "action": "MODIFY", "note": "T1: +cutoff_seconds gauge emit in spawn_task loop (or apply_pass post-write block)"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T2: :285-288 comment — 'static' → 'initialized from config, re-emitted hourly'"},
  {"path": "rio-scheduler/src/lib.rs", "action": "MODIFY", "note": "T3: :163-166 describe_gauge! text — drop 'static'"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T4: :106 table row — drop 'set once, static'; add rebalancer cross-ref"},
  {"path": "rio-scheduler/src/rebalancer/tests.rs", "action": "MODIFY", "note": "T5: +cutoff_seconds_gauge_re_emitted_after_apply_pass test; r[verify sched.rebalancer.sita-e]"}
]
```

```
rio-scheduler/src/
├── rebalancer.rs         # T1: gauge emit
├── main.rs               # T2: comment fix
├── lib.rs                # T3: describe text
└── rebalancer/tests.rs   # T5: regression test
docs/src/
└── observability.md      # T4: table row
```

## Dependencies

```json deps
{"deps": [230], "soft_deps": [229, 304], "note": "discovered_from=230 (rev-p230). P0230 wired apply_pass to rewrite cutoffs hourly but spawn_task only re-emits class_load_fraction, never cutoff_seconds. Operator sees config values while routing uses rebalanced. main.rs:285 + lib.rs:165 + obs.md:106 all say 'static' — three lies. Soft-dep P0229 (compute_cutoffs exists — P0230's dep, transitively DONE if P0230 is). Soft-dep P0304-T95/T96 (RebalancerConfig Deserialize + min_samples guard — same rebalancer.rs file, non-overlapping: T1 here touches spawn_task :350+, those touch struct def :44-65 + config-load in main.rs). observability.md count=24 (HOT) — single table-row edit, rebase-clean against other row adds."}
```

**Depends on:** [P0230](plan-0230-rwlock-wire-cpu-bump-classify.md) — `Arc<RwLock>` + `apply_pass` + `spawn_task` exist; cutoffs are live-rewritten.

**Conflicts with:** [`rebalancer.rs`](../../rio-scheduler/src/rebalancer.rs) — [P0304-T95/T96](plan-0304-trivial-batch-p0222-harness.md) touch struct definition `:44-65` (Deserialize derive, min_samples doc); T1 here touches `spawn_task` at `:350-361`. Non-overlapping. [`main.rs`](../../rio-scheduler/src/main.rs) count=33 (HOT) — T2 is a 4-line comment edit at `:285-288`, no logic change. [`observability.md`](../../docs/src/observability.md) count=24 (HOT) — single table-row edit at `:106`.
