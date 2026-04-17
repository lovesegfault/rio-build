# rio-controller

Manages rio-build lifecycle on Kubernetes via CRDs.

## CRDs

### Pool

r[ctrl.crd.pool]
```yaml
apiVersion: rio.build/v1alpha1
kind: Pool
metadata:
  name: x86-64
spec:
  kind: Builder                                # Builder | Fetcher, required
  maxConcurrent: 20                            # u32?, optional — concurrent-Job ceiling (one Job = one build); omit = uncapped (Karpenter NodePool limits.cpu is the only gate)
  deadlineSeconds: 3600                        # u32?, optional — Job activeDeadlineSeconds; default 3600 (Builder), 300 (Fetcher)
  image: rio-builder:dev                       # string, required — container image ref
  systems: [x86_64-linux]                      # list<string>, required (non-empty per CEL)
  hostUsers: false                             # bool?, optional — None ⇒ hostUsers:false (userns per ADR-012); CEL-forbidden for Fetcher
  nodeSelector:
    rio.build/builder: "true"
  tolerations:
    - { key: rio.build/builder, operator: Equal, value: "true", effect: NoSchedule }
  features: [big-parallel, kvm]                # list<string>, default [] — maps to requiredSystemFeatures (Builder-only)
  imagePullPolicy: IfNotPresent                # string?, optional — K8s default if omitted
  fuseThreads: 4                               # u32?, optional — Builder-only, CEL-forbidden for Fetcher
  fusePassthrough: true                        # bool?, optional — Builder-only, CEL-forbidden for Fetcher
  daemonTimeoutSecs: 7200                      # u64?, optional — Builder-only, CEL-forbidden for Fetcher
  terminationGracePeriodSeconds: 7200          # i64?, optional — K8s grace; defaults per-kind (r[ctrl.pod.tgps-default])
  privileged: false                            # bool?, optional — CEL-forbidden for Fetcher
  hostNetwork: false                           # bool?, optional — true requires privileged:true; CEL-forbidden for Fetcher
  seccompProfile:                              # SeccompProfileKind?, optional — CEL-forbidden for Fetcher
    type: Localhost                            #   RuntimeDefault | Localhost | Unconfined
    localhostProfile: operator/rio-builder.json#   required iff type=Localhost (r[ctrl.crd.seccomp-cel])
  # NOT CRD fields: resources (per-pod cpu/mem/disk come from the
  # scheduler's per-drv SpawnIntent — ADR-023); securityContext (caps
  # hardcoded in build_executor_pod_spec()); fuseCacheSize (hardcoded
  # per-kind); topologySpread (one-shot Jobs don't anti-affine).
status:
  replicas: 5                                  # i32 — active Jobs
  readyReplicas: 4                             # i32 — Jobs whose pod passed readinessProbe
  desiredReplicas: 8                           # i32 — concurrent-Job target the reconciler is converging on
  conditions:                                  # []Condition — see r[ctrl.condition.sched-unreachable]
    - type: SchedulerUnreachable
      status: "False"
      reason: ClusterStatusOK
```

### Job lifecycle

r[ctrl.pool.ephemeral]
The reconciler polls `AdminService.ClusterStatus` each requeue tick (10s)
and spawns K8s Jobs when `queued_derivations > 0` and active Jobs <
`spec.maxConcurrent` (or unconditionally when `maxConcurrent` is unset).
Each Job runs one rio-builder pod whose main loop exits after one
`CompletionReport` → pod terminates → Job goes Complete →
`ttlSecondsAfterFinished: 600` reaps (10min postmortem window for `kubectl
logs` on failed builders). Job settings: `backoffLimit: 0` (scheduler owns
retry), `restartPolicy: Never`, `parallelism: 1`. `spec.maxConcurrent` is
an optional concurrent-Job ceiling, not a standing set; when omitted, the
Karpenter NodePool `limits.cpu` is the only fanout gate. Zero queued
derivations means zero pods.

**Isolation guarantee:** zero cross-build state. Fresh pod means fresh
emptyDir for FUSE cache and overlayfs upper, fresh filesystem. Untrusted
tenants cannot leave poisoned cache entries for subsequent builds — there
is no "subsequent build" on that pod. Strongest isolation when combined
with `hostUsers: false` + non-privileged (see `docs/src/security.md`
§ Ephemeral Builders).

