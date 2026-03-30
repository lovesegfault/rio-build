# Plan 515: manifest-mode sweep cap divergence — `FAILED_SWEEP_PER_TICK` < `replicas.max`

[P0511](plan-0511-manifest-failed-job-sweep.md) shipped the Failed-Job sweep with a fixed `FAILED_SWEEP_PER_TICK=20` cap. The doc-comment at [`manifest.rs:113-119`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) argues convergence: "20 > 1" — sweep rate beats accumulation rate. The `1` assumes one Failed Job per tick. That's wrong.

The actual accumulation rate is `headroom` per tick, bounded by `spec.replicas.max`. At [`:270`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) `headroom = ceiling.saturating_sub(active_total)` — under a full crash-loop (`backoff_limit=0` + immediate crash, e.g., bad image tag), ALL `headroom`-worth of spawned Jobs fail before the next tick. `active_total` stays at zero (Failed are filtered from it at [`:221-225`](../../rio-controller/src/reconcilers/builderpool/manifest.rs)), so next tick spawns another full `headroom`. Net accumulation = `headroom - 20` per tick.

With `replicas.max = 20`: net zero, converges. With `replicas.max = 50` (plausible for a large pool): net `+30/tick` → **unbounded growth**. The sweep never catches up; the Failed-Job pile grows linearly forever. The [P0511 Risks § "Bounded sweep vs unbounded accumulation"](plan-0511-manifest-failed-job-sweep.md) ran the convergence math with the wrong accumulation term.

P0511-review validator: "converges iff `replicas.max < 20`." Fix is small (const→param); the cap should scale with the pool's own max, not a hardcoded floor.

## Entry criteria

- [P0511](plan-0511-manifest-failed-job-sweep.md) merged (DONE — `FAILED_SWEEP_PER_TICK` + `select_failed_jobs` exist)

## Tasks

### T1 — `fix(controller):` sweep cap = `max(20, replicas.max)` — pass as param, not const

MODIFY [`rio-controller/src/reconcilers/builderpool/manifest.rs`](../../rio-controller/src/reconcilers/builderpool/manifest.rs).

`select_failed_jobs` at [`:837-842`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) currently `.take(FAILED_SWEEP_PER_TICK)` with the const. Change signature + call site:

```rust
// :837 — add cap param, drop internal .take(const)
pub(super) fn select_failed_jobs(jobs: &[Job], cap: usize) -> Vec<&Job> {
    jobs.iter()
        .filter(|j| j.status.as_ref().and_then(|s| s.failed).unwrap_or(0) > 0)
        .take(cap)
        .collect()
}

// :239 — call site computes cap from the pool's own replicas.max
let sweep_cap = FAILED_SWEEP_MIN.max(wp.spec.replicas.max as usize);
let failed_jobs = select_failed_jobs(&jobs.items, sweep_cap);
```

Rename the const `FAILED_SWEEP_PER_TICK` → `FAILED_SWEEP_MIN` at [`:120`](../../rio-controller/src/reconcilers/builderpool/manifest.rs). It's now the floor, not the cap. Rewrite the doc-comment at [`:113-119`](../../rio-controller/src/reconcilers/builderpool/manifest.rs):

