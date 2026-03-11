# Phase 4c: Adaptive Scheduling + Validation + Polish (Months 20-22)

**Goal:** Activate SITA-E adaptive size-class cutoffs, ship the `WorkerPoolSet` CRD with per-class autoscaling, close VM test gaps (PDB/NetPol/FOD proxy), deliver Grafana dashboards + load benchmarks, and sync all deferral blocks out of the design docs.

**Implements:** [ADR-015](../decisions/015-size-class-routing.md) (`CutoffRebalancer` + `WorkerPoolSet` CRD), [Capacity Planning](../capacity-planning.md) (declarative size-class pools), [Verification](../verification.md) (criterion benchmarks + VM test matrix)

> **Depends on Phase 4a/4b:** `CutoffRebalancer` writes to `build_samples` (needs migration 009 chain). `GetSizeClassStatus` RPC shares the admin-RPC pattern from 4a. VM tests exercise rate limiting (4b), GC automation (4b), and tenant smoke (4a).

## Tasks

### SITA-E adaptive cutoffs (D2: `build_samples` table)

- [ ] **Migration 009 Part D**: `build_samples(id BIGSERIAL PK, pname, system, duration_secs DOUBLE, peak_memory_bytes BIGINT, completed_at TIMESTAMPTZ)` + time index. `build_history` stores EMA only; SITA-E needs raw samples for an empirical CDF. Retention: scheduler background task `DELETE WHERE completed_at < now() - interval '30 days'`.
- [ ] Write to `build_samples` from `actor/completion.rs` alongside the existing EMA update. One row per successful completion.
- [ ] **`CutoffRebalancer`** (`rio-scheduler/src/rebalancer.rs`) — background task, configurable interval (default 1h). Algorithm: query last-7-days samples → sort by `duration_secs` → compute SITA-E equalizing cutoffs (partition into N classes such that `sum(duration)` per class is equal). Apply EMA smoothing (`alpha=0.3` default) between old and new cutoffs to damp oscillation. Minimum samples threshold (default 100) before adjusting.
- [ ] Wire `CutoffRebalancer` into `DagActor` via shared `Arc<RwLock<Vec<SizeClassConfig>>>`. `classify()` in `assignment.rs` reads through the RwLock. Rebalancer writes. Actor never blocks on the lock (RwLock read is cheap, writes are rare).
- [ ] **CPU-based class bumping** — extend `classify()` to consult `ema_peak_cpu_cores` alongside duration+memory. Add `cpu_limit_cores: Option<f64>` to `SizeClassConfig`. High-CPU builds bump to a class with more cores even if duration/memory would place them lower.
- [ ] `rio_scheduler_class_load_fraction` gauge (`observability.md:116` Phase 4+ row) — emitted by `CutoffRebalancer` on each recompute. Input to future alerting ("class imbalance >2× for >1h").
- [ ] `rio_scheduler_misclassification_total` counter — incremented when a completed build's actual duration would have placed it in a different class than the one it ran on. Signals cutoff drift.
- [ ] **`GetSizeClassStatus` admin RPC** — new `GetSizeClassSnapshot` ActorCommand (O(n) scan over ready queue, acceptable for the controller's 30s poll; pattern from `ClusterSnapshot` at `actor/command.rs:183-305`). Response: per-class `{name, effective_cutoff_secs, configured_cutoff_secs, queued_derivations, running_derivations, sample_count}`.
- [ ] Unit test: seed `build_samples` with a synthetic bimodal distribution → `CutoffRebalancer` converges to the expected boundary within 3 iterations.

### `WorkerPoolSet` CRD (D3: full reconciler)

- [ ] **CRD struct** (`rio-controller/src/crds/workerpoolset.rs`) — `WorkerPoolSetSpec { size_classes: Vec<SizeClassSpec>, cutoff_learning: CutoffLearningConfig, image, systems, features, ... }`. `SizeClassSpec { name, duration_cutoff: Option<f64>, pool: PoolTemplate }`. `PoolTemplate` is a subset of `WorkerPoolSpec` (omits `image`/`systems`/`sizeClass` — derived from parent/class name). `CutoffLearningConfig { enabled, recompute_interval_secs, min_samples, smoothing_alpha }` mirrors scheduler config. `WorkerPoolSetStatus { classes: Vec<ClassStatus> }` where `ClassStatus { name, effective_cutoff_secs, replicas, ready_replicas, queued_derivations }`.
  - Derive pattern: `#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, KubeSchema)]` — **`KubeSchema` NOT `JsonSchema` on spec** (enables CEL via `#[x_kube(validation)]`). Status struct uses plain `JsonSchema + Default`. k8s-openapi passthrough types via `#[schemars(schema_with = "crate::crds::any_object")]`. See `crds/workerpool.rs:31-44,224-235` for the reference pattern.
  - Add `WorkerPoolSet::crd()` serialization to `rio-controller/src/bin/crdgen.rs`.
- [ ] **Reconciler** (`rio-controller/src/reconcilers/workerpoolset/mod.rs`):
  - `apply`: for each `size_class`, build child `WorkerPool` (name = `{wps-name}-{class-name}`, `sizeClass` = class name, image/systems from WPS shared fields, replicas/resources from `PoolTemplate`) + set `controller_owner_ref(&())` (`&()` because `DynamicType=()` for static CRDs) → SSA patch. Finalizer-wrapped (pattern from `reconcilers/workerpool/mod.rs:84-109`).
  - `cleanup`: explicit child `WorkerPool` delete for immediate feedback (ownerRef GC handles the cascade but is asynchronous).
  - Status refresh: call `GetSizeClassStatus` → write `effective_cutoff` + `queued_derivations` per class. **SSA status patch MUST include `apiVersion` + `kind`** (pattern from `reconcilers/workerpool/mod.rs:230-250` — the 3a bug where this was forgotten caused silent 30s retry loops).
- [ ] Child builder (`rio-controller/src/reconcilers/workerpoolset/builders.rs`) — `build_child_workerpool(wps, class) -> WorkerPool`. Merge shared fields + `PoolTemplate`, set `sizeClass`, set ownerRef.
- [ ] **Per-class autoscaler** (`rio-controller/src/scaling.rs` extension) — call `GetSizeClassStatus`, apply existing `compute_desired(queued, target, min, max)` per child pool. Separate SSA field manager `"rio-controller-wps-autoscaler"` (pattern at `scaling.rs:338`). Skip pools being deleted (skip-deleting guard at `scaling.rs:227-230`).
- [ ] Wire third `Controller` in `main.rs` with `.owns(Api::<WorkerPool>::all(...))` + `tokio::join!` → `join3`.
- [ ] RBAC: add `workerpoolsets` resource to `deploy/base/rbac.yaml`. Regenerate `deploy/base/crds.yaml`.
- [ ] `rio-cli cutoffs` + `rio-cli wps` subcommands.
- [ ] VM test: apply a `WorkerPoolSet` CR → controller creates 3 child `WorkerPool` CRs with correct `sizeClass` + ownerRef → delete WPS → children deleted via ownerRef GC cascade.

### VM tests (D6: E + D in scope)

- [ ] **phase3a section E (PDB)** — extend `nix/tests/phase3a.nix` (~25 lines). Assert controller-managed PDB `{pool}-pdb` exists with `maxUnavailable: 1` + ownerRef points to `WorkerPool`. Delete `WorkerPool` → PDB GC'd via ownerRef.
- [ ] **`nix/tests/phase4.nix`** — new test file, 4 VMs (control + k8s + worker + client):
  - Section A: tenant smoke (from 4a milestone).
  - Section B: NetPol egress block — k3s bundled kube-router (current config doesn't `--disable-network-policy`). Assert `kubectl exec worker curl 169.254.169.254` → blocked, `curl 1.1.1.1:80` → blocked. If kube-router doesn't enforce: fallback to Calico with image preload. **NetPol ingress filtering SKIPPED** — scheduler/store aren't pods in this topology (systemd on `control` VM), so `podSelector` matches nothing. Document in test comment.
  - Section C: GC schedule smoke + rate limit trip + `maxSilentTime` kill (all from 4b).
  - Section D: A1 cancel timing — submit a slow build, cancel mid-run, assert `rio_scheduler_cancel_signals_total` increments AND worker's cgroup is gone within 5s.
  - Section E: `rio-cli` smoke against the live scheduler (`status`, `workers`, `builds`, `gc --dry-run`).
  - Section F: `WorkerPoolSet` full flow.
  - Section G: load scenario — 50-derivation DAG, assert completion + Prometheus scrape shows `rio_scheduler_derivations_completed_total >= 50`.
- [ ] **`nix/tests/phase4-fod.nix`** — new test file (~200 lines). Squid as a k8s pod (tests the production `fod-proxy.yaml` manifest directly). Local HTTP origin (busybox httpd pod) for airgap-safe fetch. ConfigMap patch adds the local origin to the Squid allowlist. Assertions: FOD fetching allowlisted hostname → success + Squid access log shows the request; FOD fetching non-allowlisted → fails with `TCP_DENIED/403` in Squid log; non-FOD derivation → no `http_proxy` env set. New `dockerImages.squid` in `flake.nix` (~25 lines).
- [ ] Add `vm-phase4` and `vm-phase4-fod` to the `ci-fast` aggregate.

### Grafana + benchmarks

- [ ] **Grafana dashboard JSONs** (`deploy/grafana/`): Build Overview (active builds, completion rate, p50/p99 duration), Worker Utilization (per-class replicas, CPU/memory gauges, queue depth), Store Health (cache hit rate, GC sweep size, S3 latency), Scheduler (dispatch latency, ready-queue depth, backstop timeouts, cancel signals).
- [ ] **`rio-bench` crate** — criterion benches. Synthetic DAG generators: linear(N), binary-tree(depth), shared-diamond(N, shared_fraction). Benches: SubmitBuild latency at N∈{1,10,100,1000}; dispatch throughput (derivations/sec through a mocked worker channel). Add `rio-bench` to `flake.nix` packages + a `nix run .#bench` alias.
- [ ] Hydra comparison: documented manual procedure in `docs/src/capacity-planning.md` — equivalent hardware, same nixpkgs revision, wall-clock + CPU-seconds.

### Small deferrals + doc sync

- [ ] **Custom seccomp profile** (`deploy/base/seccomp-rio-worker.json`) — `Localhost` profile denying `ptrace`, `bpf`, `setns`, `process_vm_readv`, `process_vm_writev` under `CAP_SYS_ADMIN`. Add `seccomp_profile: Option<SeccompProfileKind>` to `WorkerPoolSpec` (default `RuntimeDefault`, option for `Localhost` with path). Closes `security.md:53,173,175` Phase 4 tracking.
- [ ] **`BuildStatus` fine-grained conditions** — wire `Scheduled`/`InputsResolved`/`Building` conditions in the Build reconciler's `drain_stream` event handler (`controller.md:30,43` deferral). `criticalPathRemaining` + `workers` fields in `BuildStatus` require a `BuildEvent` proto extension — include if straightforward, else defer the fields (not the conditions) to Phase 5.
- [ ] **CN-list config + SAN check for HMAC bypass** — currently only `CN=rio-gateway` bypasses (Phase 3b X1 fix). Add `bypass_cns: Vec<String>` to store config + check SAN entries alongside CN. Allows multi-gateway deployments with distinct certs.
- [ ] **scopeguard for `cas.rs` inflight-leak** (`rio-store/src/cas.rs:564`) — wrap `inflight.insert` + remove in a scopeguard so a cancelled `get` future doesn't leak the in-flight marker. Theoretical today (the sustained-cancellation scenario hasn't been observed), but cheap to close.
- [ ] **cgroup bind-remount test** (`rio-worker/src/cgroup.rs:301` TODO) — the rw-remount path is unreachable under `privileged: true` (containerd mounts rw already). Add a unit test that exercises it directly, or defer to a future non-privileged + device-plugin VM test (ADR-012). Low priority.
- [ ] **Doc sync** — sweep all `> **Phase 4 deferral:**` blocks and update or remove:
  - `errors.md:75` — poison PG persistence IS done (migration 004 `failed_workers TEXT[]`). Only `poisoned_at` was in-memory until 4a.
  - `errors.md:87,97` — `ClearPoison` + per-build timeout implemented.
  - `multi-tenancy.md:70` — "GC unimplemented" is stale (GC shipped in 3b). Per-tenant policy ships in 4b.
  - `multi-tenancy.md:111,114` — tenant propagation ships in 4a, not "deferred".
  - `observability.md:116,166,269` — remove `(Phase 4+)` markers on `class_load_fraction`, `gc_runs_total`, `tenant_id` span field.
  - `components/scheduler.md:37,123,156` — remove `CutoffRebalancer`/`WorkerPoolSet` deferral blocks.
  - `components/scheduler.md:384,392,398,413` — remove "deferred to Phase 4" on tenant FK + index.
  - `components/controller.md:30,43,100` — remove WPS deferral, update `BuildStatus` conditions block.
  - `components/worker.md:190,194` — references NOT empty (4b NAR scanner). Atomic multi-output stays deferred to Phase 5.
  - `capacity-planning.md:64` — `WorkerPoolSet` CRD implemented.
  - `decisions/015-size-class-routing.md:54-58` — update implementation status.
  - `challenges.md:73,100,138` — FUSE circuit breaker + WPS shipped. Staggered scheduling stays deferred.
  - `failure-modes.md:52` — FUSE timeout + breaker implemented.
  - `verification.md:10,83,94` — criterion benches wired (chunk 10). Multi-version Nix matrix + `cargo-mutants` + chaos harness stay deferred.
  - `security.md:53,173,175` — custom seccomp implemented.
- [ ] Update `docs/src/contributing.md:85` to reference `phase4c` / `TODO(phase5)`.
- [ ] Add tracey `r[...]` markers for new behaviors: `sched.tenant.resolve`, `sched.rebalancer.sita-e`, `worker.refs.nar-scan`, `ctrl.wps.reconcile`, `ctrl.wps.autoscale`, `worker.circuit.fuse`, `worker.silence.kill`.
- [ ] Mark all phase4 tasks `[x]` across `phase4a.md`/`phase4b.md`/`phase4c.md`.

## Carried-forward TODOs (resolved)

| Location | What | Resolution |
|---|---|---|
| `rio-controller/src/scaling.rs:195` | "WorkerPoolSet wires scaling" | Per-class autoscaler via `GetSizeClassStatus` |
| `rio-store/src/cas.rs:564` | scopeguard for inflight leak | Wrapped |
| `rio-worker/src/cgroup.rs:301` | rw-remount path unreachable | Unit test or documented as device-plugin-dependent |
| `docs/src/capacity-planning.md:64` | `WorkerPoolSet` CRD | Implemented |
| `docs/src/decisions/015-size-class-routing.md:54-58` | WPS + SITA-E deferred | Implemented |
| `docs/src/components/scheduler.md:156` | `CutoffRebalancer` deferred | Implemented |
| `docs/src/observability.md:116` | `class_load_fraction` Phase 4+ | Emitted |
| `docs/src/verification.md:94` | criterion benchmarks | `rio-bench` crate |

## Deferred out of Phase 4c

| Item | Reason | Target |
|---|---|---|
| `rio-scheduler/src/grpc/mod.rs:679` live preemption | Needs worker-side checkpoint + mid-build `ResourceUsage` | Phase 5 |
| `rio-store/src/grpc/chunk.rs:62` `PutChunk` RPC | No client-side chunker; refcount policy for standalone chunks undefined | Phase 5 |
| Atomic multi-output registration (`worker.md:194`) | Cleaned up by next successful rebuild; complexity not yet justified | Phase 5 |
| Staggered scheduling for cold workers (`challenges.md:73`) | Chunk cache + prefetch absorb most thundering-herd | Revisit on production data |
| Nix multi-version matrix (`verification.md:10`) | Separate CI infrastructure track | — |
| `cargo-mutants` (`verification.md:94`) | Separate CI infrastructure track | — |
| Chaos testing harness (`verification.md:83`) | User deferred; VM tests cover crash/reconnect/failover | Phase 5 |

## Milestone

`NIXBUILDNET_REUSE_BUILD_FAILURES=false nix-build-remote --no-nom -- -L .#ci-fast` passes with `vm-phase4` and `vm-phase4-fod` in the aggregate. `tracey query validate` shows 0 errors with the new markers. Grafana dashboards render against a live `vm-phase4` Prometheus. `nix run .#bench` reports SubmitBuild p99 < 50ms at N=100.
