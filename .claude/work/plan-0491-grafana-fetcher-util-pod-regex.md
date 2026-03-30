# Plan 491: Harden fetcher-utilization.json pod regex + add sibling helm-lint assert

[`infra/helm/grafana/fetcher-utilization.json:105,132`](../../infra/helm/grafana/fetcher-utilization.json)
uses `pod=~"rio-fetchers.*"` in cAdvisor queries. This works **by accident**
— the current default FetcherPool is named `rio` so pods are
`rio-fetchers-0`, `rio-fetchers-1`, etc. But if the pool is renamed (or a
second pool added), the regex silently matches zero pods and the panels go
blank. Same failure mode [P0458](plan-0458-grafana-builder-utilization-pod-regex.md)
fixed for `builder-utilization.json`.

The controller's FetcherPool STS naming follows the same
`format!("{name}-fetchers")` convention as builders → pods named
`{pool}-fetchers-{ordinal}`. The robust regex is `.*-fetchers-.*` (infix
match).

P0458 also added a helm-lint assert at [`flake.nix:952-956`](../../flake.nix)
for the builder dashboard; this plan adds the sibling assert for fetcher.

## Entry criteria

- [P0458](plan-0458-grafana-builder-utilization-pod-regex.md) merged (builder regex fix + helm-lint assert pattern exist)

## Tasks

### T1 — `fix(grafana):` fetcher-utilization.json — infix pod regex

MODIFY [`infra/helm/grafana/fetcher-utilization.json`](../../infra/helm/grafana/fetcher-utilization.json)
at `:105` and `:132` (and any other `pod=~"rio-fetchers.*"` occurrences —
grep the whole file). Replace:

```json
"expr": "sum by (pod) (rate(container_cpu_usage_seconds_total{pod=~\"rio-fetchers.*\", container!=\"\"}[5m]))"
```

with:

```json
"expr": "sum by (pod) (rate(container_cpu_usage_seconds_total{pod=~\".*-fetchers-.*\", container!=\"\"}[5m]))"
```

Regenerate `infra/helm/grafana/configmap.yaml` via `just grafana-configmap`
(if the justfile target exists — P0304-T3 adds it) or manual regen.

### T2 — `test(helm):` helm-lint assert for fetcher-utilization pod regex

MODIFY [`flake.nix`](../../flake.nix) after the builder-utilization assert
at `:956`. Add sibling assert:

```nix
# ── r[obs.metric.fetcher-util] dashboard regex (sibling to builder) ───
# Same controller STS naming: format!("{name}-fetchers") → pods
# {pool}-fetchers-{ordinal}. Assert the -fetchers- infix.
jq -r '.panels[].targets[]?.expr' \
  ${./infra/helm/grafana/fetcher-utilization.json} \
  | grep 'container_cpu_usage\|container_memory' \
  | grep -q -- '-fetchers-' \
  || { echo "FAIL: fetcher-utilization.json pod regex doesn't match controller STS naming ({pool}-fetchers-{N})" >&2; exit 1; }
```

## Exit criteria

- `grep 'rio-fetchers\.\*' infra/helm/grafana/fetcher-utilization.json` → 0 hits
- `grep -- '-fetchers-' infra/helm/grafana/fetcher-utilization.json` → ≥2 hits (CPU + memory queries)
- `nix build .#checks.x86_64-linux.helm-lint` green (new assert passes)
- `just grafana-configmap && git diff --exit-code infra/helm/grafana/configmap.yaml` — regen idempotent

## Tracey

References existing markers:
- `r[obs.metric.builder-util]` — T2's assert comment parallels the builder assert at [`flake.nix:943`](../../flake.nix) which references this marker

Adds new markers to component specs:
- `r[obs.metric.fetcher-util]` → [`docs/src/observability.md`](../../docs/src/observability.md) alongside `r[obs.metric.builder-util]` at `:212` (see Spec additions)

## Spec additions

```
r[obs.metric.fetcher-util]
The `fetcher-utilization.json` Grafana dashboard's cAdvisor pod selectors MUST use the `-fetchers-` infix regex (`pod=~".*-fetchers-.*"`) to match the controller's `format!("{name}-fetchers")` STS naming. A prefix-anchored regex (`rio-fetchers.*`) works only when the FetcherPool is named `rio`; renames or additional pools silently blank the panels.
```

## Files

```json files
[
  {"path": "infra/helm/grafana/fetcher-utilization.json", "action": "MODIFY", "note": "T1: .*-fetchers-.* infix regex at :105 :132"},
  {"path": "infra/helm/grafana/configmap.yaml", "action": "MODIFY", "note": "T1: regen to pick up T1's JSON change"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T2: helm-lint assert after :956"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T2: add r[obs.metric.fetcher-util] marker at :212"}
]
```

```
infra/helm/grafana/
├── fetcher-utilization.json  # T1: infix regex
└── configmap.yaml            # T1: regen
flake.nix                     # T2: helm-lint assert
docs/src/observability.md     # T2: marker
```

## Dependencies

```json deps
{"deps": [458], "soft_deps": [304, 311], "note": "P0458 established the builder-util fix + assert pattern; this is the fetcher sibling. Soft-dep P0304-T487 (inverts the builder assert at :952-956 to universal — this adds a sibling assert right after, coordinate at dispatch). Soft-dep P0311-T487 (adds grafana configmap drift check — orthogonal, both touch flake.nix helm-lint). discovered_from=458."}
```

**Depends on:** [P0458](plan-0458-grafana-builder-utilization-pod-regex.md) — builder fix + helm-lint assert pattern.
**Conflicts with:** [`flake.nix`](../../flake.nix) helm-lint block at `:660-960` is hot — P0304-T487 (this run) inverts the assert at `:952-956`; this adds a sibling right after. Sequence P0304 first, or coordinate in one commit. [`observability.md`](../../docs/src/observability.md) touched by P0295-T113/T114/T52/T57/T91 — all different sections, `:212` region is clear.
