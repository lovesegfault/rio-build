# Phase 4c: Adaptive Scheduling + Validation + Polish (Months 20-22)

**Goal:** Activate SITA-E adaptive size-class cutoffs, ship the `WorkerPoolSet` CRD with per-class autoscaling, close VM test gaps (PDB/NetPol/FOD proxy), deliver Grafana dashboards + load benchmarks, and sync all deferral blocks out of the design docs.

**Implements:** [ADR-015](../decisions/015-size-class-routing.md) (`CutoffRebalancer` + `WorkerPoolSet` CRD), [Capacity Planning](../capacity-planning.md) (declarative size-class pools), [Verification](../verification.md) (criterion benchmarks + VM test matrix)

> **Depends on Phase 4a/4b:** `CutoffRebalancer` writes to `build_samples` (needs migration 009 chain). `GetSizeClassStatus` RPC follows the `ClusterSnapshot` ActorCommand pattern from 4a's `ListWorkers`. `rio-cli cutoffs`/`wps` subcommands extend the 4b `rio-cli` crate. VM test sections A–E landed across `scenarios/{security,lifecycle,scheduling,cli}.nix` per the section map in `phase4.md`; 4c adds Sections F–J to the same files plus new `scenarios/{netpol,fod-proxy}.nix`.

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
- [ ] RBAC: add `workerpoolsets` resource to `infra/helm/rio-build/templates/rbac.yaml`. Regenerate CRDs: run `nix build .#crds && ./scripts/split-crds.sh result`.
- [ ] **Extend 4b `rio-cli` crate** with new subcommands: `cutoffs` (calls `GetSizeClassStatus`, prints table of `name | configured_cutoff | effective_cutoff | queued | running | samples`) and `wps` (kubectl-style `get`/`describe` via kube-rs, shows `WorkerPoolSet` CRs + child `WorkerPool` status). New `src/cutoffs.rs` + `src/wps.rs`, wire into `main.rs` clap dispatch.

### VM tests (D6: E + D in scope)

- [ ] **PDB assertion** — add to `nix/tests/scenarios/lifecycle.nix` (~25 lines, after the existing `autoscaler` subtest). Assert controller-managed PDB `{pool}-pdb` exists with `maxUnavailable: 1` + ownerRef points to `WorkerPool`. Delete `WorkerPool` → PDB GC'd via ownerRef. The `k3s-full` fixture already has `podDisruptionBudget.enabled=true` in `values/vmtest-full.yaml`.
- [ ] **Pre-verify kube-router NetPol enforcement** before committing to the Section G design: run `nix-build-remote --dev --no-nom -- -L .#checks.x86_64-linux.vm-lifecycle-k3s.driverInteractive`, apply a deny-all egress `NetworkPolicy`, check `kubectl exec {worker-pod} curl 1.1.1.1` blocked. If kube-router **does** enforce → Section G uses stock k3s. If **not** → Section G preloads `docker.io/calico/node` + `calico/cni` images (add to `nix/docker-pulled.nix`) and runs k3s with `--flannel-backend=none --disable-network-policy` (new `k3s-full` fixture param).
- [ ] **Add cases to scenario files** (see section map in `phase4.md`):
  - Section F (cancel timing) → `scenarios/scheduling.nix`. Submit a slow build, `CancelBuild` mid-run, assert `rio_scheduler_cancel_signals_total` increments AND worker's cgroup is gone within 5s.
  - Section G (NetPol egress) → new `scenarios/netpol.nix` on `k3s-full` with `networkPolicy.enabled=true` in `extraValues`. Design per pre-verify result above. `kubectl exec {worker-pod} curl 169.254.169.254` → blocked, `curl 1.1.1.1:80` → blocked. **NetPol ingress filtering SKIPPED** for now — k3s-full has everything in-cluster so `podSelector` works, but testing ingress properly needs a hostile pod. Phase 5 deferral.
  - Section H (`WorkerPoolSet`) → `scenarios/lifecycle.nix`. Apply WPS CR → controller creates 3 child `WorkerPool` CRs with correct `sizeClass` + ownerRef → delete WPS → children cascaded via ownerRef GC.
  - Section I (security) → `scenarios/security.nix`. (i) binary cache auth: unauthenticated `curl /{storePathHash}.narinfo` → 401, with Bearer token from `tenants` seed → 200; (ii) mTLS rejection: gRPC `QueryPathInfo` with no client cert → TLS handshake fails (already partly covered by `mtls-reject` subtest — extend it).
  - Section J (load) → `scenarios/scheduling.nix`. 50-derivation DAG, assert completion + `rio_scheduler_derivations_completed_total >= 50`.
- [ ] **FOD-proxy test** → new `scenarios/fod-proxy.nix` on `k3s-full` with `fodProxy.enabled=true`. Squid as a k8s pod (tests the production `fod-proxy.yaml` template directly). Local HTTP origin (busybox httpd pod) for airgap-safe fetch — same pattern as `drvs.coldBootstrapServer`. ConfigMap patch adds the local origin to the Squid allowlist. Assertions: FOD fetching allowlisted hostname → success + Squid access log shows the request; FOD fetching non-allowlisted → `TCP_DENIED/403` in Squid log; non-FOD derivation → no `http_proxy` env set. New `dockerImages.squid` in `nix/docker.nix` (~25 lines).
- [ ] Register `vm-netpol-k3s` + `vm-fod-proxy-k3s` in `nix/tests/default.nix`. Add to `ci-slow` (slow k3s tests).