**Cost:** per-build cold start (pod scheduling + container pull + FUSE
mount + scheduler registration — typically 10–30s) plus one reconcile
tick (~10s) before the Job is spawned. Nodes outlive pods (Karpenter
consolidation policy), so the node-level FSx cache survives pod churn —
the cold-start cost is pod overhead, not refetching the closure.

**Dispatch path:** the scheduler sees a Job pod heartbeat in, dispatches
one derivation, receives `CompletionReport`, then the pod disconnects.
The active mechanism is ClusterStatus polling; a push-mode RPC was
considered and rejected (see `ephemeral.rs` § Why not a
Scheduler→Controller RPC).

**RBAC:** the controller's ClusterRole grants `batch/jobs` verbs
`[get, list, watch, create, delete]`. `delete` is required for the
excess-Pending reap (`r[ctrl.ephemeral.reap-excess-pending]`) and the
orphan-Running reap. `ttlSecondsAfterFinished` reaps Completed/Failed
Jobs; ownerRef GC handles pool-delete cleanup.

r[ctrl.ephemeral.reap-excess-pending]
When the per-class queued count drops below the count of Pending-phase
Jobs for that class, the controller MUST delete the excess Pending Jobs
(oldest first). Running Jobs are not touched — those have or may
receive assignments; the scheduler handles them via
cancel-on-disconnect. "Pending" is `JobStatus.ready == 0` with
`parallelism: 1` and no readiness probe: the pod has not been
scheduled, or is scheduled but the container has not started — either
way it has never connected to the scheduler and never received an
assignment, so deletion loses no work. Jobs younger than one requeue
tick (10s) are excluded — `JobStatus.ready` is set asynchronously by
the K8s Job controller and can lag a freshly-started container. The
reap is **skipped entirely** when the queued poll failed (scheduler
unreachable): spawn treats that as `queued=0` (fail-open: don't
spawn); reap MUST treat it as unknown (fail-closed: don't delete).
Without this reap, a cancelled build leaves already-spawned Pending
Jobs sitting until `activeDeadlineSeconds` (default 1h), and Karpenter
keeps provisioning nodes for them.

r[ctrl.ephemeral.reap-orphan-running]
When a Running Job (`JobStatus.ready > 0`) is older than the orphan
grace (default 5min) AND the scheduler does not consider its executor
busy --- either the pod's `executor_id` is absent from `ListExecutors`,
or present with `busy == false` --- the controller MUST delete the
Job. This is the controller-side backstop for I-165: a builder process
stuck in uninterruptible sleep (D-state FUSE wait, OOM-loop) cannot
self-exit via the 120s `RIO_IDLE_SECS` idle-timeout, never disconnects
from the scheduler, and would otherwise sit until `activeDeadlineSeconds`
(default 1h). The grace MUST exceed the builder's idle-timeout so the
process-level exit is given first chance; the controller reap fires only
when the process cannot act on its own. A Job whose executor reports
`busy == true` is NOT reaped --- the scheduler believes a build is
in progress; `activeDeadlineSeconds` is the backstop for stuck-mid-build.
The reap is **skipped entirely** when `ListExecutors` fails (scheduler
unreachable) --- fail-closed, same posture as
`r[ctrl.ephemeral.reap-excess-pending]`.

**Cleanup:** the finalizer's `cleanup()` returns immediately. In-flight
Jobs finish their one build naturally; ownerRef GC removes them after
the pool is gone.

r[ctrl.pool.ephemeral-deadline]
Jobs MUST set `spec.activeDeadlineSeconds` from
`PoolSpec.deadlineSeconds` (default 3600). This is a backstop:
the spawn decision reads the **cluster-wide**
`ClusterStatus.queued_derivations`, not pool-matching depth. A queue
full of `x86_64-linux` work on an `aarch64-darwin` pool triggers a Job
spawn; the spawned executor heartbeats in, never matches dispatch (wrong
`system`), and would hang indefinitely without a deadline. K8s kills
the pod at deadline, `backoffLimit: 0` marks the Job Failed,
`ttlSecondsAfterFinished` reaps.

r[ctrl.ephemeral.intent-deadline]
When a `SpawnIntent` carries `deadline_secs > 0`, the Job's
`activeDeadlineSeconds` is set to that value verbatim. Precedence:
`intent.deadline_secs` > `spec.deadlineSeconds` > 3600 default. The
scheduler computes `deadline_secs` per-derivation (D7): for fitted
keys, `wall_p99 * 5` at the solved core count; for unfitted
(probe/explore), `[sla].probe.deadline_secs`; both clamped
`[resource_floor.deadline_secs, 86400]`. A `DeadlineExceeded` kill
triggers `r[ctrl.terminated.deadline-exceeded]` -> `bump_floor_or_count`
doubles `floor.deadline_secs` -> the next intent gets a longer
`activeDeadlineSeconds`. The 5x headroom is scheduler-side; the
controller adds no multiplier or margin.

r[ctrl.terminated.deadline-exceeded]
The Job-mode reconciler MUST report each Job with `status.conditions`
containing `type=Failed, reason=DeadlineExceeded` to the scheduler via
`AdminService.ReportExecutorTermination{executor_id = job.metadata.name,
reason = DeadlineExceeded}`. The Job controller deletes the Pod when
`activeDeadlineSeconds` fires, so the `report_terminated_pods` Pod-
status scan never sees a terminated container; the Job condition is
observable for `JOB_TTL_SECS=600` (~60 reconcile ticks). The scheduler
prefix-matches the Job name against its `recently_disconnected` map
(`r[sched.termination.deadline-exceeded]`). Iterates the already-listed
`jobs.items` --- no extra apiserver call. Best-effort (RPC error
logged, reconcile continues). Defense-in-depth behind the worker-side
`daemon_timeout` -> `TimedOut` primary path.

r[ctrl.pod.arch-selector]
When the pool's `spec.systems` resolves to a single *host* CPU
architecture, the controller MUST inject
`kubernetes.io/arch={amd64|arm64}` into the Job pod's `nodeSelector`
(operator-set value wins via `or_insert`). 32-bit guest systems map to
their 64-bit host (`i686`→`amd64`, `armv7l`/`armv6l`→`arm64`) so an
`extra-platforms` pool like `[x86_64-linux, i686-linux]` still
constrains to amd64. Builder-only: fetchers run `builtin`
(arch-agnostic) and benefit from cheaper nodes. Without this, an
`x86_64-linux` pool can land on an arm64 node (unconstrained fallback
NodePool — I-098), register as `x86_64` from `RIO_SYSTEMS`, accept
dispatch, and have the local nix-daemon refuse the build. Multi-arch
and `builtin`-only pools
get no selector and rely on the executor's startup arch check as the
safety net.

r[ctrl.pod.tgps-default]
The Job pod spec MUST default `terminationGracePeriodSeconds` to
`7200` (2h) when `PoolSpec.terminationGracePeriodSeconds` is
unset. SIGTERM → executor drain (`r[ctrl.drain.sigterm]`) waits for
the in-flight build to complete before exit; nix builds can
legitimately take 2h (LLVM, full NixOS closure from cold cache).
Clusters with known-shorter builds set this lower so pool deletion
doesn't stall on a stuck pod for 2h.

r[ctrl.pool.kvm-device]
When `PoolSpec.features` contains `"kvm"`, the controller MUST
append `rio.build/kvm: "true"` to the pod's `nodeSelector` AND append a
`rio.build/kvm=true:NoSchedule` toleration. containerd `base_runtime_spec`
injects `/dev/kvm` into every pod's `/dev` (same mechanism as
`r[sec.pod.fuse-device-plugin]`), but only EC2 `.metal` instance types
expose host KVM (nested virt) — on non-metal the device node ENXIOs on
open. The nodeSelector + toleration land kvm pods exclusively on the
metal NodePool while non-kvm pods stay on the cheaper general builder
NodePools. The nodeSelector + toleration are unconditional wrt
`privileged` so privileged kvm pods still land on metal.

