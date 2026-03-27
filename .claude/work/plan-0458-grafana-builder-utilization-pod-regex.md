# Plan 458: Grafana builder-utilization pod regex matches zero pods

The builder-utilization Grafana dashboard at [`infra/helm/grafana/builder-utilization.json`](../../infra/helm/grafana/builder-utilization.json) uses pod regex `pod=~"rio-builder.*"` (lines 86, 113), but the actual StatefulSet naming convention is `{pool}-builders-N` per [`rio-controller/src/reconcilers/builderpool/mod.rs:412`](../../rio-controller/src/reconcilers/builderpool/mod.rs) (`format!("{name}-builders")`). The regex matches zero pods — panels show no data.

This is a pre-existing bug inherited from the former `worker-utilization.json` dashboard, surfaced during [P0456](plan-0456-dashboard-executors-rename-metrics.md)'s rename sweep. The controller-generated STS names never used the `rio-builder-` prefix; they use the BuilderPool CR's `.metadata.name` followed by `-builders`. Example: a pool named `default` produces STS `default-builders` with pods `default-builders-0`, `default-builders-1`, etc.

Fix: change the regex to match the `-builders-` suffix pattern, or (preferred) use a label selector on the `app.kubernetes.io/component=builder` label if the controller sets it. The regex fix is the minimal change; the label-selector fix is more robust against future naming changes.

## Entry criteria

- [P0456](plan-0456-dashboard-executors-rename-metrics.md) merged (builder-utilization.json exists, renamed from worker-utilization.json)

## Tasks

### T1 — `fix(grafana):` builder-utilization pod regex — `rio-builder.*` → `.*-builders-.*`

Two PromQL expressions at [`builder-utilization.json:86,113`](../../infra/helm/grafana/builder-utilization.json):

```json
"expr": "sum by (pod) (rate(container_cpu_usage_seconds_total{pod=~\"rio-builder.*\", container!=\"\"}[5m]))"
"expr": "sum by (pod) (container_memory_working_set_bytes{pod=~\"rio-builder.*\", container!=\"\"})"
```

Change `pod=~"rio-builder.*"` → `pod=~".*-builders-.*"` at both sites. The `-builders-` infix is the controller-enforced naming per [`builderpool/mod.rs:412,581`](../../rio-controller/src/reconcilers/builderpool/mod.rs) and [`scaling/per_class.rs:127`](../../rio-controller/src/scaling/per_class.rs). The trailing `-` before `.*` ensures we match `{pool}-builders-{ordinal}` (the STS pod name) and not a hypothetical bare `{pool}-builders` service name.

Regenerate the configmap: `just grafana-configmap` (or equivalent) and commit both the JSON and the generated `configmap.yaml`.

### T2 — `test(grafana):` assert builder-utilization regex matches controller STS naming

Add a small check (bash assert in `flake.nix` helm-lint, or a unit test in rio-controller) that verifies the pod-regex in `builder-utilization.json` would match a name produced by the controller's STS naming format. Minimal approach: a grep-based assert in the existing helm-lint checkPhase alongside the `GRPCRoute` method-count assert (same pattern as [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T48). Shape:

```bash
# controller uses format!("{name}-builders"); dashboard regex must match
jq -r '.panels[].targets[]?.expr' infra/helm/grafana/builder-utilization.json \
  | grep 'container_cpu_usage\|container_memory' \
  | grep -q -- '-builders-' \
  || { echo "builder-utilization.json pod regex doesn't match controller STS naming"; exit 1; }
```

This catches future regressions if either the dashboard regex or the controller naming format changes without the other.

## Exit criteria

- `/nbr .#ci` green
- `grep 'rio-builder\.\*' infra/helm/grafana/builder-utilization.json` → 0 hits (old regex removed)
- `grep -c -- '-builders-' infra/helm/grafana/builder-utilization.json` → ≥2 hits (both PromQL expressions fixed)
- `just grafana-configmap && git diff --exit-code infra/helm/grafana/configmap.yaml` — regen idempotent (configmap reflects the fix)
- T2: helm-lint assert present and passes against the fixed JSON

## Tracey

References existing markers:
- `r[obs.metric.builder-util]` — T1 fixes the dashboard that surfaces these gauges; the `r[impl]` is at the builder's `utilization_reporter_loop`, this plan fixes the consumer side

No new markers — the dashboard consuming `container_*` metrics (cadvisor-sourced, not rio-emitted) isn't spec'd in rio's observability markers; `r[obs.metric.builder-util]` covers the rio-emitted `rio_builder_{cpu,memory}_fraction` gauges, which is the adjacent functionality.

## Files

```json files
[
  {"path": "infra/helm/grafana/builder-utilization.json", "action": "MODIFY", "note": "T1: :86,113 pod regex rio-builder.* → .*-builders-.*"},
  {"path": "infra/helm/grafana/configmap.yaml", "action": "MODIFY", "note": "T1: regenerated from builder-utilization.json"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T2: helm-lint assert — dashboard regex matches controller STS naming"}
]
```

```
infra/helm/grafana/
├── builder-utilization.json   # T1: regex fix at :86, :113
└── configmap.yaml             # T1: regenerated
flake.nix                      # T2: helm-lint assert
```

## Dependencies

```json deps
{"deps": [456], "soft_deps": [], "note": "Standalone correctness fix. Depends on P0456 (DONE — builder-utilization.json exists). discovered_from=456 (coverage-sink)."}
```

**Depends on:** [P0456](plan-0456-dashboard-executors-rename-metrics.md) — the dashboard file was renamed from `worker-utilization.json`; the regex bug predates the rename but the file path is post-P0456.
**Conflicts with:** [`flake.nix`](../../flake.nix) helm-lint checkPhase — same block as [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T48 (GRPCRoute method-count assert) and T51 (yq thirdparty-image assert). T2's jq-grep assert is additive, non-overlapping bash block. `builder-utilization.json` is low-traffic (no collision-count entry).
