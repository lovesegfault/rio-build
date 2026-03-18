# Plan 0125: vm-phase3a full exercises — Build CRD/autoscaler/lease + flake fixes

## Design

The P0118 VM test proved "worker builds in a pod." Six commits expanded it to exercise the full phase3a surface: Build CRD end-to-end, autoscaler scale-up/down, leader-election Lease acquire/release. `phase3a.nix` grew from 437 to 956 LOC.

`c43eb16` added the scheduler `lease` NixOS module option (`name`/`namespace`/`kubeconfigPath`; `holder = %H` systemd hostname) and the three exercise blocks. **Build CRD:** `kubectl apply -f build.yaml` → wait for `status.phase=Completed` → verify output in store. Exercises P0116's reconciler + `"submitted"` sentinel. **Autoscaler:** submit 5 builds with `min=1`, assert STS scales to ≥2 (P0123's env overrides set stabilization windows to 2s/5s for test speed). Then cancel, assert scale-down to 1. **Lease:** start two scheduler instances, verify only one has `is_leader=true`, kill leader, verify failover within 15s TTL.

Follow-up fixes as the exercises found more bugs: `e68c41e` fixed lease `HOSTNAME` env (systemd `%H` expansion wasn't available in the heredoc kubeconfig path; switched to f-string). `8287ea4` changed queue-drain wait from shell `wait` to polling `psql SELECT` — `wait` doesn't work when the build is running inside the pod, not as a shell child. `145450e` added `cpu_cores` assertion (P0109's cpu tracking) + fixed a race where replicas check ran before STS settled. `1693c63` fixed a phase2c VM test regression (estimator refresh baseline had to be captured BEFORE the pre-seed INSERT). `b3a994f` fixed the autoscaler to skip deletion-pending pools (found by the finalizer exercise — autoscaler raced the delete).

## Files

```json files
[
  {"path": "nix/tests/phase3a.nix", "action": "MODIFY", "note": "+360 LOC: Build CRD exercise, autoscaler scale-up/down (2s/5s windows), lease failover"},
  {"path": "nix/modules/scheduler.nix", "action": "MODIFY", "note": "lease option: name/namespace/kubeconfigPath; RIO_LEASE_* env; KUBECONFIG for out-of-cluster"},
  {"path": "nix/tests/phase2c.nix", "action": "MODIFY", "note": "estimator refresh baseline captured BEFORE pre-seed INSERT"},
  {"path": "rio-controller/src/scaling.rs", "action": "MODIFY", "note": "skip pools with deletionTimestamp (found by finalizer exercise)"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `.nix` files not parsed by tracey. The VM exercises verify spec rules that have no unit-test coverage (`r[ctrl.build.reconcile]`, `r[sched.lease.*]`, `r[ctrl.autoscale.*]`) — `.sh` shim `r[verify]` annotations added in P0126.

## Entry

- Depends on P0124: the split `admin/tests.rs` got the new command variants these exercises dispatch.
- Depends on P0121: the replica-preservation fix is exercised here.
- Depends on P0114, P0116, P0123: lease, Build CRD, autoscaler-timing-overrides — all exercised here.

## Exit

Merged as `c43eb16..b3a994f` (6 commits, non-contiguous). `.#ci` green at merge. vm-phase3a runs all three exercise blocks; total test time ~12 min (up from ~6 min at P0118). phase3a marked COMPLETE in `docs/src/phases/phase3a.md`: 813 → 909 tests.