### Grafana + benchmarks

- [ ] **Grafana dashboard JSONs** (`infra/helm/grafana/`): Build Overview (active builds, completion rate, p50/p99 duration), Worker Utilization (per-class replicas, CPU/memory gauges, queue depth), Store Health (cache hit rate, GC sweep size, S3 latency), Scheduler (dispatch latency, ready-queue depth, backstop timeouts, cancel signals).
- [ ] **`rio-bench` crate** — `criterion` (add to `[workspace.dependencies]` as a dev-dep) benches. Synthetic DAG generators: linear(N), binary-tree(depth), shared-diamond(N, shared_fraction). Benches: SubmitBuild latency at N∈{1,10,100,1000}; dispatch throughput (derivations/sec through a mocked worker channel). Add `rio-bench` to `flake.nix` packages + a `nix run .#bench` alias.
- [ ] Hydra comparison: documented manual procedure in `docs/src/capacity-planning.md` — equivalent hardware, same nixpkgs revision, wall-clock + CPU-seconds.

### Small deferrals + doc sync

- [ ] **Custom seccomp profile** (`infra/helm/rio-build/files/seccomp-rio-worker.json`) — `Localhost` profile denying `ptrace`, `bpf`, `setns`, `process_vm_readv`, `process_vm_writev` under `CAP_SYS_ADMIN`. Add `seccomp_profile: Option<SeccompProfileKind>` to `WorkerPoolSpec` (default `RuntimeDefault`, option for `Localhost` with path). Closes `security.md:53,173,175` Phase 4 tracking.
- [ ] **`BuildStatus` fine-grained conditions** — wire `Scheduled`/`InputsResolved`/`Building` conditions in the Build reconciler's `drain_stream` event handler (`controller.md:30,43` deferral). `criticalPathRemaining` + `workers` fields in `BuildStatus` require a `BuildEvent` proto extension — include if straightforward, else defer the fields (not the conditions) to Phase 5.
- [ ] **CN-list config + SAN check for HMAC bypass** (`rio-store/src/grpc/put_path.rs:65-80`) — currently only `CN=rio-gateway` bypasses (Phase 3b X1 fix, hardcoded string compare). Add `hmac_bypass_cns: Vec<String>` to store config (default `["rio-gateway"]`). Extend the cert-parse block to also check `subjectAltName` (OID 2.5.29.17) DNS entries alongside CN — `x509-parser`'s `TbsCertificate::subject_alternative_name()` returns `Option<&ParsedExtension>`; match `GeneralName::DNSName` variants against the allowlist. Allows multi-gateway deployments with distinct certs.
- [ ] **scopeguard for `cas.rs` inflight-leak** (`rio-store/src/cas.rs:564`) — wrap `inflight.insert` + remove in a scopeguard so a cancelled `get` future doesn't leak the in-flight marker. Theoretical today (the sustained-cancellation scenario hasn't been observed), but cheap to close.
- [ ] **cgroup bind-remount test** (`rio-worker/src/cgroup.rs:301` TODO) — the rw-remount path is unreachable under `privileged: true` (containerd mounts rw already). Add a unit test that exercises it directly, or defer to a future non-privileged + device-plugin VM test (ADR-012). Low priority.
- [ ] **`__noChroot` gateway pre-check** — `verification.md:69` lists this as Phase 4/5. Parse the derivation's env for `__noChroot = "1"` at `wopBuildDerivation`/`wopBuildPathsWithResults` time and reject with `STDERR_ERROR` before forwarding to the scheduler. nix-daemon's sandbox already enforces this at build time, but gateway-level rejection gives a faster, clearer error. Small addition to `rio-gateway/src/handler/opcodes_build.rs` — check `drv.env().get("__noChroot")` after ATerm parse.
- [ ] **Cross-tenant AdminService isolation test** — `verification.md:63` lists "tenant A cannot query tenant B's builds via AdminService" as a Phase 4 security test. Add a unit test in `rio-scheduler/src/admin/tests.rs`: seed two tenants + two builds, call `ListBuilds` with tenant A's UUID filter → only tenant A's build returned. The 4a `ListBuilds` task already has "tenant filter"; this adds the **verification** that the filter actually isolates.
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
  - `verification.md:68-73` — update the "Phase 4/5 deferral" block: `__noChroot` rejection implemented (this phase), mTLS rejection tested (Section I), FOD proxy tested (`phase4-fod.nix`), binary cache auth tested (Section I). Only JWT (Phase 5) remains.
  - `security.md:53,173,175` — custom seccomp implemented.
- [ ] Update `docs/src/contributing.md:85` to reference `phase4c` / `TODO(phase5)`.
- [ ] Add tracey `r[...]` markers + `r[impl]`/`r[verify]` annotations for **4c-specific** behaviors (4a/4b markers land in their own sub-phases): `sched.rebalancer.sita-e`, `sched.rebalancer.cpu-bump`, `ctrl.wps.reconcile`, `ctrl.wps.autoscale`, `ctrl.wps.cutoff-status`, `gw.reject.nochroot`, `store.hmac.san-bypass`, `worker.seccomp.localhost-profile`.
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