### Reconciler

r[ctrl.pool.reconcile]
One reconciler handles both kinds. Each tick: poll `GetSpawnIntents{kind=spec.kind, systems, features}` → spawn one Job per intent (resources stamped from the intent) up to `spec.maxConcurrent` → reap excess Pending / orphan Running → patch `.status`. Finalizer-wrapped; ownerRef GC handles Job cleanup on Pool delete.

r[ctrl.pool.fetcher-hardening]
For `kind=Fetcher`, `executor_params` MUST apply ADR-019 hardening regardless of spec: `readOnlyRootFilesystem: true`, `seccompProfile: Localhost operator/rio-fetcher.json`, `hostUsers: false`, `privileged: false`, default `node-role: fetcher` selector + `rio.build/fetcher:NoSchedule` toleration, `terminationGracePeriodSeconds: 600`. CRD CEL rejects fetcher specs that set the overridden fields at admission time; the reconciler override is belt-and-suspenders for pre-CEL specs the apiserver already accepted.

r[ctrl.pool.fetcher-spawn-builtin]
For `kind=Fetcher` pools, `spec.systems` SHOULD include `"builtin"` so `system="builtin"` FODs are counted in the spawn signal. Every executor unconditionally advertises `"builtin"` (`r[sched.dispatch.fod-builtin-any-arch]`); omitting it from the spawn signal would stall a cold-store bootstrap.

