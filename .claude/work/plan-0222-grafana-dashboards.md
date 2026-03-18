# Plan 0222: Grafana dashboard JSONs

phase4c.md:53 — four dashboards exported as raw Grafana JSON under a NEW directory `infra/helm/grafana/`. **Per Q2 default: raw JSONs + minimal ConfigMap wrapper** — the milestone says "render" not "auto-deploy." Operators can `kubectl apply` the ConfigMap or import the JSONs directly into Grafana.

**Metric-name discipline:** every PromQL query MUST reference a metric that is actually registered. Cross-reference each `expr` against metric registrations in [`rio-scheduler/src/lib.rs`](../../rio-scheduler/src/lib.rs), [`rio-store/src/lib.rs`](../../rio-store/src/lib.rs), [`rio-gateway/src/lib.rs`](../../rio-gateway/src/lib.rs), [`rio-worker/src/lib.rs`](../../rio-worker/src/lib.rs), and [`observability.md`](../../docs/src/observability.md). DO NOT invent metric names.

**Known no-data panel:** `rio_scheduler_class_load_fraction` doesn't exist until [P0229](plan-0229-cutoff-rebalancer-gauge-convergence.md) merges. The panel shows "No data" until then — which is FINE. The milestone is "dashboards render," not "all panels populated."

## Tasks

### T1 — `feat(infra):` Build Overview dashboard

NEW `infra/helm/grafana/build-overview.json`. Panels:

| Panel | PromQL | Type |
|---|---|---|
| Active builds | `rio_scheduler_builds_active` | stat |
| Completion rate | `rate(rio_scheduler_derivations_completed_total[5m])` | time series |
| Build duration p50/p99 | `histogram_quantile(0.5, rate(rio_scheduler_build_duration_seconds_bucket[5m]))` and `0.99` | time series |
| Builds by status | `sum by (status) (rio_scheduler_builds_by_status)` | pie or bar gauge |

Verify each metric exists: `grep '<metric_name>' rio-scheduler/src/lib.rs docs/src/observability.md`.

### T2 — `feat(infra):` Worker Utilization dashboard

NEW `infra/helm/grafana/worker-utilization.json`. Panels:

| Panel | PromQL | Type |
|---|---|---|
| Replicas per class | `rio_controller_workerpool_replicas{class=~".+"}` | time series, per-class |
| Worker CPU | `rate(container_cpu_usage_seconds_total{pod=~"rio-worker.*"}[5m])` | time series |
| Worker memory | `container_memory_working_set_bytes{pod=~"rio-worker.*"}` | time series |
| Queue depth per class | `rio_scheduler_ready_queue_depth{class=~".+"}` | time series |
| Class load fraction | `rio_scheduler_class_load_fraction` | gauge (NO DATA until P0229) |

Add a panel description noting "populated after P0229 cutoff rebalancer" on the load-fraction panel.

### T3 — `feat(infra):` Store Health dashboard

NEW `infra/helm/grafana/store-health.json`. Panels:

| Panel | PromQL | Type |
|---|---|---|
| Cache hit ratio | `rio_store_cache_hits_total / (rio_store_cache_hits_total + rio_store_cache_misses_total)` | stat |
| GC sweep duration | `rio_store_gc_sweep_duration_seconds` | time series |
| S3 request latency | `histogram_quantile(0.99, rate(rio_store_s3_request_duration_seconds_bucket[5m]))` | time series |
| Chunk store size | `rio_store_chunks_total_bytes` | stat |

### T4 — `feat(infra):` Scheduler dashboard

NEW `infra/helm/grafana/scheduler.json`. Panels:

| Panel | PromQL | Type |
|---|---|---|
| Dispatch latency | `histogram_quantile(0.99, rate(rio_scheduler_dispatch_latency_seconds_bucket[5m]))` | time series |
| Ready queue depth | `rio_scheduler_ready_queue_depth` | time series |
| Backstop timeouts | `rate(rio_scheduler_backstop_timeouts_total[5m])` | time series |
| Cancel signals | `rate(rio_scheduler_cancel_signals_total[5m])` | time series |
| Misclassifications (penalty) | `rate(rio_scheduler_misclassifications_total[5m])` | time series |
| Class drift (cutoff) | `rate(rio_scheduler_class_drift_total[5m])` | time series (NO DATA until P0228) |

### T5 — `feat(infra):` ConfigMap wrapper

NEW `infra/helm/grafana/configmap.yaml` — minimal Grafana sidecar-compatible ConfigMap that wraps all four JSONs:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: rio-grafana-dashboards
  labels:
    grafana_dashboard: "1"  # sidecar discovery label
data:
  build-overview.json: |
    {{ .Files.Get "grafana/build-overview.json" | indent 4 }}
  # ... etc for all 4
```

Or, since this is NOT a helm template (no `.Files.Get` context), ship as a plain kubectl-applyable ConfigMap with the JSONs inlined. Decide at implement based on whether `infra/helm/grafana/` will be a chart subdir or a standalone dir.

## Exit criteria

- `/nbr .#ci` green
- `jq . infra/helm/grafana/*.json` exits 0 for all four (JSON well-formed)
- Each `expr` field in each JSON references a metric that grep finds in `rio-*/src/lib.rs` OR `observability.md` OR is documented as "pending P022X" in the panel description
- ConfigMap wrapper parses: `kubectl apply --dry-run=client -f infra/helm/grafana/configmap.yaml`

## Tracey

No markers — dashboards are operational tooling, not normative spec.

## Files

```json files
[
  {"path": "infra/helm/grafana/build-overview.json", "action": "NEW", "note": "T1: active builds, completion rate, duration p50/p99"},
  {"path": "infra/helm/grafana/worker-utilization.json", "action": "NEW", "note": "T2: per-class replicas, CPU/mem, queue depth, load fraction (no-data until P0229)"},
  {"path": "infra/helm/grafana/store-health.json", "action": "NEW", "note": "T3: cache hit, GC sweep, S3 latency, chunk size"},
  {"path": "infra/helm/grafana/scheduler.json", "action": "NEW", "note": "T4: dispatch latency, ready-queue, backstop timeouts, cancel signals, class drift (no-data until P0228)"},
  {"path": "infra/helm/grafana/configmap.yaml", "action": "NEW", "note": "T5: sidecar-discoverable ConfigMap wrapper"}
]
```

```
infra/helm/grafana/           # NEW directory
├── build-overview.json
├── worker-utilization.json
├── store-health.json
├── scheduler.json
└── configmap.yaml
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [228, 229], "note": "Zero conflict — new directory. Soft deps on P0228/P0229 only in the sense that two panels show no-data until they merge; NOT a dag dep."}
```

**Depends on:** none. All metrics referenced either exist today or are documented as pending-P022X.
**Conflicts with:** none — `infra/helm/grafana/` does not exist (verified); zero collision.
