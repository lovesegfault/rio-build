# Plan 0169: Karpenter autoscaling + NLB cross-zone + controller balanced channel

## Design

Replaces the static `workers` managed nodegroup with Karpenter. Two-layer autoscaling chain is now fully wired: **rio-controller queue depth → pod replicas → Karpenter sees Pending → EC2 provisioned.**

**Terraform (`f0d34eb`):** `terraform-aws-eks` karpenter submodule (Pod Identity, SQS interruption queue, node IAM role, EKS access entry) + `helm_release` from OCI `public.ecr.aws`. `eks-pod-identity-agent` addon with `before_compute`. Discovery tags on node SG + private subnets. `rio.build/node-role=system` label on the system nodegroup — eks module v21 name-prefixes nodegroups so `eks.amazonaws.com/nodegroup` is unpredictable; never nodeSelector on it.

**Chart:** 1 `EC2NodeClass` + 3 weighted `NodePools` (c6a/c7a preferred @100, m/r fallback @10, general untainted @50). Default `WorkerPool` CR.

**Controller balanced channel (`6ba67d8`):** the autoscaler's `ClusterStatus` poll was still using the single-channel client → hit the standby 50% of the time → `UNAVAILABLE` → skipped poll. Switched to `BalancedChannel`.

**NLB cross-zone (`7f26b59`):** k8s-Ready ≠ NLB-target-healthy. NLB cross-zone off + `replicas < AZs` → 1/3 connects silently fail (the AZ with no target routes to nothing). Presents as a hang, not an error. `load_balancer.cross_zone.enabled=true` + smoke-test tunnel health check before declaring ready.

## Files

```json files
[
  {"path": "infra/eks/karpenter.tf", "action": "NEW", "note": "karpenter submodule: Pod Identity, SQS, node IAM, access entry"},
  {"path": "infra/eks/main.tf", "action": "MODIFY", "note": "eks-pod-identity-agent addon; discovery tags; rio.build/node-role=system label; workers nodegroup deleted"},
  {"path": "infra/eks/variables.tf", "action": "MODIFY", "note": "worker_* vars removed"},
  {"path": "infra/eks/outputs.tf", "action": "MODIFY", "note": "karpenter_node_role_name"},
  {"path": "infra/helm/rio-build/templates/karpenter.yaml", "action": "NEW", "note": "EC2NodeClass + 3 weighted NodePools"},
  {"path": "infra/helm/rio-build/templates/workerpool.yaml", "action": "NEW", "note": "default WorkerPool CR"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "autoscaler uses balanced channel to scheduler"},
  {"path": "infra/eks/smoke-test.sh", "action": "MODIFY", "note": "NLB cross-zone + tunnel health check before declaring ready"}
]
```

## Tracey

No tracey markers — infrastructure, no spec behaviors.

## Entry

- Depends on P0166: balanced channel (controller adopts it)
- Depends on P0168: helm migration (Karpenter templates in the chart)

## Exit

Merged as `f0d34eb`, `6ba67d8`, `7f26b59` (3 commits). Scale-up verified: submit 10 builds → rio-controller bumps replicas → Karpenter provisions EC2 → builds run.
