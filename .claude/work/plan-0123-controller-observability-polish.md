# Plan 0123: Controller observability + post-implementation cleanup

## Design

Eleven commits of polish after the bug-fix waves: controller metrics/validation, dead-CRD-feature revival, cleanup of footguns left behind by earlier work.

`b2cfb4e` revived two previously-dead CRD features. `Autoscaling.metric` validation: `scale_one()` reads `spec.autoscaling.metric`; only `"queueDepth"` is supported. Unknown values (operator setting `"cpuUtilization"`) were silently ignored — autoscaler scaled on queueDepth anyway. Now: warn log + `ScaleError` return + condition surface. `WorkerPool.status.{lastScaleTime,conditions}`: previously written as `{None, vec![]}` on EVERY reconcile, so any value the autoscaler set was clobbered. Now reconciler patches ONLY `replicas/ready/desired` (partial `json!`, omits other fields → SSA preserves); autoscaler patches ONLY `lastScaleTime + conditions` via **separate** field-manager `rio-controller-autoscaler-status`. On scale: `Scaling=True, reason=ScaledUp/Down`. On unknown metric: `Scaling=False, reason=UnknownMetric`.

`efe6af4` added `reconcile_duration` + `reconcile_errors` histograms + `#[instrument]` spans on both reconcilers. `51b9bc0` added env overrides for autoscaler timing (poll interval, stabilization windows) — lets vm-phase3a exercise autoscaling in reasonable test time. `3e32890` added `estimator_refresh` + `lease_acquired` metrics to scheduler.

Cleanup: `fbf9c17` removed `with_chunk_backend` entirely — it was a footgun (created its own cache, defeating sharing; P0110 noted this). Only `with_chunk_cache` remains. `bac57ce` removed unreachable `Error::Serde` variant (crdgen is the only serde-yaml caller, and it panics on error). `1cb91e0` + `8fccc04` stripped stale F-number plan references from comments (legacy planning scheme). `e19080c` tagged the worker's Cancel handler stub with `TODO(phase3b)`. `8cb39af` switched `serde_yaml` → `serde_yml` fork for crdgen (upstream unmaintained). `4f1ed11` synced `controller.md` + `worker.md` with the impl.

## Files

```json files
[
  {"path": "rio-controller/src/scaling.rs", "action": "MODIFY", "note": "metric validation: unknown → ScaleError + condition; lastScaleTime + conditions via separate field-manager"},
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "MODIFY", "note": "status patch ONLY replicas/ready/desired (partial json!, SSA preserves autoscaler fields)"},
  {"path": "rio-controller/src/reconcilers/build.rs", "action": "MODIFY", "note": "#[instrument] span; reconcile_duration histogram"},
  {"path": "rio-controller/src/error.rs", "action": "MODIFY", "note": "reconcile_errors counter in Display impl; Serde variant removed"},
  {"path": "rio-scheduler/src/lease.rs", "action": "MODIFY", "note": "lease_acquired counter"},
  {"path": "rio-scheduler/src/estimator.rs", "action": "MODIFY", "note": "estimator_refresh counter"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "with_chunk_backend removed (footgun); with_chunk_cache only"},
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "Cancel handler stub tagged TODO(phase3b)"},
  {"path": "rio-controller/Cargo.toml", "action": "MODIFY", "note": "serde_yaml → serde_yml fork"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "synced with phase3a impl"},
  {"path": "docs/src/components/worker.md", "action": "MODIFY", "note": "synced with phase3a impl"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `r[ctrl.autoscale.metric-validate]`, `r[ctrl.status.conditions]`, `r[obs.metric.reconcile-duration]`.

## Entry

- Depends on P0121: the field-manager separation is wave-3 of the SSA conflict discipline established in P0121.
- Depends on P0104: `describe!()` pattern established there; controller metrics use it.

## Exit

Merged as `b2cfb4e..8fccc04` (11 commits, non-contiguous). `.#ci` green at merge. `rg 'F-\d+'` → 0 hits (stale plan references gone). `rg 'with_chunk_backend'` → 0 hits.