r[ctrl.pool.disruption]
The DisruptionTarget watcher selects on `POOL_LABEL` (which the reconciler stamps on every kind=Builder AND kind=Fetcher pod), so fetcher pods gain the same fast-preemption behavior as builders: K8s sets `DisruptionTarget=True` → `DrainExecutor{force:true}` → reassign in seconds instead of burning the grace period.

### ComponentScaler

r[ctrl.crd.componentscaler]
```yaml
apiVersion: rio.build/v1alpha1
kind: ComponentScaler
metadata:
  name: rio-store
spec:
  targetRef:                         # required
    kind: Deployment                 #   CEL: must be "Deployment"
    name: rio-store                  #   same-namespace
  signal: scheduler-builders         # scheduler-builders (default)
  replicas:                          # required
    min: 2                           #   i32; CEL: min <= max
    max: 14                          #   i32 — for store: Aurora max_connections / pgMaxConnections
  seedRatio: 50.0                    # f64, default 50.0 — initial builders_per_replica
  loadEndpoint: rio-store-headless.rio-store:9002   # string, required — headless Service for GetLoad polling
  loadThresholds:
    high: 0.8                        # f64, default 0.8 — CEL: 0.0 < low < high <= 1.0
    low: 0.3                         # f64, default 0.3
status:
  learnedRatio: 67.3                 # f64? — EMA-adjusted; persists across controller restart
  observedLoadFactor: 0.42           # f64? — max(GetLoad.pg_pool_utilization) at last tick
  desiredReplicas: 5                 # i32 — last value patched onto deployments/scale
  lastScaleUpTime: "2026-04-08T..."  # Time? — 5min scale-down stabilization window starts here
  lowLoadTicks: 12                   # u32 — consecutive ticks with load<low (mirrored; authoritative counter in-process)
```

Why not k8s HPA: no metrics-server / custom.metrics.k8s.io adapter
in-cluster, and the controller already has the demand signal
(`GetSpawnIntents`). See `r[ctrl.scaler.component]` /
`r[ctrl.scaler.ratio-learn]` for reconciler behavior.

## Reconciliation Loops

r[ctrl.reconcile.owner-refs]
- **Pool reconciler**: spawn/reap one-shot Jobs (builder or fetcher per `spec.kind`) based on scheduler `SpawnIntent`s. All Jobs carry `ownerReferences` to the Pool CRD with `controller: true`, ensuring garbage collection on Pool deletion.
- **GC reconciler**: trigger store garbage collection on schedule.

r[ctrl.backoff.per-object]
`error_policy` requeues transient reconcile errors with per-object
exponential backoff: `5s × 2^(n-1)` capped at `300s`, keyed by
`{kind}/{ns}/{name}`. A persistent apiserver 5xx backs off to the cap
in ~6 rounds (5→10→20→40→80→160→300s) instead of retrying every 30s
indefinitely. The counter resets on the next successful reconcile so a
fresh failure restarts the curve from 5s. `Error::InvalidSpec` is NOT
on the curve — it requeues at a fixed per-reconciler interval (300s
for pools, 30s for ComponentScaler where 5min of no scaling under a
builder burst is the I-105 cliff).