```rust
/// Floor for per-tick Failed-Job deletes. The actual cap is
/// `max(FAILED_SWEEP_MIN, spec.replicas.max)` — under a full crash-
/// loop (bad image, backoff_limit=0), the spawn pass fires `headroom`
/// replacements every tick, bounded by `replicas.max`. The sweep MUST
/// clear at least that many to converge (net ≤ 0/tick). This floor
/// guarantees small pools (max<20) still sweep at a reasonable rate
/// and clear historical backlog. Operators wanting instant clear:
/// `kubectl delete jobs -l rio.build/sizing=manifest --field-selector
/// status.successful=0`.
pub(super) const FAILED_SWEEP_MIN: usize = 20;
```

The `CrashLoopDetected` event message at [`:416-422`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) interpolates the old const name — update to `sweep_cap` (the computed value, more useful to an operator than a static 20).

### T2 — `fix(controller):` sort Failed Jobs oldest-first before capping

Bonus from the P0511-review: `select_failed_jobs` returns in `jobs_api.list()` order (effectively random from the operator's perspective). Under a backlog, capping a random subset means **the oldest crash** might never get swept — it keeps being pushed out by younger ones. Sort by `metadata.creation_timestamp` before `.take()`:

```rust
pub(super) fn select_failed_jobs(jobs: &[Job], cap: usize) -> Vec<&Job> {
    let mut failed: Vec<&Job> = jobs.iter()
        .filter(|j| j.status.as_ref().and_then(|s| s.failed).unwrap_or(0) > 0)
        .collect();
    // Oldest-first: under backlog, sweep the crashes that have been
    // sitting longest. list() order is apiserver-arbitrary; without
    // this a persistently-oldest Failed Job may never get selected.
    failed.sort_by_key(|j| j.metadata.creation_timestamp.clone());
    failed.truncate(cap);
    failed
}
```

`Option<Time>` sorts `None` first — a Failed Job with no timestamp (pathological but possible in test mocks) is treated as oldest, which is the safe direction.

### T3 — `test(controller):` convergence proof — sweep cap tracks `replicas.max`

MODIFY [`rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs`](../../rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs). The existing `failed_sweep_bounded_per_tick` (from P0511-T3) asserts `≤ FAILED_SWEEP_PER_TICK`; adapt it to the new param, and add the convergence case:

```rust
// r[verify ctrl.pool.manifest-failed-sweep]
#[test]
fn failed_sweep_cap_tracks_replicas_max() {
    // CONVERGENCE PROOF: the sweep cap must be ≥ replicas.max,
    // otherwise a full crash-loop (every headroom-worth fails per
    // tick) accumulates net +(max - cap)/tick forever. P0511's
    // fixed cap=20 diverged for max≥20.
    //
    // Small pool: floor dominates.
    assert_eq!(FAILED_SWEEP_MIN.max(5_usize), 20);
    // Large pool: replicas.max dominates. This is the load-bearing
    // case — cap=50 means the sweep can clear a full 50-headroom
    // tick's worth of crashes, net ≤ 0.
    assert_eq!(FAILED_SWEEP_MIN.max(50_usize), 50);

    // 50 Failed Jobs, cap=50 → all selected (not truncated to 20).
    let failed: Vec<Job> = (0..50).map(|i| failed_job(&format!("f-{i}"))).collect();
    let selected = select_failed_jobs(&failed, 50);
    assert_eq!(selected.len(), 50, "large-pool cap not artificially low");

    // Same 50 Failed, cap=20 (small pool) → bounded.
    let selected_small = select_failed_jobs(&failed, 20);
    assert_eq!(selected_small.len(), 20);
}

