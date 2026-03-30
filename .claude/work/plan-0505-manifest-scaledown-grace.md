# Plan 505: ADR-020 phase 5 — per-pod idle grace period + manifest scale-down

[ADR-020](../../docs/src/decisions/020-per-derivation-capacity-manifest.md) § Decision ¶4: manifest-spawned pods are long-lived, NOT one-shot. A pod heartbeats, takes any derivation that fits, idles until either work arrives or a grace period expires. The controller's manifest diff sees "48Gi pod idle, no 48Gi demand for `SCALE_DOWN_WINDOW`" → delete the Job.

This is the scale-down half of P0503's diff. P0503 spawns for deficit (`demand > supply`); this plan deletes for surplus (`supply > demand` sustained for the window). The grace period is the existing `SCALE_DOWN_WINDOW` (600s default, [`rio-controller/src/scaling/mod.rs`](../../rio-controller/src/scaling/mod.rs)) applied per-pod-bucket instead of per-class.

Key distinction from `ephemeral:true`: ephemeral pods exit after one build (`RIO_EPHEMERAL=1` → worker main loop exits). Manifest pods do NOT set that env var. They're STS-like in lifetime, Job-like in creation. `ttlSecondsAfterFinished` doesn't apply (pod doesn't finish on its own) — the controller explicitly deletes idle Jobs.

## Entry criteria

- [P0503](plan-0503-manifest-diff-reconciler.md) merged (`reconcile_manifest` owns the Job set; this plan extends its diff)

## Tasks

### T1 — `feat(controller):` track per-bucket idle-since timestamp

MODIFY [`rio-controller/src/reconcilers/builderpool/manifest.rs`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) (from P0503). Add a `BTreeMap<(u64, u32), Instant>` tracking when each bucket FIRST went surplus. Stored in the reconciler's state (same place stabilization timestamps live today — `ScalingTiming` or similar).

On each tick: if `supply > demand` for a bucket and no idle-since recorded → record now. If `supply <= demand` → clear the entry (demand returned, reset the clock). If idle-since is set AND `now - idle_since > SCALE_DOWN_WINDOW` → bucket is eligible for scale-down.

### T2 — `feat(controller):` delete surplus Jobs after grace window

Extend the diff loop: for each bucket where idle-grace has elapsed, compute `surplus = supply - demand`, delete `surplus` Jobs from that bucket. Deletion order: prefer Jobs whose pods are currently idle (no active build — check via the pod's status or the scheduler's worker list). Don't delete a Job mid-build.

Idle-check: list workers from `ClusterStatus`, match by `executor_id` → Job pod, check `running_builds == 0`. A Job whose pod has `running_builds > 0` is NOT deleted even if the bucket is surplus — wait for the build to finish.

### T3 — `feat(controller):` manifest pods don't set RIO_EPHEMERAL

MODIFY the pod-spec override in `reconcile_manifest` (from P0503 T2). Ensure `RIO_EPHEMERAL` env var is NOT set (or set to `"0"`). P0503 might have inherited ephemeral's env setup via `build_pod_spec` — verify and strip. Manifest pods are long-lived: the worker loop does NOT exit after one build.

Also: `ttlSecondsAfterFinished` should NOT be set on manifest Jobs (pod never "finishes"). Controller deletion is the ONLY scale-down path.

### T4 — `test(controller):` grace window + idle-check

Unit tests with mocked clock (tokio `start_paused`):
- Bucket surplus for 599s → no delete
- Bucket surplus for 601s → delete `surplus` Jobs
- Bucket surplus, then demand returns at 300s → clock resets; surplus again → must wait full 600s from the new surplus-start
- Surplus bucket has 3 Jobs, 1 is mid-build (`running_builds=1`) → delete at most 2

## Exit criteria

- Per-bucket idle timestamp resets when demand returns (test)
- No deletion before `SCALE_DOWN_WINDOW` elapses (test)
- Jobs with `running_builds > 0` are skipped for deletion (test)
- Manifest pods do NOT have `RIO_EPHEMERAL=1` env var
- Manifest Jobs do NOT have `ttlSecondsAfterFinished` set

## Tracey

- `r[ctrl.pool.manifest-scaledown]` — NEW marker. T1+T2 are `r[impl]`; T4 is `r[verify]`.
- `r[ctrl.pool.manifest-long-lived]` — NEW marker. T3 is `r[impl]`; T4's env-var assertion is `r[verify]`.

## Spec additions

Add to `docs/src/components/controller.md` after `r[ctrl.pool.manifest-reconcile]` (from P0503):

```
r[ctrl.pool.manifest-scaledown]

Manifest-mode scale-down is per-bucket: when `supply > demand` for a `(memory-class, cpu-class)` bucket for `SCALE_DOWN_WINDOW` (600s default), the controller deletes `surplus` Jobs from that bucket. Deletion skips Jobs whose pods are mid-build (`running_builds > 0` from `ClusterStatus`). Demand returning before the window elapses resets the clock.

r[ctrl.pool.manifest-long-lived]

Manifest-spawned pods do NOT set `RIO_EPHEMERAL=1`. The worker's main loop does not exit after one build — it heartbeats, accepts any derivation that fits its `memory_total_bytes`, and idles. `ttlSecondsAfterFinished` is not set on the Job (the pod never self-terminates). Scale-down is entirely controller-driven.
```

## Files

```json files
["rio-controller/src/reconcilers/builderpool/manifest.rs", "rio-controller/src/scaling/mod.rs", "docs/src/components/controller.md"]
```

## Dependencies

```json deps
[503]
```

- P0503: `reconcile_manifest` owns the Job set. T1's per-bucket state and T2's deletion loop extend P0503's diff.

## Risks

- Deleting a Job deletes its pod. If the idle-check is wrong (pod IS mid-build but we think it's idle), we orphan a build. The `running_builds` check must use the SAME `ClusterStatus` snapshot that the demand count came from — don't fetch twice.
- Race: pod reports idle, controller initiates delete, scheduler dispatches to that pod in the same tick. Mitigate: before delete, check if the pod appeared in `ClusterStatus.dispatched_recently` (if that exists) or accept the race — scheduler retries dispatch on the next worker.
- `SCALE_DOWN_WINDOW` is currently a const or STS-level config. Verify it's accessible from the manifest reconciler; may need to plumb.
