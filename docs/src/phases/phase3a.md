# Phase 3a: Kubernetes Operator + Worker Store (Months 12-15)

**Goal:** K8s operator with CRDs, FUSE-backed worker stores at scale, deployment manifests.

**Implements:** [rio-controller](../components/controller.md), [rio-worker](../components/worker.md) FUSE store at scale

> **FUSE fallback impact:** If the Phase 1a fallback was activated (bind-mount instead of FUSE+overlay), the "Worker FUSE store" tasks below change to: explicit `nix-store --realise` pre-materialization on local SSD, bind-mount into sandbox, and directory-based cache management. The worker still requires `CAP_SYS_ADMIN` for overlayfs and the Nix sandbox. Prefetch hints still apply (pre-materialize paths before build starts) but lazy loading is eliminated. See [Phase 1a fallback plan](./phase1a.md#fuseoverlay-fallback-plan).

## Tasks

- [ ] Validate FUSE mount + overlayfs + Nix sandbox in target EKS environment (spike should already be done in Phase 1a; this validates at scale)
- [ ] `rio-controller`: K8s operator
  - CRD definitions: Build, WorkerPool
  - WorkerPool reconciler: manage StatefulSet, autoscale based on queue depth
  - Build reconciler: create/track builds, update CRD status
  - Leader election for scheduler (Kubernetes Lease)
- [ ] Worker FUSE store (`rio-fuse`)
  - FUSE daemon integration in worker process (using `fuser` crate)
  - Local SSD cache with LRU eviction (configurable size via WorkerPool CRD)
  - Prefetch hint handling from scheduler via BuildExecution stream
  - overlayfs setup/teardown per build (FUSE as lower, SSD as upper)
- [ ] Worker security context (`CAP_SYS_ADMIN` + `CAP_SYS_CHROOT`, custom seccomp profile, dedicated node pool)
- [ ] Worker lifecycle (terminationGracePeriodSeconds=7200, preStop drain hook)
- [ ] Kustomize manifests for deployment (Helm chart deferred to Phase 4)
  - PostgreSQL (can use external or bundled via CloudNativePG)
  - S3 (MinIO for dev, external S3 for prod)
  - Gateway, scheduler, store, controller, worker pool
- [ ] Service definitions (gateway NLB with idle timeout=3600, scheduler ClusterIP, store ClusterIP + Ingress)
- [ ] Health probes for all components (including startup probes for workers)

## Milestone

Deploy to EKS with rio-controller managing WorkerPool, autoscale from 2 to 5 workers under load.
