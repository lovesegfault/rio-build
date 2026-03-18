# Plan 0117: Kustomize manifests — base + overlays + crds package

## Design

Three commits built the K8s deployment surface: `deploy/base/` (CRDs + RBAC + Deployments for all components), `deploy/overlays/{dev,prod}/`, and `nix build .#crds` packaging.

`ab9a82f` packaged `nix build .#crds` + the `rio-controller` docker image. `f6d73ae` built `deploy/base/`: CRDs, RBAC (ClusterRole for controller: CRD get/watch/patch, StatefulSet create/update, Lease get/update), Deployments for scheduler/store/gateway/controller, Services. Validated with `kubectl kustomize deploy/base/` → 16 resources. Several gotchas baked into the manifests:

- **crdgen fix:** `serde_yaml` does NOT emit `---` document separators (comment claimed it did). Multi-doc YAML needs explicit `---` between; kustomize rejects otherwise ("mapping key already defined"). `crdgen` now joins with explicit `---`.
- **scheduler readinessProbe** MUST specify `grpc.service: rio.scheduler.SchedulerService` (the named service) — `set_not_serving::<S>()` only affects named, `""` stays SERVING. Liveness uses TCP not gRPC: standby replicas are NOT_SERVING (not ready, correct) but ALIVE — gRPC liveness would restart-loop them. See P0114.
- **`labels` not `commonLabels`:** the latter is deprecated + mutates selectors (immutable on StatefulSets). `includeSelectors: false` → metadata only.
- **gateway Service** port 22 → container 2222: no `CAP_NET_BIND_SERVICE` needed, Service remap is free.
- `crds.yaml` committed (not generated at apply time — kustomize needs real files). Regenerate: `nix build .#crds && cp result deploy/base/crds.yaml`.

`166b6d5` built overlays. **dev:** CloudNativePG (1 instance), MinIO (emptyDir, bucket-create Job with retry), example WorkerPool, `imagePullPolicy: Never` (airgap), 1 scheduler replica, Gateway ClusterIP (no LB controller in k3s/kind), `secretGenerator` for PG URL (hardcoded dev creds matching CNPG bootstrap). **prod:** PDBs (scheduler `maxUnavailable=1`, gateway `minAvailable=1`; none for controller/store — PDB on `replicas=1` either blocks drain forever or is a no-op). NetworkPolicy: workers egress ONLY scheduler+store+DNS — implicit deny on metadata endpoint (169.254.169.254) and internet. Controller gets cluster-egress with explicit metadata block (it needs apiserver, which isn't a pod — `ipBlock` not `podSelector`). Namespace `pod-security.kubernetes.io/enforce: privileged` (workers need `SYS_ADMIN` for FUSE; restricted/baseline reject). FOD internet access NOT allowed — design is "fetch on gateway, upload to store first."

## Files

```json files
[
  {"path": "infra/base/kustomization.yaml", "action": "NEW", "note": "labels (not commonLabels), includeSelectors: false"},
  {"path": "infra/base/crds.yaml", "action": "NEW", "note": "committed; regenerate via nix build .#crds"},
  {"path": "infra/base/rbac.yaml", "action": "NEW", "note": "ClusterRole: CRD/STS/Lease verbs; ServiceAccount"},
  {"path": "infra/base/controller.yaml", "action": "NEW", "note": "Deployment; env RIO_SCHEDULER_ADDR, RIO_STORE_ADDR"},
  {"path": "infra/base/scheduler.yaml", "action": "NEW", "note": "Deployment; readinessProbe grpc.service NAMED; liveness TCP"},
  {"path": "infra/base/store.yaml", "action": "NEW", "note": "Deployment; PVC for chunk filesystem backend"},
  {"path": "infra/base/gateway.yaml", "action": "NEW", "note": "Deployment; Service 22\u2192container 2222"},
  {"path": "infra/base/configmaps.yaml", "action": "NEW", "note": "shared config TOML"},
  {"path": "infra/overlays/dev/kustomization.yaml", "action": "NEW", "note": "CNPG, MinIO, imagePullPolicy: Never, secretGenerator"},
  {"path": "infra/overlays/prod/kustomization.yaml", "action": "NEW", "note": "PDBs, NetworkPolicy, PSA privileged"},
  {"path": "infra/overlays/prod/networkpolicy.yaml", "action": "NEW", "note": "workers egress only sched+store+DNS; implicit metadata deny"},
  {"path": "rio-controller/src/bin/crdgen.rs", "action": "MODIFY", "note": "explicit --- separator (serde_yaml doesn't emit)"},
  {"path": "nix/docker.nix", "action": "MODIFY", "note": "controller image; crds package"},
  {"path": "flake.nix", "action": "MODIFY", "note": ".#crds target"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). P0127 later removed false `r[ctrl.pdb.workers]` annotation (there is no PDB for workers — deliberately).

## Entry

- Depends on P0112: `crdgen` binary built by P0112.
- Depends on P0107: readinessProbe specs reference the probe surfaces P0107 created.

## Exit

Merged as `ab9a82f..166b6d5` (3 commits). `.#ci` green at merge. `kubectl kustomize deploy/base/` → 16 resources, no errors. `kubectl kustomize deploy/overlays/dev/` and `/prod/` both validate.
