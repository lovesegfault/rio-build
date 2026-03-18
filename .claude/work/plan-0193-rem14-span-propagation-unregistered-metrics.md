# Plan 0193: Rem-14 — Span propagation in spawn_monitored + 2 unregistered metrics + ground-truth gauge

## Design

**P1 (HIGH).** Observability grab-bag: one span-propagation bug and two unregistered-metric findings.

**`spawn_monitored` span propagation:** `rio_common::task::spawn_monitored` wraps `tokio::spawn` with panic-catch + metric. It did NOT propagate the current span — spawned tasks got a fresh root. Any `info!` inside a monitored task lost the `component` field. Fix: capture `Span::current()` and `.instrument()` the spawned future. New test: `BufWriter` `MakeWriter`, span with `component`, call `spawn_monitored`, have the spawned task `info!("hello")`, assert the JSON has `"component"`.

**2 unregistered metrics:** controller's `rio_controller_reconcile_errors_total` and worker's `rio_worker_builds_active` were emitted but never `describe_counter!`/`describe_gauge!`-registered. Prometheus scrape showed them but with no HELP text; monitoring dashboards relying on metadata couldn't categorize. Fix: hoist `describe_*` into each crate's `lib.rs` alongside the others.

**Ground-truth gauge test pattern:** `rio-*/tests/metrics_registered.rs` × 5 components. Each test scrapes `/metrics` and asserts every documented metric name appears — regression guard for the unregistered-metric class.

Remediation doc: `docs/src/remediations/phase4a/14-span-propagation-observability.md` (638 lines).

## Files

```json files
[
  {"path": "rio-common/src/task.rs", "action": "MODIFY", "note": "spawn_monitored: capture Span::current(), .instrument() the spawned future"},
  {"path": "rio-controller/src/lib.rs", "action": "MODIFY", "note": "describe_counter! for reconcile_errors_total"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "metric registration moved to lib.rs"},
  {"path": "rio-worker/src/lib.rs", "action": "MODIFY", "note": "describe_gauge! for builds_active"},
  {"path": "rio-controller/tests/metrics_registered.rs", "action": "NEW", "note": "scrape /metrics, assert all documented names appear"},
  {"path": "rio-gateway/tests/metrics_registered.rs", "action": "NEW", "note": "scrape + assert"},
  {"path": "rio-scheduler/tests/metrics_registered.rs", "action": "NEW", "note": "scrape + assert"},
  {"path": "rio-store/tests/metrics_registered.rs", "action": "NEW", "note": "scrape + assert"},
  {"path": "rio-worker/tests/metrics_registered.rs", "action": "NEW", "note": "scrape + assert"},
  {"path": "rio-scheduler/src/actor/recovery.rs", "action": "MODIFY", "note": "span context fix in spawned recovery task"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "span context fix"},
  {"path": "rio-gateway/Cargo.toml", "action": "MODIFY", "note": "tracing-test dev-dep"}
]
```

## Tracey

- `r[verify obs.log.required-fields]` — `3ed0e99` (spawn_monitored span test)
- `r[impl obs.metric.controller]` — `3ed0e99` (describe_* hoisted into lib.rs)
- `r[verify obs.metric.controller]` — `3ed0e99`
- `r[verify obs.metric.gateway]` — `3ed0e99`
- `r[verify obs.metric.scheduler]` — `3ed0e99`
- `r[verify obs.metric.store]` — `3ed0e99`
- `r[verify obs.metric.worker]` — `3ed0e99`

7 marker annotations.

## Entry

- Depends on P0152: transfer-volume metrics (these tests assert those are registered too)

## Exit

Merged as `62ade73` (plan doc) + `3ed0e99` (fix). `.#ci` green. `/metrics` scrape for all 5 components shows documented metric names with HELP text.
