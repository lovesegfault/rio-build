# Plan 0132: K8s manifests hardening — PDB/NetPol/spread/seccomp + nix.conf drift

## Design

Phase 3a's deploy manifests were "enough to run on k3s": Deployments, Services, a basic RBAC Role for the controller. No PodDisruptionBudgets (a cluster upgrade could evict all workers simultaneously), no NetworkPolicies (any pod could talk to any pod), no pod spreading (all worker replicas could land on one node), no Events (debugging a failed reconcile meant grepping controller logs). This plan landed all of it in one controller/deploy sweep.

**Per-WorkerPool PDB:** the controller's `build_pdb(wp)` generates a `PodDisruptionBudget` with `maxUnavailable: 1` and selector `{rio.build/pool: <name>}`, SSA-patched alongside the StatefulSet in `apply()`. RBAC extended for `policy/poddisruptionbudgets`. This replaced a static `pdb.yaml` with `TODO(phase4)` — the controller owns it now.

**Pod scheduling:** `WorkerPoolSpec.topology_spread: Option<bool>` (default `Some(true)`). When set, `build_pod_spec` emits `topologySpreadConstraints: [{maxSkew:1, topologyKey:"kubernetes.io/hostname", whenUnsatisfiable:ScheduleAnyway}]` plus soft `podAntiAffinity` (weight=100, same selector). Scheduler's Deployment got REQUIRED anti-affinity — `replicas=2` MUST spread across nodes, or the leader-election HA story is a lie.

**seccomp:** when `spec.privileged != Some(true)`, pod-level `seccompProfile: {type: RuntimeDefault}`. Pod-level (not container-level) so it applies to init containers too. Also: `dnsPolicy: ClusterFirstWithHostNet` when `spec.host_network == Some(true)` — without it, hostNetwork pods can't resolve ClusterIP services.

**Events:** `kube::runtime::events::Recorder` wired into `Ctx`. Emitted on: WorkerPool SSA-create (`Created`), scale decision (`Scaled` + direction), finalizer drain (`Draining`), Build submitted/terminal. RBAC for `events.k8s.io` already existed from 3a.

**NetworkPolicy:** prod overlay got `rio-scheduler-ingress-from` (from: gateway/worker/controller on 9001), `rio-store-ingress-from` (from: gateway/worker/scheduler on 9002), `rio-gateway-ingress` (from 0.0.0.0/0 on 2222). ServiceAccounts added for gateway/store/worker with `automountServiceAccountToken: false`. Store SA got a commented `eks.amazonaws.com/role-arn: REPLACE-ME` for IRSA (EKS overlay patches it).

**EKS overlay:** new `deploy/overlays/eks/` basing on prod. IRSA patch annotates store SA with `${STORE_IAM_ROLE_ARN}`. NLB patch annotates gateway Service: `aws-load-balancer-type: external`, `aws-load-balancer-nlb-target-type: ip`.

**nix.conf drift fix:** exploration found `rio-nix-conf` ConfigMap had `experimental-features = nix-command ca-derivations` but `WORKER_NIX_CONF` const in `executor/mod.rs` had empty features. The ConfigMap was defined but never mounted. CA support landed in phase 2c but the flag never propagated. Fix on both sides: `WORKER_NIX_CONF` now has `ca-derivations`, AND the ConfigMap is mounted at `/etc/rio/nix.conf` as an override — `setup_nix_conf` reads the mount if present, falls back to the const. Operators can customize without rebuilding images.

## Files

```json files
[
  {"path": "rio-controller/src/crds/workerpool.rs", "action": "MODIFY", "note": "topology_spread + fod_proxy_url fields"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "build_pdb + spread constraints + seccomp + dnsPolicy + nix-conf mount"},
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "MODIFY", "note": "SSA-patch PDB alongside STS"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests.rs", "action": "MODIFY", "note": "PDB selector + spread + seccomp assertions"},
  {"path": "rio-controller/src/reconcilers/mod.rs", "action": "MODIFY", "note": "Recorder in Ctx"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "create Reporter + Recorder"},
  {"path": "rio-controller/src/fixtures.rs", "action": "MODIFY", "note": "test Ctx with stub recorder"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "WORKER_NIX_CONF ca-derivations + setup_nix_conf override path"},
  {"path": "infra/base/crds.yaml", "action": "MODIFY", "note": "regenerated WorkerPool CRD"},
  {"path": "infra/base/rbac.yaml", "action": "MODIFY", "note": "policy/poddisruptionbudgets + SA for gateway/store/worker"},
  {"path": "infra/base/gateway.yaml", "action": "MODIFY", "note": "serviceAccountName"},
  {"path": "infra/base/scheduler.yaml", "action": "MODIFY", "note": "required podAntiAffinity"},
  {"path": "infra/base/store.yaml", "action": "MODIFY", "note": "serviceAccountName"},
  {"path": "infra/overlays/prod/networkpolicy.yaml", "action": "MODIFY", "note": "ingress policies for scheduler/store/gateway"},
  {"path": "infra/overlays/prod/pdb.yaml", "action": "MODIFY", "note": "remove TODO \u2014 controller-managed now"},
  {"path": "infra/overlays/eks/kustomization.yaml", "action": "NEW", "note": "bases: prod; patches: irsa, nlb"}
]
```

## Tracey

Markers implemented:
- `r[impl ctrl.pdb.workers]` — `build_pdb` in `builders.rs` (`ce4d5f3`).
- `r[impl ctrl.probe.named-service]` — readiness probe uses named service (annotation on existing correct code, `ce4d5f3`).

Verify annotations for both land in `ccc1ae1` (folded into P0134's tracey count as that's where the commit lives).

## Entry

- Depends on P0127: phase 3a complete (controller reconcilers, WorkerPool CRD, deploy/base/ structure).

## Exit

Merged as `ce4d5f3` (1 commit). `.#ci` green at merge. Tests: `build_pdb` generates correct selector + `maxUnavailable: 1`; `build_pod_spec` with `privileged=false` → `seccompProfile` set; with `hostNetwork=true` → `dnsPolicy: ClusterFirstWithHostNet`; spread constraint present when `topology_spread=true`.
