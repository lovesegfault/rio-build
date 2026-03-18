# Plan 0168: Great helm migration — kustomize → Helm chart

## Design

Monolithic conversion: 81 files changed, 3563 insertions, 3243 deletions. The kustomize base+overlays approach had reached its limit — conditional blocks (TLS on/off, coverage mode, NodePort vs ClusterIP, pod-level vs cluster-level resources) were accumulating as JSON patches that were hard to reason about. Helm templates with `{{- if .Values.tls.enabled }}` blocks are the right tool for this shape of variability.

The chart at `infra/helm/rio-build/` has templates for all five deployments (gateway, scheduler, store, controller, worker-daemonset), RBAC, CRDs, PDB, Secrets, ServiceAccounts. Values files for each environment: `values.yaml` (defaults), `values/dev.yaml`, `values/vmtest-full.yaml` (k3s VM tests), `values/eks.yaml`. `nix/helm-render.nix` wraps `helm template` for use in NixOS tests (no Tiller; pure rendering → `kubectl apply`).

CRDs extracted via `scripts/split-crds.sh` — Helm's CRD handling is a known footgun (can't update, can't template, installed before values are read), so CRDs go into `infra/helm/crds/` as plain YAML outside the chart.

## Files

```json files
[
  {"path": "infra/helm/rio-build/Chart.yaml", "action": "NEW", "note": "chart metadata"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "NEW", "note": "defaults; all knobs"},
  {"path": "infra/helm/rio-build/templates/gateway.yaml", "action": "NEW", "note": "Deployment + Service + NodePort conditional"},
  {"path": "infra/helm/rio-build/templates/scheduler.yaml", "action": "NEW", "note": "Deployment + headless Service + RollingUpdate + Lease Role"},
  {"path": "infra/helm/rio-build/templates/store.yaml", "action": "NEW", "note": "Deployment + Service + PVC"},
  {"path": "infra/helm/rio-build/templates/controller.yaml", "action": "NEW", "note": "Deployment + RBAC for CRD reconcile"},
  {"path": "infra/helm/rio-build/templates/rbac.yaml", "action": "NEW", "note": "ServiceAccounts + Roles + RoleBindings"},
  {"path": "infra/helm/rio-build/templates/pdb.yaml", "action": "NEW", "note": "PodDisruptionBudgets"},
  {"path": "infra/helm/rio-build/templates/_helpers.tpl", "action": "NEW", "note": "labels, selectors, fullname"},
  {"path": "infra/helm/crds/workerpools.rio.build.yaml", "action": "NEW", "note": "CRD (outside chart, plain YAML)"},
  {"path": "infra/helm/crds/builds.rio.build.yaml", "action": "NEW", "note": "CRD"},
  {"path": "nix/helm-render.nix", "action": "NEW", "note": "helm template wrapper for NixOS tests"},
  {"path": "scripts/split-crds.sh", "action": "NEW", "note": "extract CRDs from chart for independent lifecycle"},
  {"path": "rio-proto/src/client/balance.rs", "action": "MODIFY", "note": "2-line path adjustment"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "4-line path adjustment"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "7-line path adjustment"}
]
```

## Tracey

No tracey markers — infrastructure refactor, no spec behaviors.

## Entry

- Depends on P0164: EKS Terraform (the helm chart deploys to the EKS cluster)

## Exit

Merged as `187a98a` (1 commit, 81 files). `helm template` + `kubectl apply` equivalent to prior kustomize output on EKS.