r[ctrl.condition.sched-unreachable]
`Pool.status.conditions[]` MUST carry a
`SchedulerUnreachable` condition reflecting the reconciler's poll-phase
RPC result: `status="True", reason="ClusterStatusFailed"` with the gRPC
error in `message` when the poll failed; `status="False",
reason="ClusterStatusOK"` otherwise. Written every reconcile (SSA with
the `rio-controller-ephemeral` field manager owns the condition —
omitting it would leave a stale `True` after recovery).
`lastTransitionTime` is preserved across same-status writes so
operators see when the scheduler actually went down, not "~10s ago".
Without this, `replicas=0` is indistinguishable between "scheduler
idle, queued=0" and "scheduler down, queued unknown".

## RBAC

The controller requires a dedicated ServiceAccount with a ClusterRole granting (see `infra/helm/rio-build/templates/rbac.yaml`):

| API Group | Resources | Verbs |
|---|---|---|
| `rio.build` | builderpools, fetcherpools | get, list, watch, create, update, patch, delete |
| `rio.build` | builderpoolsets, componentscalers | get, list, watch, patch, update |
| `rio.build` | {builderpools,builderpoolsets,fetcherpools,componentscalers}/status | get, patch[, update] |
| `rio.build` | {builderpools,builderpoolsets,fetcherpools}/finalizers | update — `OwnerReferencesPermissionEnforcement` checks this when creating children with `blockOwnerDeletion: true` |
| `apps` | deployments | get, list, watch — ComponentScaler reads current `.spec.replicas` |
| `apps` | deployments/scale | get, patch, update — ComponentScaler `/scale` subresource patch |
| `""` (core) | pods | get, list, watch |
| `events.k8s.io` | events | create, patch |
| `batch` | jobs | get, list, watch, create, delete |

Lease permissions (`coordination.k8s.io/leases`: get, create, update) are granted to the **scheduler's** ServiceAccount via a namespaced Role, not the controller (the controller has no leader election).

> **Note:** The controller does NOT hold permissions for `NetworkPolicies`, `ConfigMaps`, or `Leases`. NetworkPolicies are deployed as static manifests via the Helm chart (see below).

## NetworkPolicy

NetworkPolicy resources are deployed via the Helm chart (`infra/helm/rio-build/templates/networkpolicy.yaml`, gated on `networkPolicy.enabled`), not controller-managed. The controller has no `networking.k8s.io` RBAC permissions. Intended policies:

- **Executors**: egress to rio-scheduler and rio-store only (gRPC ports). No access to the Kubernetes API server or cloud metadata service (`169.254.169.254`). DNS egress to kube-system (CoreDNS) required for service resolution.
- **Gateway**: ingress from external (Service type LoadBalancer/NodePort for SSH). Egress to rio-scheduler and rio-store. DNS egress to kube-system.
- **Scheduler**: egress to PostgreSQL. DNS egress to kube-system.
- **Store**: egress to PostgreSQL and S3. DNS egress to kube-system.
- **Controller**: egress to rio-scheduler (gRPC, for `AdminService.ClusterStatus`/`GetSpawnIntents` queue-depth queries) and to the Kubernetes API server (for CRD watches and Job management). DNS egress to kube-system.

## PodDisruptionBudget

| Component | Managed by | Policy | Rationale |
|---|---|---|---|
| Scheduler | Static manifest | `maxUnavailable: 1` | Leader election handles failover; at most one pod unavailable |
| Gateway | Static manifest | `minAvailable: 1` | At least one pod must remain for SSH connectivity |

Scheduler and gateway PDBs are static manifests in the Helm chart (`infra/helm/rio-build/templates/pdb.yaml`, gated on `podDisruptionBudget.enabled`). Executor pods are one-shot Jobs — a PDB on Jobs is meaningless (eviction of a Job pod just reschedules the build via `r[ctrl.drain.disruption-target]`).

## Service Definitions

| Service | Type | Purpose |
|---|---|---|
| `rio-gateway` | LoadBalancer or NodePort | SSH ingress for `nix copy` / `nix build --store ssh://` |
| `rio-scheduler` | ClusterIP | Internal gRPC for executors, gateway, and controller |
| `rio-store` | ClusterIP | gRPC for internal components |

## Health Probes

r[ctrl.probe.named-service]
Health probes against `grpc.health.v1.Health/Check` MUST target a named service (e.g., `rio.scheduler.SchedulerService`), not the empty-string default. `set_not_serving` only affects named services, not `""` --- a probe on `""` stays green through drain and through standby.

