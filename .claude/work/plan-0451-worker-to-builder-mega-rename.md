# Plan 0451: Worker→Builder atomic mega-rename — proto + CRD + crate + helm + nix + docker

[ADR-019](../../docs/src/decisions/019-builder-fetcher-split.md) splits the single "worker" pod type into **builders** (airgapped, run arbitrary derivation code) and **fetchers** (open egress, FOD-only, hash-check bounded). The terminology shift is total: "worker" was generic; "builder" says what it does. This plan lands phases R2-R4 of the rename — the ATOMIC block that must go in as one commit because proto/CRD/crate/helm/nix/docker all reference each other by type name. Split it and CI breaks at the `cargo build` step.

[P0450](plan-0450-sql-rename-worker-columns.md) already landed the SQL column renames (`assigned_worker_id`→`assigned_builder_id` etc.) as a standalone migration. This plan does NOT touch SQL. Phase R5 (tracey marker bulk-rename) and R6 (docs + dashboard) are follow-on plans that can land separately because `tracey query validate` and the dashboard build don't gate `.#ci`'s cargo check.

**Scope note:** this is a rename-plus-minimal-additions plan. The new `FetcherPool` CRD and `fetcherpool.yaml` helm template are SKELETONS — reconciler wiring, NetworkPolicy split, seccomp hardening, scheduler FOD routing are all follow-on plans. The goal here is: everything compiles, all existing tests green, zero `rio-worker` leftovers except the ~20 tokio/WebWorker false-positives.

## Entry criteria

