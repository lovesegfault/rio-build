# Plan 0230: RwLock wire + CPU-bump classify — single actor/mod.rs + assignment.rs touch

phase4c.md:16-17 + GT3 + **GT12 marker rename** — two closely coupled changes bundled to minimize hot-file touches. (a) Wrap `size_classes` in `Arc<parking_lot::RwLock<...>>`, spawn the rebalancer task with a clone, have it write new cutoffs. (b) Add `cpu_limit_cores` to `SizeClassConfig` and a CPU-bump branch in `classify()` mirroring the existing mem-bump — **`ema_peak_cpu_cores` column already exists** (GT3: queried at [`db.rs:931`](../../rio-scheduler/src/db.rs), EMA-updated at [`db.rs:1128,1141`](../../rio-scheduler/src/db.rs)). No migration, no db.rs change.

**GT12 — MARKER RENAME:** phase doc says `sched.rebalancer.cpu-bump`. The existing family is `sched.classify.{smallest-covering,mem-bump,penalty-overwrite}` at [`scheduler.md:157-171`](../../docs/src/components/scheduler.md). **Use `sched.classify.cpu-bump`** for family consistency.

**DISPATCH SOLO.** `actor/mod.rs` collision count = 31 (#2 hottest file in the repo). No other 4c plan should be in flight when this dispatches (coordinator policy, not a dag dep).

> **DISPATCH NOTE (bughunter, rebalancer latent panic):** When wiring `RebalancerConfig` from `scheduler.toml` (this plan loads it — see T1's `RebalancerConfig::default()` line), add `anyhow::ensure!(cfg.min_samples >= 1, "rebalancer.min_samples must be ≥ 1")` at config-load. [`rebalancer.rs:135`](../../rio-scheduler/src/rebalancer.rs) does `durations[idx.min(durations.len() - 1)]` — if `min_samples=0` and the sample query returns empty, `durations.len()` is 0, `0usize - 1` wraps to `usize::MAX`, index panics. Default is `min_samples=500` ([`rebalancer.rs:51`](../../rio-scheduler/src/rebalancer.rs)) so unreachable today; becomes reachable the moment this plan loads `min_samples` from user-supplied TOML. One-line guard at the config-parse boundary, not in the hot path.

**A6 — `parking_lot::RwLock` (sync, no `.await`).** Already in `Cargo.lock`. Writes are hourly (rebalancer interval) → near-zero contention. Keeps `classify()` sync.

## Entry criteria

- [P0229](plan-0229-cutoff-rebalancer-gauge-convergence.md) merged (`rebalancer::compute_cutoffs` exists; this plan wires it into a spawned task)

## Tasks

### T1 — `feat(scheduler):` Arc<RwLock> in DagActor

MODIFY [`rio-scheduler/src/actor/mod.rs`](../../rio-scheduler/src/actor/mod.rs) — change the `size_classes` field:

```rust
// Before: size_classes: Vec<SizeClassConfig>
// After:
size_classes: Arc<parking_lot::RwLock<Vec<SizeClassConfig>>>,
```

Constructor: `Arc::new(parking_lot::RwLock::new(initial_config))`.

Spawn the rebalancer task (near the other bg task spawns in `DagActor::run` or wherever the actor loop setup lives):

```rust
// Rebalancer: hourly recompute, write to the shared RwLock.
tokio::spawn({
    let size_classes = Arc::clone(&self.size_classes);
    let db = self.db.clone();
    async move {
        let mut interval = tokio::time::interval(Duration::from_secs(3600));
        let cfg = RebalancerConfig::default();
        loop {
            interval.tick().await;
            let samples = match db.query_build_samples_last_days(cfg.lookback_days).await {
                Ok(s) => s,
                Err(e) => { warn!(?e, "rebalancer sample query failed"); continue; }
            };
            let prev: Vec<f64> = size_classes.read().iter().map(|c| c.cutoff_secs).collect();
            let n = prev.len() + 1;  // N classes have N-1 cutoffs
            if let Some(result) = compute_cutoffs(&samples, n, &prev, &cfg) {
                let mut guard = size_classes.write();
                for (cls, cutoff) in guard.iter_mut().zip(&result.new_cutoffs) {
                    cls.cutoff_secs = *cutoff;
                }
                // emit CLASS_LOAD_FRACTION gauges here
            }
        }
    }
});
```

### T2 — `refactor(scheduler):` classify callsites read through RwLock

MODIFY [`rio-scheduler/src/actor/dispatch.rs`](../../rio-scheduler/src/actor/dispatch.rs) — every `classify(...)` call site:

```rust
// Before: classify(dur, mem, &self.size_classes)
// After:
let classes = self.size_classes.read();
classify(dur, mem, &classes)
// guard drops here
```

`classify()` signature stays `fn classify(..., classes: &[SizeClassConfig]) -> ...` — unchanged. Callers pass the guard-derefed slice.

**R10 CHECK — no `.await` across the guard.** `parking_lot::RwLock` is sync; holding a guard across `.await` would block the executor thread. At dispatch: `grep -A3 'classify(' rio-scheduler/src/actor/dispatch.rs` — verify no `.await` between `.read()` and `classify()`. If there IS one, clone under the lock then drop the guard:

```rust
let classes: Vec<SizeClassConfig> = self.size_classes.read().clone();
// await-safe now
classify(dur, mem, &classes)
```

### T3 — `feat(scheduler):` cpu_limit_cores + CPU-bump in classify

MODIFY [`rio-scheduler/src/assignment.rs`](../../rio-scheduler/src/assignment.rs) — add field at `:55`:

```rust
pub struct SizeClassConfig {
    // ... existing fields ...
    /// CPU limit for this class (cores). If ema_peak_cpu_cores exceeds
    /// this, bump to the next class (mirrors mem-bump).
    pub cpu_limit_cores: Option<f64>,
}
```

Add CPU-bump branch in `classify()` at `:81`, mirroring the mem-bump pattern at [`scheduler.md:164`](../../docs/src/components/scheduler.md):

```rust
// r[impl sched.classify.cpu-bump]
// CPU-bump: if ema_peak_cpu_cores exceeds the duration-chosen class's
// cpu_limit, bump to the next larger class. Mirrors mem-bump.
if let Some(cpu_limit) = classes[chosen].cpu_limit_cores {
    if ema_peak_cpu_cores > cpu_limit && chosen + 1 < classes.len() {
        chosen += 1;
    }
}
```

Extend the test helper at `:538` to accept `cpu_limit_cores`.

### T4 — `test(scheduler):` proptest — CPU-bump never goes DOWN

Proptest in `assignment.rs` test module:

```rust
// r[verify sched.classify.cpu-bump]
proptest! {
    #[test]
    fn cpu_bump_monotone(
        dur in 0.0..1000.0f64,
        mem in 0i64..10_000_000_000,
        cpu in 0.0..16.0f64,
        classes in size_class_strategy(),
    ) {
        let without_cpu = classify(dur, mem, 0.0, &classes);  // cpu=0 → no bump
        let with_cpu = classify(dur, mem, cpu, &classes);
        // CPU-bump only moves UP (or stays put)
        prop_assert!(class_index(&with_cpu) >= class_index(&without_cpu));
    }
}
```

### T5 — `docs(scheduler):` r[sched.classify.cpu-bump] spec ¶

MODIFY [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) — add immediately after `r[sched.classify.mem-bump]` at `:165`:

```markdown
r[sched.classify.cpu-bump]
If `ema_peak_cpu_cores` for a derivation exceeds the target class's `cpu_limit_cores`, the derivation is bumped to the next larger class regardless of duration (resource-aware class bumping, mirroring mem-bump).
```

## Exit criteria

- `/nbr .#ci` green
- `tracey query rule sched.classify.cpu-bump` shows spec + impl + verify
- `cpu_bump_monotone` proptest passes (bump never goes down)
- Integration: seed bimodal samples → tick rebalancer → `size_classes.read()` cutoffs changed from initial

## Tracey

Adds new marker to component specs:
- `r[sched.classify.cpu-bump]` → [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) (see ## Spec additions below) — **RENAMED from phase doc's `sched.rebalancer.cpu-bump`** per GT12 for family consistency with `sched.classify.{smallest-covering,mem-bump,penalty-overwrite}`

## Spec additions

**`r[sched.classify.cpu-bump]`** — placed in `docs/src/components/scheduler.md` immediately after `r[sched.classify.mem-bump]` (`:165`), standalone paragraph, blank line before, col 0:

```
r[sched.classify.cpu-bump]
If `ema_peak_cpu_cores` for a derivation exceeds the target class's `cpu_limit_cores`, the derivation is bumped to the next larger class regardless of duration (resource-aware class bumping, mirroring mem-bump).
```

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "T1: Arc<parking_lot::RwLock<Vec<SizeClassConfig>>> field + rebalancer task spawn. HOTTEST 4c touch (count=31)."},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "T2: classify callsites read through .read() guard; verify no .await across guard"},
  {"path": "rio-scheduler/src/assignment.rs", "action": "MODIFY", "note": "T3+T4: cpu_limit_cores field at :55 + CPU-bump branch at :81 + proptest; r[impl]+r[verify sched.classify.cpu-bump]"},
  {"path": "rio-scheduler/src/rebalancer.rs", "action": "MODIFY", "note": "T1: write-to-lock (compute_cutoffs caller writes result.new_cutoffs via guard)"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T5: r[sched.classify.cpu-bump] spec ¶ after mem-bump at :165"}
]
```

```
rio-scheduler/src/
├── actor/
│   ├── mod.rs            # T1: RwLock + spawn (count=31 — SOLO dispatch)
│   └── dispatch.rs       # T2: .read() at callsites
├── assignment.rs         # T3+T4: r[impl]+r[verify] (count=7)
└── rebalancer.rs         # T1: write via guard
docs/src/components/scheduler.md  # T5: spec ¶
```

## Dependencies

```json deps
{"deps": [229], "soft_deps": [], "note": "SITA-E spine hop 4. DISPATCH SOLO — actor/mod.rs count=31 (#2 hottest). No other 4c plan concurrent (coordinator policy). Unblocks the hub (P0231)."}
```

**Depends on:** [P0229](plan-0229-cutoff-rebalancer-gauge-convergence.md) — `rebalancer::compute_cutoffs` must exist; the task spawn calls it.
**Conflicts with:** `actor/mod.rs` count=31 — P0231 also touches it (match arm) but deps on P0230 so serialized. `assignment.rs` count=7 — single 4c touch. **Dispatch SOLO per R4.**

**Hidden check at dispatch:** `grep -A3 'classify(' rio-scheduler/src/actor/dispatch.rs` — verify no `.await` between the read site and the `classify()` call. If found, clone under the lock then drop.
