# Plan 0453: Controller reconciler split — BuilderPool + new FetcherPool, shared STS builder, fod-proxy deletion

[ADR-019](../../docs/src/decisions/019-builder-fetcher-split.md) splits the single worker pod type into builders (airgapped, arbitrary-code) and fetchers (internet-facing, hash-checked FODs). [P0451](plan-0451-builderpool-fetcherpool-crds-rename.md) lands the CRD rename (`WorkerPool` → `BuilderPool`) and introduces the `FetcherPool` CRD. This plan adapts the controller reconciler layer: extract the 1100-line STS-building monolith at [`builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) into a shared `common/sts.rs`, add a minimal `fetcherpool/` reconciler that calls it with stricter security posture, delete the `fod_proxy_url` env threading (the proxy is gone per ADR-019), and widen controller RBAC to watch/patch STS across both executor namespaces.

The bulk of STS shape (FUSE volumes, wait-seccomp initContainer, TLS mounts, coverage propagation, RUST_LOG passthrough) is role-agnostic. The diff between builder and fetcher pods is a handful of fields: `rio.build/role` label, `securityContext.readOnlyRootFilesystem`, seccomp profile name, nodeSelector/toleration pair, and the `RIO_EXECUTOR_KIND` env. Extracting the shared builder lets the fetcher reconciler stay ~100 lines instead of copy-pasting 1100.

## Entry criteria

- [P0451](plan-0451-builderpool-fetcherpool-crds-rename.md) merged — `BuilderPool`/`FetcherPool` CRDs exist, `reconcilers/workerpool/` is renamed to `reconcilers/builderpool/`, `seccomp-rio-worker.json` is renamed to `rio-builder.json`

## Tasks

### T1 — `refactor(controller):` extract STS-building into reconcilers/common/sts.rs

At [`rio-controller/src/reconcilers/builderpool/builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) (post-P0451 path):

1. New module `reconcilers/common/sts.rs` exposing `pub fn build_executor_sts(params: ExecutorStsParams) -> StatefulSet`. `ExecutorStsParams` carries the role-varying bits: `role: ExecutorRole` (enum `Builder | Fetcher`), `seccomp_profile_name: &str`, `read_only_root_fs: bool`, `node_selector`/`tolerations`, `extra_env: Vec<EnvVar>`, plus the existing spec-driven knobs (replicas, systems, resources, TLS secret, etc.).
2. Move the role-agnostic STS shape — FUSE volumes (`:753+`), wait-seccomp initContainer (`:268-310`), pod-level seccomp (`:377-402`), TLS mounts (`:714-720`), coverage/RUST_LOG env passthrough (`:736-749`), `build_seccomp_profile` helper (`:980`) — into `common/sts.rs`.
3. `builderpool/builders.rs` shrinks to: construct `ExecutorStsParams { role: Builder, seccomp_profile_name: "rio-builder.json", read_only_root_fs: false, ... }`, call `build_executor_sts`, apply. Keep the builder-specific knobs (size-class annotations, `bloom_expected_items`, `daemon_timeout_secs`, `fuse_passthrough`) as `extra_env` entries.
4. Add `reconcilers/common/mod.rs` re-exporting `sts`.

The `// r[impl ctrl.pool.bloom-knob]` marker at `:710` and `// r[impl worker.seccomp.localhost-profile]` at `:979` move with their code; P0451 should have renamed the latter to `builder.seccomp.localhost-profile` — verify.

### T2 — `feat(controller):` new reconcilers/fetcherpool/ — minimal STS with strict securityContext

New `rio-controller/src/reconcilers/fetcherpool/mod.rs`:

1. `reconcile(fp: Arc<FetcherPool>, ctx: Arc<Context>)` — construct `ExecutorStsParams { role: Fetcher, seccomp_profile_name: "rio-fetcher.json", read_only_root_fs: true, node_selector: {"rio.build/node-role": "fetcher"}, tolerations: [{key: "rio.build/fetcher", effect: NoSchedule}], extra_env: [env("RIO_EXECUTOR_KIND", "fetcher")], ... }`, call `build_executor_sts`, server-side-apply into `rio-fetchers` namespace.
2. Pod label `rio.build/role: fetcher` (the shared builder sets this from `params.role`).
3. `securityContext.readOnlyRootFilesystem: true` at container level. The overlay upper-dir is a `tmpfs` emptyDir per ADR-019 §Sandbox hardening — ensure `common/sts.rs` already mounts a writable emptyDir at the build scratch path; if not, add one gated on `read_only_root_fs`.
4. Wire into `reconcilers/mod.rs` and the controller's `main.rs` watch-loop alongside `builderpool`.