> **Note:** This requirement applies to the **client-side balancer** (`rio-proto/src/client/balance.rs`), which probes the named service to find the leader. It does NOT apply to K8s probes: `infra/helm/rio-build/templates/scheduler.yaml` intentionally uses `tcpSocket` for both readiness and liveness --- standby replicas report `NOT_SERVING` on the gRPC health endpoint (they haven't won the lease), so gRPC-based K8s probes would crash-loop them. TCP-accept succeeding proves the process is live; leader-election is client-side routing's concern, not K8s'. `store.yaml` does use `grpc.service: rio.store.StoreService` for readiness (store is not leader-elected, so `NOT_SERVING` only means drain or PG/S3 unhealthy --- correct to take out of rotation). Executor probes are HTTP (`/healthz`/`/readyz`) and are unrelated to this rule.

| Component | Liveness | Readiness | Startup |
|---|---|---|---|
| Gateway | TCP check on SSH port | After scheduler gRPC connection established | — |
| Scheduler | TCP socket (gRPC health is leader-election-aware, unsuitable for K8s probes on HA standby) | TCP socket — process is live, port is bound. Leader election happens; client-side balancer routes to leader via named-service health. | TCP socket. Startup budget sized for state recovery (reload non-terminal builds from PG). |
| Store | TCP socket | gRPC health check on named service --- after PostgreSQL + S3 reachable | — |
| Controller | HTTP `/healthz` | After kube-client + scheduler gRPC connect (intentionally before CRD watches; single-replica, no leader election) | — |
| Executor | HTTP `/healthz` + `/readyz` (no gRPC server) | `/readyz` 200 after first accepted heartbeat | HTTP check, `failureThreshold × periodSeconds ≥ 120s` (FUSE mount + cache warm) |

## Executor Lifecycle

r[ctrl.drain.sigterm]
**Scale-down:** `terminationGracePeriodSeconds` is set to `7200` (2 hours) to allow in-flight builds to complete.

**SIGTERM drain (no preStop hook needed):** the executor's main loop has a `select!` arm on SIGTERM. On signal:

1. Set heartbeat `draining=true` (the executor-authoritative drain source — I-063); the scheduler's `has_capacity()` reads it via `is_draining()`.
2. Keep the `BuildExecution` stream connected (and reconnect across scheduler restart) until `BuildSlot::wait_idle()` returns — the in-flight build's completion is reported.
3. Send `AdminService.DrainExecutor` (best-effort exit deregister) and exit 0.

A preStop hook doing the same is redundant: K8s sends SIGTERM on pod termination regardless of preStop, and the executor's signal handler implements the drain. The Job pod template does NOT define a preStop.

r[ctrl.drain.disruption-target]
**Eviction-triggered preemption:** the controller runs a Pod watcher filtered to `rio.build/pool`-labeled pods with `status.conditions[type=DisruptionTarget,status=True]`. When K8s marks a pod for eviction (node drain, spot interrupt), the watcher calls `AdminService.DrainExecutor{force:true}` — the scheduler sends `CancelSignal` for the in-flight build → executor `cgroup.kill()`s → the build reassigns to a healthy executor within seconds. Without this, the evicting pod would self-drain with `force=false` and wait up to `terminationGracePeriodSeconds` (2h) for the in-flight build to complete naturally before SIGKILL loses it anyway. The SIGTERM self-drain path (above) is the fallback if the watcher misses the window.

## ComponentScaler

r[ctrl.scaler.component]
The controller reconciles `ComponentScaler` CRs into `apps/v1 Deployment {targetRef} /scale` patches. `desired_replicas = clamp(ceil(Σ(queued+running) / status.learnedRatio), spec.replicas.min, spec.replicas.max)` where `Σ(queued+running)` comes from `AdminService.GetSpawnIntents` (the **predictive** signal — scheduler knows N builders are about to exist before they exist; store scales ahead of the burst). Scale-down is held for 5 minutes after the last scale-up and limited to −1/tick. Reconcile interval: 10s.

r[ctrl.scaler.ratio-learn]
`status.learnedRatio` self-calibrates against `max(StoreAdminService.GetLoad().pg_pool_utilization)` over the `spec.loadEndpoint` headless-service endpoints (the **observed** signal). Asymmetric correction: `load > spec.loadThresholds.high` (default 0.8) → immediate `current+1` AND `learnedRatio *= 0.95` (under-provisioning is dangerous — I-105 cascade); `load < spec.loadThresholds.low` (default 0.3) for 30 consecutive ticks → `learnedRatio *= 1.02` (over-provisioning is cheap). The ratio persists in `.status` so a controller restart resumes from the learned value, not `spec.seedRatio`.

r[store.admin.get-load]
`StoreAdminService.GetLoad` returns `pg_pool_utilization = (pool.size − pool.num_idle) / max_connections` for the replica it's called on. The ComponentScaler reconciler polls every store pod (DNS-resolving the headless service) and uses the max as `observedLoadFactor`. The handler also publishes `rio_store_pg_pool_utilization` so Prometheus sees the same value the controller acted on.

When `componentScaler.store.enabled=true`, the helm chart MUST omit `Deployment.spec.replicas` from the rendered store template — otherwise `helm upgrade` resets the replica count and fights the controller. The controller's `/scale` patches use field-manager `rio-controller-componentscaler` (distinct from helm's apply manager).

