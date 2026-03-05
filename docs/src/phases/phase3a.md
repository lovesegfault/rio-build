# Phase 3a: Kubernetes Operator + Worker Store (Months 12-15)

**Goal:** K8s operator with CRDs, FUSE-backed worker stores at scale, deployment manifests.

**Implements:** [rio-controller](../components/controller.md), [rio-worker](../components/worker.md) FUSE store at scale

> **FUSE fallback impact:** If the Phase 1a fallback was activated (bind-mount instead of FUSE+overlay), the "Worker FUSE store" tasks below change to: explicit `nix-store --realise` pre-materialization on local SSD, bind-mount into sandbox, and directory-based cache management. The worker still requires `CAP_SYS_ADMIN` for overlayfs and the Nix sandbox. Prefetch hints still apply (pre-materialize paths before build starts) but lazy loading is eliminated. See [Phase 1a fallback plan](./phase1a.md#fuseoverlay-fallback-plan).

## Tasks

- [x] `rio-controller`: K8s operator
  - [x] CRD definitions: Build, WorkerPool (with CEL validation)
  - [x] WorkerPool reconciler: manage StatefulSet via server-side apply, finalizer-wrapped
  - [x] Autoscaler: poll `ClusterStatus.queued_derivations`, patch replicas with 30s/10min stabilization windows
  - [ ] Build reconciler: SubmitBuild + WatchBuild → status patches → CancelBuild on finalizer (deferred to 3b — SSH path is primary)
  - [ ] Leader election for scheduler via Kubernetes Lease (deferred to 3b — `Arc<AtomicU64>` generation plumbing is in place, lease task lands with replicas=2)
- [x] Worker FUSE prefetch
  - [x] Scheduler sends `PrefetchHint` (DAG children's outputs, bloom-filtered) before `WorkAssignment`
  - [x] Worker handles hint via `spawn_blocking` (Cache methods use `block_on` internally — nested-runtime panic otherwise)
  - [x] Singleflight shared with FUSE `ensure_cached` — prefetch returns early on `WaitFor` (hint not dependency)
- [x] Worker resource tracking (FIXES phase 2c VmHWM bug)
  - [x] cgroup v2 per-build: `memory.peak` + polled `cpu.stat usage_usec` — tree-wide, not daemon-PID
  - [x] `delegated_root()` = parent of `own_cgroup()` — per-build cgroups are SIBLINGS (no-internal-processes rule)
  - [x] systemd `Delegate=yes` + `DelegateSubgroup=builds` in NixOS module
  - [x] `CompletionReport.peak_cpu_cores` → `build_history.ema_peak_cpu_cores` (COALESCE blend)
- [x] Worker security context (`CAP_SYS_ADMIN` + `CAP_SYS_CHROOT`, NOT privileged — in StatefulSet generation)
- [x] Worker lifecycle
  - [x] `terminationGracePeriodSeconds=7200` in StatefulSet
  - [x] SIGTERM → `select!` biased → `DrainWorker` RPC → `acquire_many` wait → exit 0
  - [x] `AdminService.DrainWorker` + `WorkerState.draining` (one-way; `has_capacity()` checks `!draining`)
- [x] Health probes
  - [x] scheduler/store: `tonic-health` on existing gRPC port
  - [x] gateway: separate tonic server on `health_addr` (SSH has no gRPC)
  - [x] worker: axum `/healthz` + `/readyz` (readiness tracks heartbeat `accepted`)
  - [x] controller: inline HTTP `/healthz`
- [x] Store chunk backend + streaming zstd
  - [x] `ChunkBackendKind` config (inline/filesystem/s3) → one shared `Arc<ChunkCache>`
  - [x] Streaming compression pipeline (O(chunk×K) instead of O(nar_size))
  - [x] NixOS module `extraConfig` TOML for `[chunk_backend]`
- [ ] Kustomize manifests (deferred to 3b — `crdgen` produces CRD YAML; controller can run as systemd service against k3s for now)
- [ ] vm-phase3a with k3s (deferred to 3b — requires all above + kustomize; `vm-phase2c` validates store chunk backend + cgroup end-to-end)

## Milestone

Deploy to EKS with rio-controller managing WorkerPool, autoscale from 2 to 5 workers under load.

> **Status**: Core controller + worker lifecycle + resource tracking + prefetch pipeline complete. EKS deployment pending kustomize manifests (3b). 22 commits, 813 → 878 tests.

## Key Bugs Found During Implementation

- **SIGTERM drain**: `close()` + waiting `acquire_many` → Err even when permits return. Fix: skip `close()` — loop already broke.
- **cgroup layout**: per-build as children of `own_cgroup()` violates no-internal-processes. Fix: `delegated_root()` = parent; per-build are siblings.
- **tonic-health**: `set_not_serving` only affects named service, not "". K8s readinessProbe MUST specify `grpc.service=rio.scheduler.SchedulerService`.
- **Cache sharing**: `with_chunk_backend` created its OWN cache; comment claimed sharing. Fix: `with_chunk_cache(Arc)` actually shares.
- **HTTP health RST**: writing before reading + dropping stream → kernel RST. Fix: read request first, then write + `shutdown()`.
