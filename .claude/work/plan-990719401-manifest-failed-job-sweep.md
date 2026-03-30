# Plan 990719401: manifest-mode Failed-Job sweep — crash-loop amplifier fix

[`manifest.rs:913`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) sets `backoff_limit: Some(0)` with no `ttl_seconds_after_finished`. The code comment at `:925-929` frames the resulting Failed-Job accumulation as `kubectl get jobs` clutter — a cosmetic annoyance. The [P0505](plan-0505-manifest-scaledown-grace.md) review surfaced the sharper angle: **it is a crash-loop amplifier.**

A manifest pod crash-looping on boot (bad image, config error, OOM-on-start) is filtered from `active_jobs` at [`:202-206`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) (`status.failed > 0`) and thus doesn't count as supply. Next tick, the diff sees a deficit and spawns a replacement — which also crashes. At the default 10s requeue interval that is **1 Failed Job per tick = 360/hr = 8640/day**. The `spec.replicas.max` ceiling doesn't cap this: `active_total` at [`:210`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) is computed from the Failed-filtered `active_jobs` slice, so Failed Jobs are invisible to the ceiling check.

Compounding: `jobs_api.list()` at `:185` returns the growing Failed set on every tick. List latency scales with object count; reconcile gets progressively slower as the pile grows, while still adding one Job per tick. A BuilderPool with a transiently-bad image can accumulate thousands of dead Jobs before an operator notices.

The code comment names two fixes — this plan implements the first (Failed-Job sweep in the scale-down pass) as the safer option. TTL-based reaping would race the controller's deliberate scale-down deletes (a healthy-but-idle Job's `Complete` condition could fire TTL between the idle-grace check and the controller's `delete()` call).

## Entry criteria

- [P0505](plan-0505-manifest-scaledown-grace.md) merged (`select_deletable_jobs` owns the delete pass; this plan extends its scope from idle-surplus to idle-surplus + Failed)

## Tasks

### T1 — `fix(controller):` sweep Failed Jobs alongside idle-surplus deletes

MODIFY [`rio-controller/src/reconcilers/builderpool/manifest.rs`](../../rio-controller/src/reconcilers/builderpool/manifest.rs). After the `active_jobs` filter at `:199-207`, compute a second slice:

```rust
// Failed Jobs: not supply, not capacity, but still ours to reap.
// backoff_limit=0 means one pod crash → Job Failed permanently.
// Under crash-loop (bad image, OOM-on-start) these accumulate at
// 1/tick; sweep them alongside idle-surplus deletes. Bounded-per-
// tick to keep the delete burst manageable (don't fire 8000 deletes
// if the operator just fixed the image after a day of crash-loop).
let failed_jobs: Vec<&Job> = jobs.items.iter()
    .filter(|j| j.status.as_ref().and_then(|s| s.failed).unwrap_or(0) > 0)
    .collect();
const FAILED_SWEEP_PER_TICK: usize = 20;
```

Then in the delete pass (post-`select_deletable_jobs`), chain `failed_jobs.iter().take(FAILED_SWEEP_PER_TICK)` onto the delete loop. Failed Jobs need no idle-check (they have no pod to interrupt) — straight delete.

Update the code comment at `:925-929` from "Known gap … Followup needed" to a cross-ref to the sweep site. Keep the `backoff_limit=0` + no-TTL invariant — the sweep replaces TTL semantically.

### T2 — `fix(controller):` emit `CrashLoopDetected` event when Failed count crosses threshold

A Failed-Job pile is a strong operator signal. Before sweeping, if `failed_jobs.len() >= 3`, publish a K8s `Warning` event on the BuilderPool via `ctx.publish` (same path as `SchedulerUnreachable` at [`ephemeral.rs`](../../rio-controller/src/reconcilers/builderpool/ephemeral.rs)):

```rust
if failed_jobs.len() >= CRASH_LOOP_WARN_THRESHOLD {
    ctx.publish(wp, "Warning", "CrashLoopDetected", &format!(
        "{} Failed manifest Jobs (backoff_limit=0); check pod logs for \
         crash cause. Sweeping {} this tick.",
        failed_jobs.len(), failed_jobs.len().min(FAILED_SWEEP_PER_TICK),
    ));
}
```

`kubectl describe builderpool X` then shows the crash-loop in the event stream, not just as a growing `kubectl get jobs` list.

### T3 — `test(controller):` Failed-Job sweep + threshold event

Add to [`manifest_tests.rs`](../../rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs):