## GC Cron

r[ctrl.gc.startup-delay]
The GC cron's FIRST tick MUST be delayed by `STARTUP_DELAY` (300 s) plus 0–60 s jitter from controller start. Firing at t≈0 collides GC mark with post-deploy validation traffic — every helm rollout caused `nix copy` to exhaust the gateway's `Aborted` retry budget (I-168). 5 minutes clears the deploy-then-stress window while keeping the "controller restart doesn't delay GC by 24 h" property.

r[ctrl.gc.cron-schedule]
Controller runs a GC cron reconciler: `tokio::select!` on
`shutdown.cancelled()` vs `interval.tick()` (default 24h, configurable
via `controller.toml gc_interval_hours`; 0 = disabled). Each tick:
connect to store-admin with `tokio::time::timeout(30s, ...)` — on
connect failure, `warn!` + increment
`rio_controller_gc_runs_total{result="connect_failure"}` + `continue`
(NEVER `?`-propagate out of the loop — tonic has no default connect
timeout and a stale IP hangs on SYN). On success: `TriggerGC`, drain
the `GcProgress` stream, increment `{result="success"}`. Implementation
at `rio-controller/src/reconcilers/gc_schedule.rs`; wired via
`spawn_monitored("gc-cron", ...)` gated on `gc_interval_hours > 0`.
No leader-gate: controller is single-replica by design; `replicas>1`
misconfig is serialized by the store's `GC_LOCK_ID` advisory lock.

## NodePool Shared Budget

r[ctrl.nodepoolbudget]
When `RIO_NODEPOOL_BUDGET__CPU_MILLICORES > 0`, the controller runs a
NodePool budget reconciler: every 30s, list `karpenter.sh/v1` NodePools
matching `RIO_NODEPOOL_BUDGET__SELECTOR` (default `rio.build/karpenter-
budget=shared`), read each `status.resources.cpu`, compute `headroom =
budget − Σused`, and merge-patch each `spec.limits.cpu = used +
headroom`. This gives the governed pools a shared vCPU budget — any pool
can absorb a burst up to the aggregate. Freeze-on-exhaustion: when
`Σused ≥ budget`, headroom is 0 and each limit equals current usage
(Karpenter stops provisioning; never `limit < used` so no forced
scale-down). Fills the gap from kubernetes-sigs/karpenter#1747 (no
native cross-pool limit). Metrics: `rio_controller_nodepool_budget_
{used,headroom}_millicores`. Implementation at `rio-controller/src/
reconcilers/nodepoolbudget.rs`; wired via `spawn_monitored
("nodepool-budget", ...)` gated on `cpu_millicores > 0`.

## Build CRD (removed)

The `Build` CRD (`rio.build/v1alpha1 Build`) was removed in P0294. It was an
alternative K8s-native build submission path that duplicated the SSH
(`ssh-ng://`) flow. No known production users.

**Cluster upgrade:** existing `Build` CRs on running clusters are orphans
after controller upgrade (the reconciler no longer watches them). They can be
safely deleted: `kubectl delete builds.rio.build --all -A`. The CRD itself
remains installed (helm `crds/` directory is install-only, not upgrade-managed);
delete it manually: `kubectl delete crd builds.rio.build`.

## Component Deployment Model

The controller manages:
- **Pool** CRD → spawns/reaps one-shot builder/fetcher Jobs (per-intent resources, security context)

