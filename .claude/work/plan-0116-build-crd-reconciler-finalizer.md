# Plan 0116: Build CRD reconciler + WorkerPool finalizer drain

## Design

Three commits completed the controller's CRD surface: the Build reconciler (K8s-native build submission via `kubectl apply -f build.yaml`) and the WorkerPool finalizer (clean shutdown drains workers before delete).

`64455fb` built the Build CRD reconciler (683 LOC). `kubectl apply -f build.yaml` → `SubmitBuild` → watch task drains `BuildEvent` stream → `.status` patches. Finalizer cleanup → `CancelBuild`. Single-node DAG (phase3a scope): fetch `.drv` from `rio-store` via `GetPath`, parse ATerm (`rio-nix`), construct one `DerivationNode`. Works when the full closure is already in `rio-store` — the CRD docs **require** "upload via `nix copy --to ssh-ng://` first." Zero edges = leaf → scheduler dispatches immediately → worker resolves inputDrv outputs from store. TODO(phase4): full DAG reconstruction for inputDrvs not yet built.

Idempotence via `"submitted"` sentinel in `status.build_id`: finalizer-add triggers re-reconcile **before** first `BuildEvent` arrives; without the sentinel, `apply()` would double-submit. Watch task overwrites with real UUID on first event. `Event::Log` filtered from status patches (same "~20/sec, too chatty" as P0115). `DerivationEvent` also skipped — per-drv detail; `Progress` carries the aggregate. Best-effort cleanup: scheduler down → log + proceed. Blocking delete on an outage means `kubectl delete` stalls until scheduler returns — operationally annoying.

`84f2925` added the WorkerPool finalizer: on delete, `drain_worker(force=false)` for each worker in the pool, wait for running builds to drain, then let the StatefulSet delete proceed. Uses `force=false` by default — the builds genuinely finish. Operator can `kubectl annotate workerpool/foo rio.dev/force-drain=true` to skip the wait.

`96af31e` added `tower-test` `ApiServerVerifier` mocks — unit tests for the reconciler without a real apiserver. Mock intercepts K8s API calls, asserts request shape (SSA patch body, finalizer ops), returns canned responses. Tests: reconcile creates STS with correct spec; status patch includes readyReplicas; finalizer-add before drain_stream spawn (see P0121 for the race fix).

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/build.rs", "action": "NEW", "note": "683 LOC: SubmitBuild, drain_stream watch task, status patches, 'submitted' sentinel, finalizer CancelBuild"},
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "MODIFY", "note": "finalizer: drain_worker per-worker, wait, let STS delete proceed"},
  {"path": "rio-controller/src/reconcilers/mod.rs", "action": "MODIFY", "note": "build reconciler registered in Controller::run"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "spawn build reconciler loop"},
  {"path": "rio-controller/src/fixtures.rs", "action": "NEW", "note": "tower-test ApiServerVerifier mock setup"},
  {"path": "rio-controller/Cargo.toml", "action": "MODIFY", "note": "tower-test dev-dep; rio-nix dep for ATerm parse"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `r[ctrl.build.reconcile]`, `r[ctrl.build.sentinel]`, `r[ctrl.build.finalizer-cancel]`, `r[ctrl.workerpool.finalizer-drain]`. P0127 later removed a false `r[verify ctrl.build.sentinel]` — tests don't exercise the double-reconcile race.

## Entry

- Depends on P0112: crate skeleton + WorkerPool reconciler exist.
- Depends on P0115: `WatchBuild` uses the event stream with replay.
- Depends on P0106: finalizer calls `DrainWorker`.

## Exit

Merged as `64455fb..96af31e` (3 commits). `.#ci` green at merge. tower-test mocks cover reconcile happy path + finalizer path. Tested end-to-end by P0125's vm-phase3a Build CRD exercises.
