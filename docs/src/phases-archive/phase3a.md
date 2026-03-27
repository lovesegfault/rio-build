# Phase 3a: Kubernetes Operator + Worker Store (Months 12-15)

**Goal:** K8s operator with CRDs, FUSE-backed worker stores at scale, deployment manifests.

**Implements:** [rio-controller](../components/controller.md), [rio-worker](../components/builder.md) FUSE store at scale

> **FUSE fallback impact:** If the Phase 1a fallback was activated (bind-mount instead of FUSE+overlay), the "Worker FUSE store" tasks below change to: explicit `nix-store --realise` pre-materialization on local SSD, bind-mount into sandbox, and directory-based cache management. The worker still requires `CAP_SYS_ADMIN` for overlayfs and the Nix sandbox. Prefetch hints still apply (pre-materialize paths before build starts) but lazy loading is eliminated. See [Phase 1a fallback plan](./phase1a.md#fuseoverlay-fallback-plan).

## Tasks

- [x] `rio-controller`: K8s operator
  - [x] CRD definitions: Build, WorkerPool (with CEL validation)
  - [x] WorkerPool reconciler: manage StatefulSet via server-side apply, finalizer-wrapped
  - [x] Autoscaler: poll `ClusterStatus.queued_derivations`, patch replicas with 30s/10min stabilization windows
  - [x] Build reconciler: SubmitBuild + WatchBuild → status patches → CancelBuild on finalizer (`110497b`). Single-node DAG (closure already in store). `build_id="submitted"` sentinel guards finalizer-add re-reconcile race.
  - [x] Leader election for scheduler via Kubernetes Lease (`380707a`). Gated on `RIO_LEASE_NAME` env; VM tests run without it (single scheduler). Readiness gated on `is_leader` (`a7feff6`).
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
- [x] Kustomize manifests (`b0e4128` base + `aac45d8` overlays). crdgen via `nix build .#crds`. serde_yaml no `---` — concat manually. `labels` not `commonLabels` (latter mutates selectors, breaks STS).
- [x] vm-phase3a with k3s (`e6ad6ae` + 7 followup fixes). 3 VMs. Controller-as-systemd (RBAC bootstrap ordering). WorkerPool `privileged+hostNetwork+imagePullPolicy` escape hatches. 2-drv chain asserts prefetch + cgroup memory.peak → build_history end-to-end.

## Milestone

Deploy to EKS with rio-controller managing WorkerPool, autoscale from 2 to 5 workers under load.

> **Status**: COMPLETE. Controller + worker lifecycle + resource tracking + prefetch + kustomize + k3s VM test. EKS deployment ready (manual, post-merge). 48 commits, 813 → 909 tests.

## Key Bugs Found During Implementation

- **SIGTERM drain**: `close()` + waiting `acquire_many` → Err even when permits return. Fix: skip `close()` — loop already broke.
- **cgroup layout**: per-build as children of `own_cgroup()` violates no-internal-processes. Fix: `delegated_root()` = parent; per-build are siblings.
- **tonic-health**: `set_not_serving` only affects named service, not "". K8s readinessProbe MUST specify `grpc.service=rio.scheduler.SchedulerService`.
- **Cache sharing**: `with_chunk_backend` created its OWN cache; comment claimed sharing. Fix: `with_chunk_cache(Arc)` actually shares.
- **HTTP health RST**: writing before reading + dropping stream → kernel RST. Fix: read request first, then write + `shutdown()`.
- **rustls dual-provider panic**: kube→ring, aws-sdk→aws-lc-rs, both active → rustls 0.23 can't auto-select. Fix: `install_default()` first line of main. Only rio-controller affected.
- **CRD schemars `serde_json::Value` → `{}`**: K8s rejects `type: Required value`. Fix: `schema_with=any_object` → `{type:object, x-kubernetes-preserve-unknown-fields:true}`.
- **cgroup-ns-root in pods**: containerd cgroupns → `/proc/self/cgroup=0::/` → parent=/sys/fs → CRASH. Fix: move-self-into-/leaf/, ns root becomes delegated_root.
- **imagePullPolicy `:latest` → Always**: K8s defaults `:latest` to `Always` (never check local). Airgap `ctr images import` → image present but kubelet ignores it → `ErrImagePull` loop. Fix: `WorkerPoolSpec.image_pull_policy` field (controller-managed STS can't be kustomize-patched) + docker tag `latest`→`dev`.
- **SSA status patch missing apiVersion+kind**: WorkerPool reconcile creates STS then 400s on status patch. `error_policy` logged at `debug!`, invisible → silent 30s retry loop. Fix: include `apiVersion`+`kind` in the json! patch (build.rs had it; workerpool.rs forgot).
- **overlay upperdir on container root = overlayfs**: `RIO_OVERLAY_BASE_DIR=/var/rio/overlays` had no volume mount → lands on containerd's overlayfs → can't create `trusted.*` xattrs → every mount EINVAL. Fix: emptyDir volume (node's ext4/xfs disk).
- **docker image missing `/nix/var/nix/db` + `/tmp`**: daemon spawn's bind target must exist; closure only populates `/nix/store`. Fix: `extraCommands` mkdir.
- **nixbld group empty member list**: `getgrnam->gr_mem` reads explicit members (4th field), not primary-group cross-ref from passwd. Fix: 8 nixbld{N} users + populated member list.
- **BuildResult start/stop_time never propagated → build_history EMA never fired**: nix-daemon's wire `BuildResult` has `start_time`/`stop_time` (u64 Unix epoch). Worker read them off the wire but set `start_time: None, stop_time: None` in the proto `CompletionReport`. Scheduler's `completion.rs` guards `update_build_history` on BOTH being `Some` → can't compute `duration_secs` → **entire** `build_history` row write silently skipped (incl. `peak_memory_bytes`, `peak_cpu_cores`, `output_size_bytes`). EMA updates had NEVER fired from real completions — vm-phase2c pre-seeds via `psql INSERT` so it never tested the real chain. Found by vm-phase3a's `psql SELECT ema_peak_memory_bytes` end-to-end check. Fix: propagate the wire fields.
