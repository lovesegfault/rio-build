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

### Carried-forward TODOs (tagged `TODO(phase3b)` in code)

These are concrete in-code deferrals from phases 2a–3a. Each should either be resolved or retagged before phase completion.

- [ ] **Build reconciler WatchBuild reconnect** (`rio-controller/src/reconcilers/build.rs`): on controller restart, store last-seen sequence in `.status` and call `WatchBuild(build_id, since_sequence)` to resume instead of letting status go stale. Currently: operator sees frozen Progress; manual delete+reapply required.
- [ ] **`build_event_log` time-based sweep** (`rio-scheduler/src/actor/build.rs`): fire-and-forget DELETE on build termination can miss rows if PG is down at that moment. Add a periodic sweep on the actor Tick that deletes rows older than some grace (e.g., `created_at < now - 1h`).
- [ ] **Delayed re-queue using computed backoff** (`rio-scheduler/src/actor/completion.rs`): Phase 2a retries transient failures immediately (no delay). Use the computed backoff `Duration` — ties into K8s-aware retry task above.
- [ ] **`ClusterStatus.store_size_bytes` background refresh** (`rio-scheduler/src/admin/mod.rs`): currently always 0 (endpoint is on the autoscaler hot path; inline store RPC too slow). Add a separate slow-refresh background task if the dashboard needs it.
- [ ] **`PutChunk` RPC** (`rio-store/src/grpc/chunk.rs`): stubbed UNIMPLEMENTED. Only needed if client-side chunking lands (worker chunks locally, sends chunks + manifest separately). Needs refcount policy for standalone chunks (short grace TTL before GC).
- [ ] **Worker cancel via `cgroup.kill`** (`rio-worker/src/main.rs`): on `SchedulerMessage::Cancel`, write "1" to the build's `cgroup.kill` (SIGKILLs the whole tree), release semaphore permit, send `CompletionReport{status: Cancelled}`. Needed for pod-preemption handling (scheduler cancels builds on an evicting node before SIGTERM grace wastes `terminationGracePeriodSeconds`).
- [ ] **Fuzz corpus persistence** (`flake.nix`): nightly 10-min fuzz runs currently discard the work corpus on each run. Add S3 upload/download so findings accumulate across runs.

## Milestone

Pass basic resilience test: single worker kill mid-build triggers reassignment, FOD builds fetch through proxy, NetworkPolicy blocks unauthorized egress.
