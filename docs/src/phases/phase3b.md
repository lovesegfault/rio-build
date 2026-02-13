# Phase 3b: Production Hardening + Networking (Months 15-17)

**Goal:** Security hardening, network isolation, retry improvements, GC, and FOD support.

**Implements:** [Security](../security.md) (network policies, RBAC), [Error Taxonomy](../errors.md) (K8s-aware retry)

## Tasks

- [ ] RBAC (including PDB, NetworkPolicy, Events permissions), NetworkPolicy (with DNS egress), PodDisruptionBudget
- [ ] Pod scheduling: resource requests/limits, node affinity, anti-affinity, pod anti-affinity for worker spread
- [ ] EKS-specific: IRSA for S3, IMDSv2 hop limit=1 on worker nodes, NLB annotations
- [ ] K8s-aware retry: extend Phase 2a basic retry with pod preemption detection, node failure handling, and worker health tracking
- [ ] Basic GC: manual trigger via `AdminService.TriggerGC`, no automated scheduling yet. Mark-and-sweep with configurable grace period.
- [ ] FOD network egress proxy
  - Deploy forward proxy (e.g., Squid) as a ClusterIP service
  - Configurable domain allowlist (default: `cache.nixos.org`, `github.com`, `gitlab.com`)
  - Workers set `http_proxy`/`https_proxy` for FOD builds; non-FOD builds retain full egress deny
  - NetworkPolicy egress exception: workers → proxy service port
  - Audit logging for all proxied requests; non-allowlisted domains rejected
- [ ] Integration test: deploy on real EKS cluster, build nixpkgs.hello, survive single worker kill

## Milestone

Pass basic resilience test: single worker kill mid-build triggers reassignment, FOD builds fetch through proxy, NetworkPolicy blocks unauthorized egress.
