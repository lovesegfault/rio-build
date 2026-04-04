# Plan 0539: Observability stack — Prometheus, dashboards, missing metrics, xtask tooling

## Design

Stress-testing session I-128→I-148 was debugged via log-grep and ad-hoc `cli` polls. Every component emits `rio_*` metrics on `:9090` but **no Prometheus scrapes them**. The I-139/I-140 per-phase histograms are invisible. Dispatch latency, mailbox depth, and per-class queue breakdown require manual log correlation.

This plan deploys kube-prometheus-stack, wires ServiceMonitors, adds the 6 metrics that would have caught I-139/140/142/144/145/147 directly, builds 6 Grafana dashboards, and fixes the xtask tooling friction (stale SSM tunnels, NLB target-health, metrics scrape).

## Partition (5 independent worktrees)

| Part | Scope | Crates/dirs | Collisions |
|---|---|---|---|
| **A** | kube-prometheus-stack + ServiceMonitors + PrometheusRule alerts | `infra/eks/monitoring.tf`, `infra/helm/rio-build/templates/{servicemonitor,prometheusrule}.yaml`, `values.yaml` | none (new files + values block) |
| **B** | Grafana dashboards (6× JSON) | `infra/helm/rio-build/dashboards/*.json` + ConfigMap template | none (new dir) |
| **C** | Scheduler metrics: mailbox_depth, dispatch_wait_seconds, broadcast_lagged_total | `rio-scheduler/src/` | none |
| **D** | Store + controller metrics: putpath_retries_total, gc_sweep_paths_remaining, reconcile_duration_seconds | `rio-store/src/`, `rio-controller/src/` | I-148 (rio-store/migrations) — different files |
| **E** | xtask: `metrics` subcommand, rsb SSM cleanup, deploy NLB wait | `xtask/src/` | none |

Parts A-E are file-disjoint. Merge order: any; A enables B's dashboards to render but B's JSON is valid standalone.

## Exit criteria

- [ ] `kubectl get servicemonitor -n rio-system` returns 5 (gateway/scheduler/controller/store/builder)
- [ ] `kubectl get prometheusrule -n rio-system rio-alerts` exists with 4+ rules
- [ ] Grafana at `kubectl port-forward svc/kube-prometheus-stack-grafana` shows 6 rio-* dashboards
- [ ] `curl scheduler:9090/metrics | grep rio_scheduler_actor_mailbox_depth` returns a gauge
- [ ] `nix develop -c cargo xtask k8s -p eks metrics` prints scheduler/store key gauges in <5s
- [ ] `xtask rsb` kills stale SSM tunnels on port 2222 before establishing new
- [ ] `.#ci` green on each part independently

## Deps

None. Builds on sprint-1@87723933.