The controller does **NOT** manage:
- Scheduler or store Deployments --- these are deployed via Helm/kustomize as standard Deployments
- Rationale: scheduler and store have simple lifecycle (single replica or leader-elected); CRD management adds complexity without benefit

## Key Files

- `rio-crds/src/` --- CRD type definitions (separate crate; Pool, ComponentScaler)
- `rio-controller/src/reconcilers/` --- Reconciliation loops
- `rio-controller/src/scaling/` --- ComponentScaler (rio-store/rio-gateway Deployment scaling)

## CRD Versioning

CRDs follow a `v1alpha1` → `v1beta1` → `v1` progression. The initial implementation uses `v1alpha1` with no stability guarantees. Plan a conversion webhook before promoting to `v1beta1` to support zero-downtime upgrades from `v1alpha1`.

## CRD Validation

CRDs use CEL validation rules (`x-kubernetes-validations`) for structural constraints:

| CRD | Rule | Reject reason |
|---|---|---|
| Pool | `size(self.systems) > 0` | A pool with no target systems accepts no work. |
| Pool | `hostNetwork ⇒ privileged` | See `r[ctrl.crd.host-users-network-exclusive]`. |
| SeccompProfileKind | type ∈ allowed; `Localhost ⇔ has(localhostProfile)` | See `r[ctrl.crd.seccomp-cel]`. |
| ComponentScaler.Replicas | `self.min <= self.max` | Clamp range must be non-empty. |
| ComponentScaler.TargetRef | `self.kind == 'Deployment'` | Reconciler patches `apps/v1 deployments/scale` only. |
| ComponentScaler.LoadThresholds | `0.0 < low < high <= 1.0` | Threshold ordering for ratio correction. |

r[ctrl.crd.seccomp-cel]
`SeccompProfileKind` is a struct (`{type, localhostProfile?}`), not a Rust enum: kube-core's structural-schema rewriter rejects oneOf-variant subschemas with non-identical shared properties, so the type/localhostProfile coupling is enforced by CEL instead of the Rust type system. Two rules: `self.type in ['RuntimeDefault', 'Localhost', 'Unconfined']`, and `self.type == 'Localhost' ? has(self.localhostProfile) : !has(self.localhostProfile)`. The struct mirrors `pod.spec.securityContext.seccompProfile` exactly so operators can copy-paste; nested `KubeSchema` carries the rules through into the `Pool` schema.

r[ctrl.crd.host-users-network-exclusive]
The controller MUST reject `Pool` specs with `hostNetwork: true` and `privileged` unset or false. Kubernetes admission rejects pod specs combining `hostUsers: false` with `hostNetwork: true` (user-namespace UID remapping is incompatible with the host network namespace). Since the non-privileged path sets `hostUsers: false` unconditionally (ADR-012, `r[sec.pod.host-users-false]`), `hostNetwork: true` implies the `privileged: true` escape hatch. CRD CEL validation enforces this at `kubectl apply` time; the builder additionally suppresses `hostUsers` when the combination is encountered in pre-existing specs (emitting a Warning event).

r[ctrl.event.spec-degrade]
The Pool reconciler MUST emit a `Warning`-type Kubernetes Event for every spec field the builder silently degrades. CEL validation rejects NEW specs with invalid combinations; existing specs applied before the CEL rule landed are defensively corrected at pod-template time (e.g., `hostUsers` suppressed for `hostNetwork: true`). Without a Warning event, the operator has no signal that their spec is stale — `kubectl get pool -o yaml` shows the original value; the pod template shows the corrected value. The Warning names the field, the spec value, and the remediation.

There is no `spec.fuseCacheSize` CRD field — the FUSE cache emptyDir `sizeLimit` is the hardcoded `BUILDER_FUSE_CACHE` (50Gi) / `FETCHER_FUSE_CACHE` constant; pods are one-shot so the cache never outlives one build's input closure.

## Pool Finalizer

Pool CRDs carry a `pool.rio.build/drain` finalizer. The
finalizer's `cleanup()` removes the finalizer immediately; in-flight Jobs
finish their one build naturally and ownerRef GC removes them after the
pool is gone. The reconciler's `apply()` path short-circuits when
`deletionTimestamp` is set (finalizer wraps it) so no new Jobs are
spawned during deletion.
