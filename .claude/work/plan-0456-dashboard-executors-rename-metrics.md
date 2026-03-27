# Plan 0456: dashboard Workers→Executors rename + FOD queue depth + fetcher utilization panels

[ADR-019](../../docs/src/decisions/019-builder-fetcher-split.md) split the generic "worker" into two kinds: **builders** (sandboxed derivation builds) and **fetchers** (FOD/network). [P0451](plan-0451-proto-executor-rename.md) renamed the proto types (`WorkerInfo`→`ExecutorInfo`, added `ExecutorKind` enum). [P0452](plan-0452-scheduler-fod-queue-metrics.md) landed the new metrics (`rio_scheduler_fod_queue_depth`, `rio_scheduler_fetcher_utilization`). This plan is the dashboard + Grafana tail — the last piece of the ADR-019 user-facing surface.

The Workers page becomes **Executors** and shows BOTH kinds with a kind filter. Grafana gets the split treatment: `worker-utilization.json` → `builder-utilization.json`, new sibling `fetcher-utilization.json`, and a new FOD-queue-depth panel (surfacing whether FOD scheduling is backed up — the main operational signal the split was designed to give).

## Entry criteria

- [P0451](plan-0451-proto-executor-rename.md) merged — proto has `ExecutorInfo` + `ExecutorKind`; admin service exposes `ListExecutors`/`DrainExecutor`
- [P0452](plan-0452-scheduler-fod-queue-metrics.md) merged — `rio_scheduler_fod_queue_depth` + `rio_scheduler_fetcher_utilization` emitted

## Tasks

### T1 — `refactor(dashboard):` regenerate api/types.ts from proto

After P0451, the proto defs have `ExecutorInfo`/`ExecutorKind`. Regen the TS types (`@bufbuild/protobuf` codegen) so [`types.ts`](../../rio-dashboard/src/api/types.ts) re-exports the new names. The re-export at `:20` (`WorkerInfo`) becomes `ExecutorInfo`; add `ExecutorKind` to the export list.

### T2 — `refactor(dashboard):` Workers.svelte → Executors.svelte with kind filter

`git mv` [`Workers.svelte`](../../rio-dashboard/src/pages/Workers.svelte) → `Executors.svelte`. Then:

1. `:9` `WorkerInfo` → `ExecutorInfo`; add `ExecutorKind` import
2. `:18` `workers` state → `executors`
3. `:26` `admin.listWorkers` → `admin.listExecutors`
4. Add a `kindFilter: ExecutorKind | 'all'` reactive state with a `<select>` at the top (`all` / `builder` / `fetcher`); filter the rendered list on it
5. `:41` `loadPct(w: WorkerInfo)` → `loadPct(e: ExecutorInfo)`
6. `:49` error text `listWorkers` → `listExecutors`
7. Page header `Workers` → `Executors`; add a kind column to the table

Also `git mv` [`Workers.test.ts`](../../rio-dashboard/src/pages/__tests__/Workers.test.ts) → `Executors.test.ts`; update `listWorkers` mocks → `listExecutors` at `:19`, `:42`, `:58`; add a test case for the kind filter.

### T3 — `refactor(dashboard):` App.svelte route + nav

At [`App.svelte`](../../rio-dashboard/src/App.svelte):

- `:24` `<Link to="/workers">Workers</Link>` → `<Link to="/executors">Executors</Link>`
- `:35` `<Route path="/workers"><Workers /></Route>` → `<Route path="/executors"><Executors /></Route>`
- Update the `import Workers from` line

### T4 — `refactor(dashboard):` DrainButton + Cluster + test-support refs

- [`DrainButton.svelte`](../../rio-dashboard/src/components/DrainButton.svelte) `:3`, `:13`, `:19` `WorkerInfo` → `ExecutorInfo`; `:54` `admin.drainWorker` → `admin.drainExecutor`; prop `workerId` → `executorId`
- [`DrainButton.test.ts`](../../rio-dashboard/src/components/__tests__/DrainButton.test.ts) `:26`, `:41`, `:52`, `:67`, `:82`, `:89`, `:104`, `:142` — `drainWorker` → `drainExecutor`; call-args assertion `workerId` → `executorId`
- [`DrainHarness.svelte`](../../rio-dashboard/src/components/__tests__/DrainHarness.svelte) `:8`, `:10`, `:14` `WorkerInfo` → `ExecutorInfo`
- [`Cluster.svelte`](../../rio-dashboard/src/pages/Cluster.svelte) `:51-52` — `Workers` → `Executors`, `activeWorkers`/`totalWorkers` → `activeExecutors`/`totalExecutors` (field names follow P0451 proto)
- [`admin-mock.ts`](../../rio-dashboard/src/test-support/admin-mock.ts) `:6` comment, `:46` `listWorkers: vi.fn()` → `listExecutors: vi.fn()`, `:50` `drainWorker: vi.fn()` → `drainExecutor: vi.fn()`

### T5 — `feat(grafana):` worker-utilization → builder-utilization + fetcher-utilization + FOD queue panel