#[test]
fn failed_sweep_oldest_first_under_backlog() {
    // T2: sort before truncate. Without this, a random-order
    // .take(cap) may keep skipping the same old Job forever.
    let mut jobs: Vec<Job> = (0..5).map(|i| failed_job(&format!("f-{i}"))).collect();
    // Make f-2 the oldest, f-0 the newest (out-of-order).
    set_creation_timestamp(&mut jobs[2], "2026-01-01T00:00:00Z");
    set_creation_timestamp(&mut jobs[0], "2026-03-30T00:00:00Z");
    let selected = select_failed_jobs(&jobs, 1);
    assert_eq!(selected[0].metadata.name.as_deref(), Some("f-2"),
        "oldest (f-2) selected, not list-first (f-0)");
}
```

## Exit criteria

- `grep 'FAILED_SWEEP_PER_TICK' rio-controller/src/reconcilers/builderpool/manifest.rs` → 0 hits (const renamed)
- `grep 'FAILED_SWEEP_MIN' rio-controller/src/reconcilers/builderpool/manifest.rs` → ≥2 hits (const def + `max()` call site)
- `grep 'cap: usize' rio-controller/src/reconcilers/builderpool/manifest.rs` → ≥1 hit in `select_failed_jobs` signature
- `grep 'sort_by_key.*creation_timestamp' rio-controller/src/reconcilers/builderpool/manifest.rs` → ≥1 hit
- `cargo nextest run -p rio-controller failed_sweep_cap_tracks_replicas_max failed_sweep_oldest_first_under_backlog` → 2 passed
- Existing `failed_sweep_bounded_per_tick` test either adapted to new signature or superseded (no broken test left behind)
- `nix develop -c cargo clippy --all-targets -- --deny warnings` green

## Tracey

References existing markers:
- `r[ctrl.pool.manifest-failed-sweep]` — T1 corrects the convergence guarantee this marker describes. T3's `failed_sweep_cap_tracks_replicas_max` is `r[verify]`. **Spec text at [`controller.md:157`](../../docs/src/components/controller.md) says "bounded per-tick (default 20)" — T1 changes the bound from a fixed default to `max(20, replicas.max)`. Bump the marker (`tracey bump`) since the normative bound changed.**

## Spec additions

MODIFY [`docs/src/components/controller.md`](../../docs/src/components/controller.md) at `r[ctrl.pool.manifest-failed-sweep]` (line `:156-157`). Replace "bounded per-tick (default 20)" with "bounded per-tick to `max(20, spec.replicas.max)` — the cap tracks the pool's own spawn ceiling so the sweep converges under full crash-loop".

Run `tracey bump` before committing; existing `r[impl ctrl.pool.manifest-failed-sweep]` annotations become stale until T1 updates them.

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/builderpool/manifest.rs", "action": "MODIFY", "note": "T1: FAILED_SWEEP_PER_TICK → FAILED_SWEEP_MIN; select_failed_jobs(+cap param); :239 call site + :113-119 doc-comment + :416 event message. T2: sort_by_key creation_timestamp before truncate at :838"},
  {"path": "rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs", "action": "MODIFY", "note": "T3: failed_sweep_cap_tracks_replicas_max + failed_sweep_oldest_first_under_backlog; adapt failed_sweep_bounded_per_tick to new signature"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "r[ctrl.pool.manifest-failed-sweep] text update + tracey bump — bound changed from fixed-20 to max(20, replicas.max)"}
]
```

```
rio-controller/src/reconcilers/builderpool/
├── manifest.rs           # T1: const→param; T2: sort oldest-first
└── tests/
    └── manifest_tests.rs # T3: convergence proof + oldest-first
docs/src/components/
└── controller.md         # marker bump — bound semantics changed
```

## Dependencies

```json deps
{"deps": [511], "soft_deps": [513, 516], "note": "P0511 DONE (const + fn exist). Soft-dep P0513: job_common extraction — select_failed_jobs may move to job_common.rs alongside is_active_job; if P0513 lands first, apply T1+T2 in the new location. Soft-dep P0516: both touch manifest.rs post-P0511; this plan is ~15L net (const rename + param add + sort), P0516 reorders ~50L — THIS FIRST. discovered_from=511."}
```

**Depends on:** [P0511](plan-0511-manifest-failed-job-sweep.md) — DONE. `FAILED_SWEEP_PER_TICK` const at [`:120`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) and `select_failed_jobs` at [`:837`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) are the subjects.

**Conflicts with:** `manifest.rs` not in top-20 collisions. [P0516](plan-0516-manifest-quota-deadlock.md) touches `:309` + `:393` (spawn short-circuit + sweep site — different lines, but semantic overlap in the Failed-sweep flow). Serialize: this plan FIRST (smaller). [P0513](plan-0513-job-common-extraction.md) extracts `is_active_job` from `:202-206` and may absorb `select_failed_jobs` as `is_failed_job` inverse — T1's param-add survives the move (it's a signature change, not a location-dependent edit). [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T501 touches `manifest_tests.rs` (complement test) — additive test fns, non-overlapping.

## Risks

- **Const rename breaks callers:** `FAILED_SWEEP_PER_TICK` is `pub(super)` — grep `rio-controller/` for other reference sites before renaming. `manifest_tests.rs` imports it (P0511-T3 uses it in `failed_sweep_bounded_per_tick`).
- **`replicas.max` as usize:** `wp.spec.replicas.max` is `i32` in the CRD (k8s convention). `as usize` on a negative would wrap — but CEL validation at CRD admission enforces `max >= 1`. Safe.
- **Test helper `set_creation_timestamp` may not exist** — if the test fixture builders don't expose it, either add a thin helper to `manifest_tests.rs` or set `job.metadata.creation_timestamp` directly (it's `Option<Time>` public).
