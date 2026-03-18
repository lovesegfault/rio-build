# Plan 0190: Rem-11 — tonic-health drain: set_not_serving before serve_with_shutdown returns

## Design

**P1 (HIGH).** All three long-lived servers pass `shutdown.cancelled_owned()` directly to `serve_with_shutdown`. On SIGTERM: (1) `shutdown_signal()` fires the token; (2) `serve_with_shutdown` sees cancelled → stops accepting, drains in-flight, returns; (3) `main()` exits. Between (1) and (3) (~50-500ms drain), tonic-health is still reporting SERVING. kubelet probes at `periodSeconds: 5` — ~5s window where the last probe was SERVING, pod stays in Endpoint slice. kube-proxy / NLB keeps routing NEW connections to a process that is tearing down. Client opens fresh gRPC stream → TCP accept succeeds (listener still bound during drain) → RST when process exits → `Unavailable: transport error` with zero diagnostic context.

**Two-stage shutdown:** SIGTERM fires the parent token (stops background loops), drain task flips health to NOT_SERVING, sleeps `drain_grace_secs` (default 6 = `periodSeconds+1`), THEN cancels the serve token. Gives kubelet/BalancedChannel one full probe cycle.

**Architecture note:** `serve_shutdown` is an INDEPENDENT `CancellationToken`, NOT `parent.child_token()`. `child_token` cascades synchronously — `parent.cancel()` (which is exactly what `shutdown_signal` does) would set `child.is_cancelled()=true` instantly, zero drain window. Test `drain_sets_not_serving_before_child_cancel` proves the independent-token pattern; it caught the cascade bug in the original plan.

**Per-component nuance:** gateway uses `set_service_status("", NotServing)` — its probe has no `grpc.service` field so `set_not_serving<S>()` would miss it. Scheduler's `tcpSocket` probe passes as long as listener bound — only `BalancedChannel` clients (3s probe loop) see the flip.

Also: `ActorError::ChannelSend` → `Status::unavailable` (was `internal`); `TriggerGC` forward task biased select! on shutdown.

Remediation doc: `docs/src/remediations/phase4a/11-tonic-health-drain.md` (694 lines).

## Files

```json files
[
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "two-stage: parent cancel → NOT_SERVING → sleep drain_grace → serve_shutdown cancel"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "two-stage shutdown; plaintext-health waits on serve_shutdown too"},
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "two-stage; set_service_status(\"\", NotServing) for empty-service probe"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "ActorError::ChannelSend → Status::unavailable"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "TriggerGC biased select! on shutdown"},
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "MODIFY", "note": "drain_sets_not_serving_before_child_cancel — proves independent-token pattern"},
  {"path": "rio-scheduler/src/admin/tests.rs", "action": "MODIFY", "note": "drain verify"},
  {"path": "infra/helm/rio-build/templates/scheduler.yaml", "action": "MODIFY", "note": "drain_grace_secs helm knob"},
  {"path": "infra/helm/rio-build/templates/store.yaml", "action": "MODIFY", "note": "drain_grace_secs"},
  {"path": "infra/helm/rio-build/templates/gateway.yaml", "action": "MODIFY", "note": "drain_grace_secs"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "r[common.drain.not-serving-before-exit] spec marker"}
]
```

## Tracey

- `r[impl common.drain.not-serving-before-exit]` ×3 — `a6a5a77` (scheduler, store, gateway)
- `r[verify common.drain.not-serving-before-exit]` — `a6a5a77`

4 marker annotations.

## Entry

- Depends on P0174: scheduler token-aware shutdown (the serve loop this wraps)

## Exit

Merged as `99f2864` (plan doc) + `a6a5a77` (fix). `.#ci` green. Rolling restart: no `transport error` on fresh connections during pod exit.