- `failed_jobs_swept_without_idle_check` — construct 5 Failed Jobs + 3 active; assert all 5 Failed are deletable without grace-window wait, the 3 active are untouched.
- `failed_sweep_bounded_per_tick` — construct 30 Failed Jobs; assert at most `FAILED_SWEEP_PER_TICK` selected. Verifies the burst cap.
- `failed_jobs_excluded_from_ceiling` — **regression guard for the existing bug**: construct `replicas.max=5`, 3 active, 10 Failed; assert spawn budget is still `5-3=2` (Failed don't count toward ceiling — this is correct behavior, the sweep handles them separately).

No `reconcile_manifest` integration test here — that's [P990719402](plan-990719402-manifest-reconcile-vm-test.md)'s VM-test scope.

## Exit criteria

- `reconcile_manifest` deletes up to `FAILED_SWEEP_PER_TICK` Failed Jobs per tick, no idle-grace wait applied
- Failed Jobs do NOT count toward `spec.replicas.max` ceiling (regression test locks this in)
- `CrashLoopDetected` Warning event emitted when `failed_jobs.len() >= 3`
- Code comment at `:925-929` no longer says "Followup needed" — points at the sweep site
- `grep 'ttl_seconds_after_finished' rio-controller/src/reconcilers/builderpool/manifest.rs` → still only in the explanatory comment, not set on the Job spec

## Tracey

Adds new marker to component specs:
- `r[ctrl.pool.manifest-failed-sweep]` → [`docs/src/components/controller.md`](../../docs/src/components/controller.md) (see § Spec additions). T1 is `r[impl]`; T3's `failed_jobs_swept_without_idle_check` is `r[verify]`.

References existing markers:
- `r[ctrl.pool.manifest-long-lived]` — T1 preserves the no-TTL invariant this marker describes (sweep is the replacement for TTL-based reaping)
- `r[ctrl.event.spec-degrade]` — T2's `CrashLoopDetected` event is the same operator-visibility class as the spec-degrade events

## Spec additions

Add to [`docs/src/components/controller.md`](../../docs/src/components/controller.md) after `r[ctrl.pool.manifest-scaledown]` (`:154`), before `r[ctrl.pool.manifest-long-lived]` — standalone paragraph, blank line before, col 0:

```
r[ctrl.pool.manifest-failed-sweep]
The manifest reconciler MUST delete Failed Jobs alongside idle-surplus deletes. With `backoff_limit=0` and no TTL (`r[ctrl.pool.manifest-long-lived]`), a crash-looping pod produces one Failed Job per reconcile tick; the ceiling (`spec.replicas.max`) does not cap this because Failed Jobs are not active supply. The sweep is bounded per-tick (default 20) to avoid a delete burst when an operator fixes a long-running crash-loop. A `CrashLoopDetected` Warning event is emitted when the Failed count crosses 3.
```

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/builderpool/manifest.rs", "action": "MODIFY", "note": "T1: failed_jobs slice + FAILED_SWEEP_PER_TICK delete chain after :207; T2: CrashLoopDetected event emit; :925-929 comment update"},
  {"path": "rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs", "action": "MODIFY", "note": "T3: 3 new tests — swept-without-idle-check, bounded-per-tick, excluded-from-ceiling"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "Spec addition: r[ctrl.pool.manifest-failed-sweep] after :154"}
]
```

```
rio-controller/src/reconcilers/builderpool/
├── manifest.rs          # T1+T2: failed_jobs sweep + event
└── tests/
    └── manifest_tests.rs  # T3: 3 tests
docs/src/components/
└── controller.md        # new marker
```

## Dependencies

```json deps
{"deps": [505], "soft_deps": [990719402, 990719403], "note": "P0505 owns select_deletable_jobs (T1 chains onto its delete loop). Soft-dep P990719402: the VM test SHOULD exercise the sweep once it exists. Soft-dep P990719403: job_common extraction moves is_active_job predicate — T1's failed-filter is the inverse of it; land this first (smaller diff) or rebase over the extracted helper."}
```

**Depends on:** [P0505](plan-0505-manifest-scaledown-grace.md) — `select_deletable_jobs` + delete loop are the insertion point for T1's sweep. DONE, no blocker.

**Conflicts with:** `manifest.rs` is not in top-20 collisions currently, but [P990719403](plan-990719403-job-common-extraction.md) touches `:199-207` (the `is_active_job` predicate T1 sits immediately after) and `:243-250`. Serialization: prefer this plan FIRST (T1 is a ~15-line insert; P990719403 is a ~50-line extraction that would churn the context). If P990719403 lands first, T1's `failed_jobs` filter moves into `job_common.rs` as `is_failed_job(j: &Job) -> bool` alongside `is_active_job`.

[P990719402](plan-990719402-manifest-reconcile-vm-test.md) adds `manifest_tests.rs` comment fix at `:7` — T3 adds tests at file-end; non-overlapping.

## Risks

- **Sweep races re-create:** if the sweep deletes Failed Job N at tick T, and the spawn pass at tick T also fires (deficit unchanged — Failed was already excluded from supply), we create Job N+1. That's CORRECT behavior (replacement for the crashed pod), but the `CrashLoopDetected` event should fire ONCE per threshold-cross, not every tick. T2's event will fire on every tick where count ≥ 3 — K8s deduplicates events by `(reason, message)` but the message includes the count, which changes. **Check at dispatch:** either interpolate a coarse bucket (`≥3`, `≥10`, `≥50`) into the message so K8s can dedupe, or track a "crashloop-warned" bit in `Ctx` alongside `manifest_idle`.
- **Bounded sweep vs unbounded accumulation:** `FAILED_SWEEP_PER_TICK=20` means a 8640-Job pile takes 432 ticks (~72 min at 10s) to clear after the crash stops. The sweep rate exceeds the accumulation rate (20 > 1) so it converges — but operators may want `kubectl delete jobs -l rio.build/sizing=manifest,status.failed=1` to clear immediately. Document this in the event message.
- **Priority hazard:** this is a prod-grade footgun (operator ships bad image → silent Job flood → etcd bloat → apiserver latency). Priority 85 is correct; land before any BuilderPool ships to a cluster with `sizing: Manifest`.