1. `git mv` [`worker-utilization.json`](../../infra/helm/grafana/worker-utilization.json) → `builder-utilization.json`; update `uid` (`rio-worker-utilization` → `rio-builder-utilization`), `title`, and any `rio_worker_*` metric refs → `rio_builder_*` (per ADR-019 §Observability)
2. New `fetcher-utilization.json`: same panel layout, `uid: rio-fetcher-utilization`, queries `rio_scheduler_fetcher_utilization`
3. Add an **FOD queue depth** panel to `scheduler.json` (or `fetcher-utilization.json` — whichever has the scheduler panels): timeseries querying `rio_scheduler_fod_queue_depth` with a threshold annotation at the configured fetcher concurrency
4. Run `cargo xtask regen grafana` to regenerate `configmap.yaml`

## Exit criteria

- `pnpm test` green in `rio-dashboard/`
- `pnpm build` green (dashboard compiles against new proto types)
- `cargo xtask regen grafana` emits valid YAML; `jq . infra/helm/grafana/*.json >/dev/null` (all JSON parses)
- `/nbr .#ci` green
- `grep -r 'WorkerInfo\|listWorkers\|drainWorker\|/workers' rio-dashboard/src/` → 0 hits
- `grep -r 'rio_worker_' infra/helm/grafana/*.json` → 0 hits
- `ls rio-dashboard/src/pages/Executors.svelte infra/helm/grafana/builder-utilization.json infra/helm/grafana/fetcher-utilization.json` → all exist

## Tracey

References existing markers:
- `r[builder.executor.kind-gate]` — the kind filter in T2 is the dashboard surface for the executor-kind distinction this marker defines

No new spec markers — this is UI/observability plumbing, not normative behavior.

## Files

```json files
[
  {"path": "rio-dashboard/src/api/types.ts", "action": "MODIFY", "note": "T1: regen from proto — WorkerInfo→ExecutorInfo, +ExecutorKind export"},
  {"path": "rio-dashboard/src/pages/Workers.svelte", "action": "RENAME", "note": "T2: → Executors.svelte; kind filter, listExecutors"},
  {"path": "rio-dashboard/src/pages/__tests__/Workers.test.ts", "action": "RENAME", "note": "T2: → Executors.test.ts; mock renames + kind-filter test"},
  {"path": "rio-dashboard/src/App.svelte", "action": "MODIFY", "note": "T3: /workers→/executors route + nav link + import"},
  {"path": "rio-dashboard/src/components/DrainButton.svelte", "action": "MODIFY", "note": "T4: ExecutorInfo, drainExecutor, executorId prop"},
  {"path": "rio-dashboard/src/components/__tests__/DrainButton.test.ts", "action": "MODIFY", "note": "T4: drainExecutor mock refs"},
  {"path": "rio-dashboard/src/components/__tests__/DrainHarness.svelte", "action": "MODIFY", "note": "T4: ExecutorInfo type"},
  {"path": "rio-dashboard/src/pages/Cluster.svelte", "action": "MODIFY", "note": "T4: Workers→Executors label + proto field names"},
  {"path": "rio-dashboard/src/test-support/admin-mock.ts", "action": "MODIFY", "note": "T4: listExecutors, drainExecutor vi.fn()"},
  {"path": "infra/helm/grafana/worker-utilization.json", "action": "RENAME", "note": "T5: → builder-utilization.json; uid/title/metric refs"},
  {"path": "infra/helm/grafana/fetcher-utilization.json", "action": "CREATE", "note": "T5: rio_scheduler_fetcher_utilization panel"},
  {"path": "infra/helm/grafana/scheduler.json", "action": "MODIFY", "note": "T5: +FOD queue depth panel (rio_scheduler_fod_queue_depth)"},
  {"path": "infra/helm/grafana/configmap.yaml", "action": "MODIFY", "note": "T5: regen via cargo xtask regen grafana"}
]
```

```
rio-dashboard/src/
├── api/types.ts                          # T1: regen
├── App.svelte                            # T3: route
├── pages/
│   ├── Executors.svelte                  # T2: rename+filter (was Workers.svelte)
│   ├── Cluster.svelte                    # T4: label refs
│   └── __tests__/Executors.test.ts       # T2: rename
├── components/
│   ├── DrainButton.svelte                # T4: drainExecutor
│   └── __tests__/
│       ├── DrainButton.test.ts           # T4: mock refs
│       └── DrainHarness.svelte           # T4: type ref
└── test-support/admin-mock.ts            # T4: mock fns

infra/helm/grafana/
├── builder-utilization.json              # T5: rename (was worker-utilization.json)
├── fetcher-utilization.json              # T5: new
├── scheduler.json                        # T5: +FOD panel
└── configmap.yaml                        # T5: regen
```

## Dependencies

```json deps
{"deps": [451, 452], "soft_deps": [], "note": "P0451 renames proto types (ExecutorInfo/ExecutorKind) — T1 regen needs this. P0452 lands rio_scheduler_fod_queue_depth + rio_scheduler_fetcher_utilization — T5 panels query these."}
```

**Depends on:** [P0451](plan-0451-proto-executor-rename.md) (proto types), [P0452](plan-0452-scheduler-fod-queue-metrics.md) (metrics).
**Conflicts with:** none in collisions top-30. `rio-dashboard/` and `infra/helm/grafana/` have no other open plans touching them.