- [P0450](plan-0450-sql-rename-worker-columns.md) merged (SQL columns `assigned_worker_id`→`assigned_builder_id`, `worker_id`→`builder_id`, index rename — so this plan's Rust code can reference the new column names)

## Tasks

### T1 — `refactor(proto):` worker.proto→builder.proto + ExecutorKind enum + all message/field renames

At [`rio-proto/proto/`](../../rio-proto/proto/):

1. `git mv worker.proto builder.proto`. Inside: `WorkerService`→`ExecutorService`, all `Worker*` message types → `Executor*` (or `Builder*` where builder-specific).
2. [`build_types.proto`](../../rio-proto/proto/build_types.proto): add `enum ExecutorKind { BUILDER = 0; FETCHER = 1; }`. Rename field `worker_id`→`executor_id` in all messages that carry it.
3. [`admin_types.proto`](../../rio-proto/proto/admin_types.proto): `WorkerInfo`→`ExecutorInfo`, add `ExecutorKind kind` field.
4. [`scheduler.proto`](../../rio-proto/proto/scheduler.proto): `HeartbeatRequest` gains `ExecutorKind kind`; rename `worker_id` fields.
5. Update `import` statements in all `.proto` files that reference `worker.proto`.
6. `rio-proto/build.rs`: update the proto file list if it's enumerated.

Validate: `nix develop -c cargo build -p rio-proto` compiles (downstream crates will break until T2-T4 land — that's expected, this is atomic).

### T2 — `refactor(crds):` workerpool.rs→builderpool.rs, add fetcherpool.rs skeleton, regen YAML

At [`rio-crds/src/`](../../rio-crds/src/):

1. `git mv workerpool.rs builderpool.rs`. Inside: `WorkerPool`→`BuilderPool`, `WorkerPoolSpec`→`BuilderPoolSpec`, `WorkerPoolStatus`→`BuilderPoolStatus`. **Delete the `fod_proxy_url` field** (fetchers get direct egress per ADR-019). Kind annotation `#[kube(kind = "BuilderPool", ...)]`, group stays `rio.build`, plural `builderpools`.
2. `git mv workerpoolset.rs builderpoolset.rs`. Inside: `WorkerPoolSet`→`BuilderPoolSet`, child ref type `WorkerPool`→`BuilderPool`.
3. NEW `fetcherpool.rs`: minimal `FetcherPool` CRD skeleton — `replicas`, `systems`, `nodeSelector`, `tolerations`, `resources`. No size-class (fetchers are network-bound). `#[kube(kind = "FetcherPool", group = "rio.build", plural = "fetcherpools")]`. Status struct can be a stub `{ ready_replicas: i32 }`.
4. [`lib.rs`](../../rio-crds/src/lib.rs): re-export the renamed + new types.
5. Run `cargo xtask regen crds` → rewrites `infra/helm/crds/workerpools.rio.build.yaml` to `builderpools.rio.build.yaml`, `workerpoolsets.rio.build.yaml` to `builderpoolsets.rio.build.yaml`, adds `fetcherpools.rio.build.yaml`. `git rm` the old YAML names if regen doesn't auto-delete.

Validate: `nix develop -c cargo build -p rio-crds` + `ls infra/helm/crds/` shows only the new names.

### T3 — `refactor(common,scheduler):` WorkerId→ExecutorId, WorkerState→ExecutorState, state/worker.rs→executor.rs

1. [`rio-common/src/newtype.rs`](../../rio-common/src/newtype.rs) (or wherever `WorkerId` lives): `WorkerId`→`ExecutorId`. This ripples to ~every crate.
2. [`rio-scheduler/src/state/worker.rs`](../../rio-scheduler/src/state/worker.rs) → `git mv` to `executor.rs`. Inside: `WorkerState`→`ExecutorState`, add `pub kind: ExecutorKind` field (from T1's proto). Module re-export in `state/mod.rs`.
3. [`rio-scheduler/src/state/newtypes.rs`](../../rio-scheduler/src/state/newtypes.rs): any `Worker*` newtypes → `Executor*`.
4. Scheduler-wide: `find_worker_with_overflow`→`find_executor_with_overflow`, `WorkerSlot`→`ExecutorSlot`, etc. Bulk `sed -i 's/WorkerState/ExecutorState/g; s/WorkerId/ExecutorId/g'` on `rio-scheduler/src/`, hand-review.

Validate: `nix develop -c cargo build -p rio-scheduler` (will fail on `rio_worker` crate import until T4 — acceptable mid-atomic).

### T4 — `refactor(worker→builder):` crate directory rename + all 644 type refs

1. `git mv rio-worker rio-builder`. Inside `rio-builder/Cargo.toml`: `name = "rio-builder"`, binary name `rio-builder`.
2. Root [`Cargo.toml`](../../Cargo.toml) workspace members: `"rio-worker"` → `"rio-builder"`.
3. [`rio-builder/src/config.rs`](../../rio-builder/src/config.rs): add `pub executor_kind: ExecutorKind` (from `RIO_EXECUTOR_KIND` env), **delete `fod_proxy_url`**.
4. Bulk identifier rename across ALL workspace crates: `use rio_worker`→`use rio_builder`, `rio-worker`→`rio-builder` in path literals, `WorkerConfig`→`BuilderConfig`, `WorkerError`→`BuilderError`, `WorkerRuntime`→`BuilderRuntime`, etc. `grep -rn 'rio_worker\|rio-worker\|WorkerConfig\|WorkerError\|WorkerRuntime' rio-*/src/ xtask/src/` to enumerate.
5. **PRESERVE** (do NOT rename): `#[tokio::test(flavor = "multi_thread", worker_threads = N)]` (~14 sites in `rio-scheduler/src/grpc/tests/`, `rio-store/src/gc/drain.rs`, `rio-builder/src/fuse/fetch.rs`), `graphLayout.worker.ts` + all `WebWorker` references in `rio-dashboard/src/lib/` (~8 sites).

Validate: `nix develop -c cargo build --workspace` compiles.

### T5 — `refactor(controller):` reconcilers/workerpool/→builderpool/, update CRD refs

At [`rio-controller/src/reconcilers/`](../../rio-controller/src/reconcilers/):

1. `git mv workerpool builderpool`. Inside all files: `WorkerPool`→`BuilderPool`, `WorkerPoolSpec`→`BuilderPoolSpec`, the reconciler struct name, `reconcile_workerpool`→`reconcile_builderpool`.
2. [`builders_tests.rs`](../../rio-controller/src/reconcilers/workerpool/tests/builders_tests.rs): update seccomp profile path literal `"profiles/rio-worker.json"`→`"profiles/rio-builder.json"`, `include_str!` path `seccomp-rio-worker.json`→`seccomp-rio-builder.json`.
3. Module registration in `reconcilers/mod.rs`: `workerpool`→`builderpool`. **Do NOT add `fetcherpool` reconciler yet** — skeleton only, wiring is a follow-on plan.
4. Delete `fod_proxy_url` threading wherever the reconciler passed it into pod env.

Validate: `nix develop -c cargo build -p rio-controller` + `cargo nextest run -p rio-controller`.

### T6 — `refactor(helm):` templates/workerpool.yaml→builderpool.yaml, add fetcherpool.yaml, delete fod-proxy.yaml, rename seccomp

At [`infra/helm/rio-build/`](../../infra/helm/rio-build/):

1. `git mv templates/workerpool.yaml templates/builderpool.yaml`. Inside: `kind: WorkerPool`→`BuilderPool`, `.Values.worker`→`.Values.builder`, labels `rio.build/role: worker`→`rio.build/role: builder`, taint keys `rio.build/worker`→`rio.build/builder`.
2. NEW `templates/fetcherpool.yaml`: skeleton `FetcherPool` manifest, `.Values.fetcher` block, `rio.build/role: fetcher` label. Minimal — just enough to `helm template` without error.
3. `git rm templates/fod-proxy.yaml`.
4. `git mv files/seccomp-rio-worker.json files/seccomp-rio-builder.json`. Update `templates/seccomp-installer.yaml` to reference the new filename.
5. [`values.yaml`](../../infra/helm/rio-build/values.yaml): `worker:` block → `builder:`, add `fetcher:` block (replicas, resources stubs). Karpenter NodePool names `rio-worker-*`→`rio-builder-*`, taint key `rio.build/worker`→`rio.build/builder`. Delete `fodProxy:` block.
6. [`templates/networkpolicy.yaml`](../../infra/helm/rio-build/templates/networkpolicy.yaml): delete the `fod-proxy:3128` egress rule from the worker policy. Rename policy `worker-egress`→`builder-egress`, selector labels.

Validate: `helm template infra/helm/rio-build/ | kubeconform -strict -` (or equivalent) passes.

### T7 — `refactor(nix):` modules/worker.nix→builder.nix, docker image names, test fixture refs

1. `git mv nix/modules/worker.nix nix/modules/builder.nix`. Inside: option namespace `services.rio-worker`→`services.rio-builder`, systemd unit name, binary path.
2. [`nix/docker.nix`](../../nix/docker.nix): image name `rio-worker`→`rio-builder`. Add a second image `rio-fetcher` that wraps the SAME `rio-builder` binary with `RIO_EXECUTOR_KIND=fetcher` env baked in (per ADR-019 §Terminology).
3. `nix/tests/`: grep for `rio-worker`, `services.rio-worker`, `WorkerPool` in all scenario files and `default.nix` — update to new names. The VM test fixture imports `modules/worker.nix` → `modules/builder.nix`.
4. Any `flake.nix` package outputs named `rio-worker` → `rio-builder`.

Validate: `nix flake check` dry-eval passes (don't run full VM tests locally — `/nixbuild .#ci` catches them).

### T8 — `refactor(xtask):` k8s deploy/status/smoke refs

At [`xtask/src/k8s/`](../../xtask/src/k8s/):

1. Grep `shared.rs`, `status.rs`, `eks/`, `k3s/`, `kind/` for `WorkerPool`, `workerpool`, `rio-worker`, `rio.build/worker` — rename all to builder equivalents.
2. `ensure_namespace` / namespace-creation logic: if it hardcodes `rio-system` only, leave alone (4-namespace split is a follow-on plan). If it already mentions `workers`, rename.
3. `xtask/src/regen/seccomp.rs`: output filename `seccomp-rio-worker.json`→`seccomp-rio-builder.json`.

Validate: `nix develop -c cargo build -p xtask`.

### T9 — `test:` grep-audit for leftovers + preserve-list verification

1. `grep -rn 'rio-worker\|rio_worker\|WorkerPool\|WorkerId\|WorkerState\|WorkerInfo\|WorkerService\|fod.proxy\|fodProxy' --include='*.rs' --include='*.proto' --include='*.yaml' --include='*.nix' --include='*.toml' .` → MUST be empty (except `.claude/`, `docs/`, and items on the preserve list below).
2. `grep -rn 'worker_threads\|WebWorker\|graphLayout\.worker\.ts' .` → MUST show ONLY the ~20 preserved sites (tokio test attrs + rio-dashboard WebWorker). Verify each hit is intentional.
3. `grep -rn 'seccomp-rio-worker\|rio-worker\.json' .` → MUST be empty.

## Exit criteria

- `/nixbuild .#ci` green
- `nix develop -c tracey query validate` → exit 0 (no broken `r[...]` refs; `r[ctrl.builderpool.reconcile]` impl site moved but still resolves)
- `grep -rn 'rio-worker\|rio_worker' --include='*.rs' --include='*.proto' --include='*.yaml' --include='*.nix' --include='*.toml' . | grep -v '.claude/\|docs/\|worker_threads\|WebWorker\|graphLayout.worker'` → 0 hits
- `grep -rn 'WorkerPool\|WorkerId\b\|WorkerState\|WorkerInfo\|WorkerService' --include='*.rs' --include='*.proto' .` → 0 hits
- `grep -rn 'fod.proxy\|fodProxy\|fod_proxy' --include='*.rs' --include='*.yaml' .` → 0 hits
- `ls infra/helm/crds/` → `builderpools.rio.build.yaml`, `builderpoolsets.rio.build.yaml`, `fetcherpools.rio.build.yaml` only (no `workerpool*`)
- `ls infra/helm/rio-build/templates/fod-proxy.yaml` → does not exist
- `ls rio-worker/` → does not exist; `ls rio-builder/Cargo.toml` → exists with `name = "rio-builder"`
- `grep -c 'worker_threads' rio-*/src/**/*.rs` → ≥9 (tokio attrs preserved)
- `ls rio-dashboard/src/lib/graphLayout.worker.ts` → exists (WebWorker preserved)

## Tracey

References existing markers:
- `r[ctrl.builderpool.reconcile]` — the `// r[impl ...]` site moves from `rio-controller/src/reconcilers/workerpool/mod.rs` to `rio-controller/src/reconcilers/builderpool/mod.rs`. T5 must carry the annotation across the `git mv`. `tracey query validate` catches if it's dropped.

New markers (spec text already in ADR-019, impl lands in follow-on plans — this plan does NOT add `r[impl]` for these):
- `r[ctrl.fetcherpool.reconcile]` — FetcherPool reconciler (skeleton here, wiring follow-on)
- `r[sched.dispatch.fod-to-fetcher]`, `r[sched.dispatch.no-fod-fallback]`, `r[builder.executor.kind-gate]`, `r[builder.netpol.airgap]`, `r[fetcher.netpol.egress-open]`, `r[fetcher.sandbox.strict-seccomp]`, `r[fetcher.node.dedicated]` — all follow-on plans

## Files

```json files
[
  {"path": "rio-proto/proto/worker.proto", "action": "RENAME", "to": "rio-proto/proto/builder.proto", "note": "T1: WorkerService→ExecutorService, all message renames"},
  {"path": "rio-proto/proto/build_types.proto", "action": "MODIFY", "note": "T1: +ExecutorKind enum, worker_id→executor_id"},
  {"path": "rio-proto/proto/admin_types.proto", "action": "MODIFY", "note": "T1: WorkerInfo→ExecutorInfo, +kind field"},
  {"path": "rio-proto/proto/scheduler.proto", "action": "MODIFY", "note": "T1: HeartbeatRequest +ExecutorKind, worker_id rename"},
  {"path": "rio-crds/src/workerpool.rs", "action": "RENAME", "to": "rio-crds/src/builderpool.rs", "note": "T2: WorkerPool→BuilderPool, DELETE fod_proxy_url field"},
  {"path": "rio-crds/src/workerpoolset.rs", "action": "RENAME", "to": "rio-crds/src/builderpoolset.rs", "note": "T2: WorkerPoolSet→BuilderPoolSet"},
  {"path": "rio-crds/src/fetcherpool.rs", "action": "CREATE", "note": "T2: FetcherPool CRD skeleton (replicas, systems, nodeSelector, tolerations, resources)"},
  {"path": "rio-crds/src/lib.rs", "action": "MODIFY", "note": "T2: re-exports"},
  {"path": "infra/helm/crds/workerpools.rio.build.yaml", "action": "DELETE", "note": "T2: regen → builderpools.rio.build.yaml"},
  {"path": "infra/helm/crds/workerpoolsets.rio.build.yaml", "action": "DELETE", "note": "T2: regen → builderpoolsets.rio.build.yaml"},
  {"path": "infra/helm/crds/builderpools.rio.build.yaml", "action": "CREATE", "note": "T2: xtask regen output"},
  {"path": "infra/helm/crds/builderpoolsets.rio.build.yaml", "action": "CREATE", "note": "T2: xtask regen output"},
  {"path": "infra/helm/crds/fetcherpools.rio.build.yaml", "action": "CREATE", "note": "T2: xtask regen output"},
  {"path": "rio-common/src/newtype.rs", "action": "MODIFY", "note": "T3: WorkerId→ExecutorId"},
  {"path": "rio-scheduler/src/state/worker.rs", "action": "RENAME", "to": "rio-scheduler/src/state/executor.rs", "note": "T3: WorkerState→ExecutorState, +kind field"},
  {"path": "rio-scheduler/src/state/mod.rs", "action": "MODIFY", "note": "T3: module re-export"},
  {"path": "rio-scheduler/src/state/newtypes.rs", "action": "MODIFY", "note": "T3: Worker* newtypes → Executor*"},
  {"path": "rio-worker/", "action": "RENAME", "to": "rio-builder/", "note": "T4: crate directory + Cargo.toml name"},
  {"path": "Cargo.toml", "action": "MODIFY", "note": "T4: workspace members rio-worker→rio-builder"},
  {"path": "rio-builder/src/config.rs", "action": "MODIFY", "note": "T4: +executor_kind, DELETE fod_proxy_url"},
  {"path": "rio-controller/src/reconcilers/workerpool/", "action": "RENAME", "to": "rio-controller/src/reconcilers/builderpool/", "note": "T5: WorkerPool→BuilderPool refs, seccomp path literals"},
  {"path": "rio-controller/src/reconcilers/mod.rs", "action": "MODIFY", "note": "T5: module registration"},
  {"path": "infra/helm/rio-build/templates/workerpool.yaml", "action": "RENAME", "to": "infra/helm/rio-build/templates/builderpool.yaml", "note": "T6: kind, values path, labels, taints"},
  {"path": "infra/helm/rio-build/templates/fetcherpool.yaml", "action": "CREATE", "note": "T6: FetcherPool skeleton manifest"},
  {"path": "infra/helm/rio-build/templates/fod-proxy.yaml", "action": "DELETE", "note": "T6: Squid proxy removed per ADR-019"},
  {"path": "infra/helm/rio-build/files/seccomp-rio-worker.json", "action": "RENAME", "to": "infra/helm/rio-build/files/seccomp-rio-builder.json", "note": "T6: filename only, profile content unchanged"},
  {"path": "infra/helm/rio-build/templates/seccomp-installer.yaml", "action": "MODIFY", "note": "T6: reference new seccomp filename"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T6: worker:→builder:, +fetcher:, karpenter pool/taint names, DELETE fodProxy:"},
  {"path": "infra/helm/rio-build/templates/networkpolicy.yaml", "action": "MODIFY", "note": "T6: delete fod-proxy:3128 egress, rename worker-egress→builder-egress"},
  {"path": "nix/modules/worker.nix", "action": "RENAME", "to": "nix/modules/builder.nix", "note": "T7: services.rio-worker→services.rio-builder"},
  {"path": "nix/docker.nix", "action": "MODIFY", "note": "T7: rio-worker image→rio-builder, +rio-fetcher (same binary, RIO_EXECUTOR_KIND=fetcher)"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T7: scenario refs, module imports"},
  {"path": "xtask/src/k8s/shared.rs", "action": "MODIFY", "note": "T8: WorkerPool→BuilderPool, rio-worker→rio-builder"},
  {"path": "xtask/src/k8s/status.rs", "action": "MODIFY", "note": "T8: k8s resource name refs"},
  {"path": "xtask/src/regen/seccomp.rs", "action": "MODIFY", "note": "T8: output filename seccomp-rio-worker.json→seccomp-rio-builder.json"}
]
```

```
rio-proto/proto/
├── builder.proto            # T1: RENAMED from worker.proto
├── build_types.proto        # T1: +ExecutorKind
├── admin_types.proto        # T1: WorkerInfo→ExecutorInfo
└── scheduler.proto          # T1: heartbeat +kind
rio-crds/src/
├── builderpool.rs           # T2: RENAMED, -fodProxyUrl
├── builderpoolset.rs        # T2: RENAMED
├── fetcherpool.rs           # T2: NEW skeleton
└── lib.rs                   # T2: re-exports
rio-common/src/
└── newtype.rs               # T3: WorkerId→ExecutorId
rio-scheduler/src/state/
├── executor.rs              # T3: RENAMED from worker.rs, +kind
├── newtypes.rs              # T3: Worker*→Executor*
└── mod.rs                   # T3: module re-export
rio-builder/                 # T4: RENAMED from rio-worker/
├── Cargo.toml               # T4: name = "rio-builder"
└── src/config.rs            # T4: +executor_kind, -fod_proxy_url
rio-controller/src/reconcilers/
├── builderpool/             # T5: RENAMED from workerpool/
└── mod.rs                   # T5: module reg
infra/helm/rio-build/
├── templates/
│   ├── builderpool.yaml     # T6: RENAMED
│   ├── fetcherpool.yaml     # T6: NEW
│   ├── fod-proxy.yaml       # T6: DELETED
│   ├── seccomp-installer.yaml  # T6: filename ref
│   └── networkpolicy.yaml   # T6: -fod-proxy rule, rename policy
├── files/
│   └── seccomp-rio-builder.json  # T6: RENAMED
├── crds/
│   ├── builderpools.rio.build.yaml      # T2: regen
│   ├── builderpoolsets.rio.build.yaml   # T2: regen
│   └── fetcherpools.rio.build.yaml      # T2: regen
└── values.yaml              # T6: worker→builder, +fetcher
nix/
├── modules/builder.nix      # T7: RENAMED
├── docker.nix               # T7: image names, +rio-fetcher
└── tests/default.nix        # T7: fixture refs
xtask/src/
├── k8s/{shared,status}.rs   # T8: k8s refs
└── regen/seccomp.rs         # T8: output filename
```

## Dependencies

```json deps
{"deps": [450], "soft_deps": [], "note": "P0450 renamed SQL columns assigned_worker_id→assigned_builder_id etc. — this plan's rio-scheduler/rio-store code references the new column names in sqlx queries. Must land AFTER the migration is applied."}
```

**Depends on:** [P0450](plan-0450-sql-rename-worker-columns.md) — SQL column rename migration. This plan's `rio-scheduler` and `rio-store` sqlx queries reference `assigned_builder_id` / `builder_id`; those columns must exist first.
**Conflicts with:** this plan touches ~265 files across every crate. It MUST land as a single atomic commit — no parallel plans can safely touch `rio-worker/`, `rio-crds/`, `rio-proto/`, `rio-controller/src/reconcilers/`, `infra/helm/`, or `nix/modules/` until this merges. Coordinate via `onibus collisions check 451` before launching any sibling plan.
