# Plan 0107: Health probes — tonic-health + worker /healthz/readyz

## Design

K8s needs liveness/readiness probes for every pod. The store/scheduler/gateway already had gRPC ports; the worker is a gRPC **client** with no server. Two commits gave each component a probe surface matching its architecture.

`dd6eaca` added `tonic-health` to store, scheduler, and gateway. Store and scheduler serve `grpc.health.v1.Health` on their existing gRPC port — `HealthReporter::set_serving::<S>()` after startup completes. Gateway has an SSH listener, not a gRPC server, so it spawns a **separate** tonic server on `health_addr` solely for health. Critical gotcha (documented in the commit body, load-bearing for P0117's kustomize manifests): `set_not_serving::<S>()` only affects the **named** service, not the empty-string service. K8s `readinessProbe` MUST specify `grpc.service: rio.scheduler.SchedulerService` — if it probes `""`, it stays `SERVING` forever and standby schedulers get routed traffic.

`2c6326b` gave the worker plain HTTP via axum on `health_addr` (default `0.0.0.0:9193` — metrics port 9093 + 100, same +100 pattern as gateway). `/healthz` (liveness): always 200. Reaching the handler proves the process is alive and tokio is responsive. No gating — a worker with a broken FUSE mount is still alive (restarting might fix it); a worker that can't reach the scheduler is still alive (restarting doesn't fix the network). Liveness = minimum viable signal. `/readyz` (readiness): 200 if heartbeat accepted, 503 otherwise. `Arc<AtomicBool>` written by heartbeat loop, read by handler. Starts `false`: NOT READY until first accepted heartbeat. StatefulSet rollout waits for this before moving to next pod. `Relaxed` ordering — pure signal, no synchronization-with semantics. 503 not 500: the worker is fine, its **dependency** (scheduler) is unavailable. Spawned **before** gRPC connect so liveness passes as soon as process is up (connect may take seconds on slow DNS). `spawn_monitored`: if the health server dies, liveness fails → pod restart → self-healing. `router()` separate from `spawn` for testability — tests use `Router::oneshot` without a real TCP listener.

## Files

```json files
[
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "HealthReporter wired; set_serving after actor spawn"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "HealthReporter wired; set_serving after PG connect"},
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "separate tonic server on health_addr (SSH has no gRPC port)"},
  {"path": "rio-worker/src/health.rs", "action": "NEW", "note": "axum router: /healthz always 200, /readyz tracks Arc<AtomicBool> from heartbeat"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "spawn health server BEFORE gRPC connect; spawn_monitored wrapper"},
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "heartbeat loop writes ready flag: true on accepted, false on rejected or gRPC error"},
  {"path": "rio-worker/Cargo.toml", "action": "MODIFY", "note": "axum dep; tower dev-dep (util feature for ServiceExt::oneshot)"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `r[sched.health.*]`, `r[worker.health.liveness]`, `r[worker.health.readiness]`. P0127 later removed a false `r[ctrl.probe.named-service]` annotation (it was a kustomize-manifest concern, not code).

## Entry

- Depends on P0106: the readiness flag wraps heartbeat acceptance, which P0106's admin surface depends on being accurate.

## Exit

Merged as `dd6eaca` + `2c6326b` (2 commits). `.#ci` green at merge. Worker tests: `healthz_always_ok` (liveness unconditional, ready flag irrelevant), `readyz_tracks_flag` (false→503, true→200, false→503 again). Known gotcha hit during development: new `health.rs` needed `git add` before `nix build` (flake source filter is git-tracked only).
