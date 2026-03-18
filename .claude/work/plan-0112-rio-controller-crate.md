# Plan 0112: rio-controller crate — CRDs + WorkerPool reconciler + autoscaler

## Design

**New crate.** `rio-controller` is the K8s operator — the phase 3a deliverable. Four commits built the skeleton: CRD definitions, WorkerPool reconciler (manages StatefulSet via server-side apply), and an autoscaling loop that polls `ClusterStatus` (P0106) and patches replicas.

`52ba2a6` created the crate with `WorkerPoolSpec` + `BuildSpec` via `#[derive(CustomResource, KubeSchema)]`. The `KubeSchema` derive is essential — **alongside** `CustomResource` on spec structs. `KubeSchema` processes `#[x_kube(validation)]` attrs into `x-kubernetes-validations` (CEL); `CustomResource` alone ignores them, schema would have no CEL, apiserver accepts invalid specs. `KubeSchema` also emits `JsonSchema` internally — don't derive that separately (conflict). `crdgen` binary generates YAML for kustomize; `nix build .#crds` packages it.

`319020a` built the WorkerPool reconciler (845 LOC). `reconcile()` builds a StatefulSet spec from `WorkerPoolSpec`, applies via SSA with `.force()` (field-manager `rio-controller-workerpool`). The StatefulSet template encodes: worker container (image from `spec.image`), env vars (`RIO_SCHEDULER_ADDR`, `RIO_STORE_ADDR`, `RIO_WORKER_SIZE_CLASS`, `RIO_WORKER_MAX_BUILDS`), security context (`CAP_SYS_ADMIN` + `CAP_SYS_CHROOT`, NOT privileged — granular caps), `terminationGracePeriodSeconds=7200` (2h — enough for ~any single build), readiness probe on `/readyz` (P0107), liveness on `/healthz`. `.owns(Api::<StatefulSet>)` triggers reconcile on STS changes.

`8c78cd8` built the autoscaling loop (576 LOC). Polls `admin_client.cluster_status()` every 30s, computes `desired = clamp(queued / queue_per_worker, min, max)`, patches STS `.spec.replicas`. 30s scale-up stabilization window, 10min scale-down (flapping prevention — scale-down is the expensive direction, pods get drained). **Separate field manager** `rio-controller-autoscaler` so SSA doesn't conflict with the reconciler. Each WorkerPool gets its own tracker state (`last_scale_time`, prior desired) in a `DashMap`.

`8e97dbb` wired `main.rs`: `Controller::run` spawns both loops. `38ae823` (folded here) did mid-phase docs checkoff + retagged deferred TODOs.

## Files

```json files
[
  {"path": "rio-controller/Cargo.toml", "action": "NEW", "note": "new workspace member: kube, kube-runtime, schemars, k8s-openapi, serde_yaml"},
  {"path": "rio-controller/src/lib.rs", "action": "NEW", "note": "mod declarations; Controller struct"},
  {"path": "rio-controller/src/main.rs", "action": "NEW", "note": "Controller::run spawns reconciler + autoscaler"},
  {"path": "rio-controller/src/crds/mod.rs", "action": "NEW", "note": "re-exports"},
  {"path": "rio-controller/src/crds/workerpool.rs", "action": "NEW", "note": "WorkerPoolSpec: image, replicas{min,max}, resources, size_class, max_builds; CEL validation via x_kube"},
  {"path": "rio-controller/src/crds/build.rs", "action": "NEW", "note": "BuildSpec: drv_path; BuildStatus: phase, build_id, conditions"},
  {"path": "rio-controller/src/bin/crdgen.rs", "action": "NEW", "note": "dumps CRD YAML; nix build .#crds packages it"},
  {"path": "rio-controller/src/error.rs", "action": "NEW", "note": "Error enum: Kube, Scheduler, Store"},
  {"path": "rio-controller/src/reconcilers/mod.rs", "action": "NEW", "note": "shared Context{scheduler_client, store_client, store_addr}"},
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "NEW", "note": "reconcile() → build_statefulset → SSA .force(); .owns(StatefulSet)"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "NEW", "note": "build_statefulset, build_container; security caps, grace period, probes (split by P0124)"},
  {"path": "rio-controller/src/scaling.rs", "action": "NEW", "note": "autoscaler loop: poll ClusterStatus 30s, clamp(queued/queue_per_worker), 30s/10min stabilization, separate field-manager"},
  {"path": "Cargo.toml", "action": "MODIFY", "note": "rio-controller workspace member"},
  {"path": "docs/src/phases/phase3a.md", "action": "MODIFY", "note": "checkoff + deferred TODO retag"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `r[ctrl.crd.workerpool]`, `r[ctrl.crd.build]`, `r[ctrl.reconcile.sts-ssa]`, `r[ctrl.autoscale.poll-interval]`, `r[ctrl.autoscale.stabilization]`, `r[ctrl.sts.grace-period]`, `r[ctrl.sts.security-caps]`.

## Entry

- Depends on P0106: autoscaler polls `ClusterStatus`; WorkerPool finalizer (P0116) calls `DrainWorker`.
- Depends on P0107: readiness/liveness probes go in the STS template.
- Depends on P0108: `terminationGracePeriodSeconds=7200` is meaningful only because the worker's SIGTERM handler drains.

## Exit

Merged as `52ba2a6..8e97dbb` + `38ae823` (5 commits). `.#ci` green at merge. `cargo clippy -p rio-controller --all-targets -- --deny warnings` passes. `nix build .#crds` produces valid CRD YAML (validated by `kubectl apply --dry-run=server` in P0118).