No size-class, no disruption budget, no ephemeral-mode — `FetcherPool.spec` is minimal per ADR-019 §Two CRDs. If the shared `ExecutorStsParams` makes these `Option`al, the fetcher path passes `None`.

### T3 — `refactor(controller):` delete fod_proxy_url threading

At post-P0451 `builderpool/builders.rs` (currently [`:721-727`](../../rio-controller/src/reconcilers/workerpool/builders.rs)):

1. Delete the `if let Some(url) = &wp.spec.fod_proxy_url { e.push(env("RIO_FOD_PROXY_URL", url)); }` block and its comment.
2. P0451 should have already dropped `fod_proxy_url` from `BuilderPool.spec` — if a stale field ref remains, delete it. `grep -r fod_proxy rio-controller/` must return zero hits after this task.

### T4 — `feat(helm):` controller RBAC — watch/patch STS in rio-builders AND rio-fetchers

At [`infra/helm/rio-build/templates/rbac.yaml`](../../infra/helm/rio-build/templates/rbac.yaml):

1. The controller ClusterRole already grants `statefulsets` verbs (`:131`). Keep it.
2. Add two `RoleBinding`s binding that ClusterRole to the controller ServiceAccount in **both** `rio-builders` and `rio-fetchers` namespaces. The current single-namespace (`$ns` at `:1`) binding won't reach the new namespaces per ADR-019 §Consequences (cross-namespace RBAC).
3. If the chart's `.Values.namespace` schema is still single-valued, this depends on P0451 having introduced the four-namespace values layout — coordinate.

### T5 — `feat(seccomp):` new rio-fetcher.json — builder profile + extra denies

New `nix/seccomp/rio-fetcher.json` (or `infra/helm/rio-build/files/seccomp-rio-fetcher.json` — match wherever P0451 placed `rio-builder.json`):

1. Start from `rio-builder.json` (the renamed `seccomp-rio-worker.json`).
2. Add explicit `SCMP_ACT_ERRNO` entries for: `ptrace`, `bpf`, `setns`, `process_vm_readv`, `process_vm_writev`, `keyctl`, `add_key`.
3. Keep `mount` allowed — FUSE needs it per ADR-019 §Sandbox hardening.
4. Wire the new profile into the seccomp-installer DaemonSet template so it lands on fetcher nodes.

ADR-019 warns this may be too strict for git-lfs/hg/svn FODs. The profile starts tight; allowlist widens as real FODs hit denials. T6's git-based FOD subtest is the canary.

### T6 — `test(vm):` FetcherPool CR → running STS with correct labels+securityContext

New scenario fragment under `nix/tests/scenarios/` (or extend `lifecycle.nix`/`security.nix` — whichever already exercises controller→STS reconciliation):

1. Apply a `FetcherPool` CR in the `rio-fetchers` namespace.
2. Wait for the owned StatefulSet to reach `readyReplicas == spec.replicas`.
3. Assert pod labels include `rio.build/role: fetcher`.
4. Assert pod `spec.securityContext.readOnlyRootFilesystem == true` and `spec.containers[0].securityContext.seccompProfile.localhostProfile == "rio-fetcher.json"`.
5. Assert pod `spec.nodeSelector["rio.build/node-role"] == "fetcher"` and the matching toleration is present.
6. (Stretch, per ADR-019 §Consequences) run one git-based FOD through the fetcher to smoke-test the seccomp profile isn't over-tight.

Wire into `nix/tests/default.nix` `subtests = [...]` with the `# r[verify ...]` markers per CLAUDE.md VM-test placement rule — markers go at the wiring site, NOT in the scenario file header.

## Exit criteria

- `/nixbuild .#ci` green
- `ls rio-controller/src/reconcilers/` → `builderpool/`, `fetcherpool/`, `common/` all present
- `grep -r fod_proxy rio-controller/` → 0 hits
- `grep 'rio.build/role.*fetcher' rio-controller/src/reconcilers/fetcherpool/` → ≥1 hit
- `grep 'readOnlyRootFilesystem\|read_only_root_filesystem' rio-controller/src/reconcilers/common/sts.rs` → ≥1 hit (the param is threaded)
- `grep 'rio-fetchers' infra/helm/rio-build/templates/rbac.yaml` → ≥1 hit (RoleBinding exists)
- `jq '.syscalls[] | select(.action=="SCMP_ACT_ERRNO") | .names[]' <seccomp-dir>/rio-fetcher.json | grep -c 'ptrace\|bpf\|setns\|process_vm\|keyctl\|add_key'` → ≥7
- VM test: `FetcherPool` CR in `rio-fetchers` → STS running, pod has `rio.build/role: fetcher` label, `readOnlyRootFilesystem: true`, `rio-fetcher.json` seccomp, fetcher nodeSelector+toleration

## Tracey

