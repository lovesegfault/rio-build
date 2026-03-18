# Plan 0165: Scheduler rollout deadlock — Recreate strategy + step_down on SIGTERM

## Design

Found during EKS deployment: `just eks deploy` → new scheduler pods stay Running-but-not-Ready forever, logging `lease is held by rio-scheduler-XXX and has not yet expired`.

**Root cause:** `readinessProbe` gates on `is_leader` (by design — it's how the Service routes only to the active replica). That means at most 1 pod is EVER Ready. `RollingUpdate` requires `(replicas - maxUnavailable)` Ready at all times; with `replicas=2 maxUnavailable=1` that's 1 minimum. So k8s won't terminate the old leader (the only Ready pod) until a new pod is Ready — but new pods can't be Ready until they hold the lease, which the old leader holds. **Deadlock.** (Earlier commit `6250327` set `maxUnavailable: 1` thinking it fixed this — it only lets k8s kill the old standby, which was already not-Ready.)

**Fix part 1:** `strategy.type: Recreate`. Terminate all old pods first, then create new. The standby is for UNPLANNED failover, not rolling updates — a few seconds of scheduler-unavailable during a deliberate deploy is fine.

**Fix part 2:** `step_down()` on SIGTERM. Without this, the old leader holds the lease until TTL expiry (15s) after termination — the new pods wait an extra TTL cycle. With step_down, the lease is released gracefully on shutdown and new pods acquire immediately. New spec marker `r[sched.lease.graceful-release]`.

`7b731ae` refined: deploy.sh waits for `readyReplicas=1` (specific condition), not the generic `Available` which can flip true before the lease-holder is ready.

## Files

```json files
[
  {"path": "rio-scheduler/src/lease.rs", "action": "MODIFY", "note": "step_down() on SIGTERM \u2014 release lease before exit"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "wire step_down into shutdown path"},
  {"path": "infra/base/scheduler.yaml", "action": "MODIFY", "note": "strategy.type: Recreate (not RollingUpdate)"},
  {"path": "infra/eks/deploy.sh", "action": "MODIFY", "note": "wait for readyReplicas=1 not Available"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "r[sched.lease.graceful-release] spec marker"}
]
```

## Tracey

- `r[impl sched.lease.graceful-release]` — `46e4fe2`
- `r[verify sched.lease.graceful-release]` — added later in `baa0d1f` (VM graceful-release subtest) + col-0 move in `647bb90`

1 impl marker in this cluster; verify lands in P0179.

## Entry

- Depends on P0148: phase 3b complete
- Depends on P0114: phase 3a lease implementation (extends it with step_down)

## Exit

Merged as `46e4fe2`, `7b731ae` (2 commits). `just eks deploy` rollout completes in seconds instead of deadlocking.