Implements:
- `r[ctrl.builderpool.reconcile]` — T1+T3: builderpool reconciler calls shared STS builder, no fod-proxy
- `r[ctrl.fetcherpool.reconcile]` — T2: new fetcherpool reconciler
- `r[fetcher.sandbox.strict-seccomp]` — T5: rio-fetcher.json with extra denies; T2 sets readOnlyRootFilesystem

Verifies (T6, markers at `nix/tests/default.nix` subtests wiring):
- `r[ctrl.fetcherpool.reconcile]`
- `r[fetcher.sandbox.strict-seccomp]`
- `r[fetcher.node.dedicated]` — nodeSelector+toleration assertion

References existing:
- `r[builder.seccomp.localhost-profile]` (renamed from `worker.*` by P0451) — T1 moves the impl marker with the code into `common/sts.rs`

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/common/mod.rs", "action": "CREATE", "note": "T1: re-export sts"},
  {"path": "rio-controller/src/reconcilers/common/sts.rs", "action": "CREATE", "note": "T1: ExecutorStsParams + build_executor_sts extracted from builderpool/builders.rs"},
  {"path": "rio-controller/src/reconcilers/builderpool/builders.rs", "action": "MODIFY", "note": "T1: shrink to params-construct+call; T3: delete fod_proxy_url block at :721-727"},
  {"path": "rio-controller/src/reconcilers/fetcherpool/mod.rs", "action": "CREATE", "note": "T2: minimal reconciler — role:fetcher, readOnlyRootFilesystem, rio-fetcher.json seccomp, fetcher toleration+nodeSelector"},
  {"path": "rio-controller/src/reconcilers/mod.rs", "action": "MODIFY", "note": "T1+T2: pub mod common; pub mod fetcherpool;"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "T2: wire fetcherpool watch-loop alongside builderpool"},
  {"path": "infra/helm/rio-build/templates/rbac.yaml", "action": "MODIFY", "note": "T4: RoleBindings for controller SA in rio-builders + rio-fetchers namespaces"},
  {"path": "nix/seccomp/rio-fetcher.json", "action": "CREATE", "note": "T5: rio-builder.json + deny ptrace/bpf/setns/process_vm_*/keyctl/add_key. Path may be infra/helm/rio-build/files/ — match P0451's rio-builder.json location"},
  {"path": "infra/helm/rio-build/templates/seccomp-installer.yaml", "action": "MODIFY", "note": "T5: DS installs rio-fetcher.json on fetcher nodes"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T6: FetcherPool CR → STS assertion fragment (or new scenario file if lifecycle.nix is wrong home)"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T6: wire fetcherpool subtest with r[verify ctrl.fetcherpool.reconcile] + r[verify fetcher.sandbox.strict-seccomp] + r[verify fetcher.node.dedicated] markers"}
]
```

```
rio-controller/src/reconcilers/
├── builderpool/
│   └── builders.rs        # T1: shrunk — call common::sts; T3: fod_proxy_url deleted
├── common/                # T1: NEW
│   ├── mod.rs
│   └── sts.rs             # ExecutorStsParams, build_executor_sts
├── fetcherpool/           # T2: NEW
│   └── mod.rs
└── mod.rs                 # T1+T2: pub mod common; pub mod fetcherpool;

infra/helm/rio-build/
├── files/
│   └── seccomp-rio-fetcher.json   # T5 (or nix/seccomp/ per P0451)
└── templates/
    ├── rbac.yaml                  # T4: cross-namespace RoleBindings
    └── seccomp-installer.yaml     # T5: install fetcher profile

nix/tests/
├── default.nix            # T6: wire subtest + r[verify] markers
└── scenarios/
    └── lifecycle.nix      # T6: FetcherPool assertion fragment
```

## Dependencies

```json deps
{"deps": [451], "soft_deps": [452], "note": "P0451 is hard: CRDs must exist, workerpool/→builderpool/ rename done, seccomp-rio-worker.json→rio-builder.json. Soft P0452 (scheduler ExecutorKind routing): fetcher pods are useless until the scheduler routes FODs to them, but the reconciler can land independently — the STS comes up, just receives no work."}
```

**Depends on:** [P0451](plan-0451-builderpool-fetcherpool-crds-rename.md) — CRD types + directory rename + seccomp profile rename. This plan's file paths assume post-P0451 layout.
**Soft-depends on:** P0452 (scheduler FOD→fetcher routing) — fetcher pods without scheduler routing are idle but harmless. Can merge in either order.
**Conflicts with:** `builderpool/builders.rs` (post-rename) is the primary edit target. Any plan touching the pre-rename `workerpool/builders.rs` must land before P0451 or rebase after this plan. `onibus collisions check rio-controller/src/reconcilers/` before starting.
