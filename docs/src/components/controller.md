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
  terminationGracePeriodSeconds: 7200          # i64?, optional — K8s grace; defaults per-kind (r[ctrl.pod.tgps-default])
  privileged: false                            # bool?, optional — CEL-forbidden for Fetcher
  hostNetwork: false                           # bool?, optional — true requires privileged:true; CEL-forbidden for Fetcher
  seccompProfile:                              # SeccompProfileKind?, optional — CEL-forbidden for Fetcher
    type: Localhost                            #   RuntimeDefault | Localhost | Unconfined
    localhostProfile: operator/rio-builder.json#   required iff type=Localhost (r[ctrl.crd.seccomp-cel])
  fuseCacheBytes: 53687091200                  # u64?, optional — emptyDir sizeLimit + ephemeral-storage; per-kind default 8Gi/4Gi
  # NOT CRD fields: resources (per-pod cpu/mem/disk come from the
  # scheduler's per-drv SpawnIntent — ADR-023); securityContext (caps
  # hardcoded in build_executor_pod_spec()); topologySpread (one-shot
  # Jobs don't anti-affine).
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

r[ctrl.ephemeral.reap-excess-pending+3]
When the per-class queued count drops below the count of Pending-phase
Jobs for that class, the controller MUST delete the excess Pending Jobs
(orphan-by-intent first; residual excess oldest-first). Running Jobs are not touched — those have or may
receive assignments; the scheduler handles them via
cancel-on-disconnect. "Pending" is `JobStatus.ready == 0` with
`parallelism: 1` and no readiness probe: the pod has not been
scheduled, or is scheduled but the container has not started — either
way it has never connected to the scheduler and never received an
assignment, so deletion loses no work. Jobs younger than one requeue
tick (10s) are excluded — `JobStatus.ready` is set asynchronously by
the K8s Job controller and can lag a freshly-started container. Before
issuing each DELETE the controller MUST re-check `Pod.status.phase`
via a live (non-informer) lookup and skip the Job if any pod is
`Running`: after a Karpenter cold start the 10s creation-age grace is
already exhausted when the pod first runs, and the scheduler's `queued`
count drops on assignment before `JobStatus.ready` propagates. The
reap is **skipped entirely** when the queued poll failed (scheduler
unreachable): spawn treats that as `queued=0` (fail-open: don't
spawn); reap MUST treat it as unknown (fail-closed: don't delete).
Without this reap, a cancelled build leaves already-spawned Pending
Jobs sitting until `activeDeadlineSeconds` (default 1h), and Karpenter
keeps provisioning nodes for them.

r[ctrl.ephemeral.reap-orphan-running+3]
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
The reap is **skipped entirely** when `ListExecutors` fails, returns
empty, **or reports `leader_for_secs` below the orphan grace**
(scheduler unreachable, new-leader pre-reconnect, or partial-reconnect
window) --- fail-closed, same posture as
`r[ctrl.ephemeral.reap-excess-pending]`. The new leader's executor map
fills incrementally as workers reconnect over a 1--10s spread; a
non-empty partial list cannot prove absence.

**Cleanup:** the finalizer's `cleanup()` returns immediately. In-flight
Jobs finish their one build naturally; ownerRef GC removes them after
the pool is gone.

r[ctrl.ephemeral.intent-deadline]
Jobs MUST set `spec.activeDeadlineSeconds` to `SpawnIntent.deadline_secs`
verbatim (floored at 180 as defense against the proto default). The
scheduler computes `deadline_secs` per-derivation (D7): for fitted
keys, `wall_p99 * 5` at the solved core count; for unfitted
(probe/explore), `[sla].probe.deadline_secs`; both clamped
`[resource_floor.deadline_secs, 86400]`. `SlaConfig::validate`
enforces `probe.deadline_secs >= 180`, so the intent value is always
positive --- the controller has no per-kind fallback. A
`DeadlineExceeded` kill triggers `r[ctrl.terminated.deadline-exceeded]`
-> `bump_floor_or_count` doubles `floor.deadline_secs` -> the next
intent gets a longer `activeDeadlineSeconds`. The 5x headroom is
scheduler-side; the controller adds no multiplier or margin. Backstop
purpose: a wrong-pool spawn (executor heartbeats in, never matches
dispatch) would hang indefinitely without it; K8s kills at deadline,
`backoffLimit: 0` marks Failed, `ttlSecondsAfterFinished` reaps.

r[ctrl.terminated.deadline-exceeded+2]
The Job-mode reconciler MUST report each Job with `status.conditions`
containing `type=Failed, reason=DeadlineExceeded` to the scheduler via
`AdminService.ReportExecutorTermination{executor_id = job.metadata.name,
reason = DeadlineExceeded}`. With `restartPolicy:Never` +
`backoffLimit:0` + the `job-tracking` finalizer the SIGKILL'd Pod IS
listable with `terminated.reason="Error"` for the grace window, but
`Error` is non-promoting so `report_terminated_pods` skips it; the Job
condition is observable for `JOB_TTL_SECS=600` (~60 reconcile ticks). The scheduler
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
  observedLoadFactor: 0.42           # f64? — max of pg-pool and substitute-admission utilization at last tick
  desiredReplicas: 5                 # i32 — last value patched onto deployments/scale
  lastScaleUpTime: "2026-04-08T..."  # Time? — 5min scale-down stabilization window starts here
  lowLoadTicks: 12                   # u32 — consecutive ticks with load<low (mirrored; authoritative counter in-process)
```

Why not k8s HPA: no metrics-server / custom.metrics.k8s.io adapter
in-cluster, and the controller already has the demand signal
(`ClusterStatus`). See `r[ctrl.scaler.component+2]` /
`r[ctrl.scaler.ratio-learn]` for reconciler behavior.

## Reconciliation Loops

r[ctrl.admin.rpc-timeout]
Every `AdminServiceClient` RPC issued from a controller reconcile or
watcher loop MUST be bounded by a 5-second timeout. `build_endpoint`
sets `.connect_timeout()` only; h2 keepalive detects dead transport
(~40s) but not a live-but-stalled scheduler (actor mailbox backlog,
slow PG). A hung await would block the watcher loop indefinitely
(missed `DisruptionTarget` fast-preemption) or block the kube-runtime
reconciler with no requeue. On timeout the call site treats it as the
RPC's existing `Err` arm (best-effort: log + continue / requeue). The
bound is NOT applied at the channel level — builder/gateway data-plane
RPCs (long-poll, streaming) legitimately exceed 5s.

r[ctrl.pool.hw-class-annotation]
The `rio.build/hw-class` pod annotation MUST be exposed to the builder
via a downward-API **volume** (`/etc/rio/downward/hw-class`), not an
env var. The annotation is stamped reactively by `run_pod_annotator`
after `spec.nodeName` binds; the env-var form resolves once at
container-create and races the annotator permanently. The builder
reads the file with a bounded 30s poll (`hw_class::resolve`) so a
late stamp still reaches a running pod.

r[ctrl.pool.node-affinity-from-intent]
When `SpawnIntent.node_affinity` is non-empty the pool reconciler MUST
set `pod.spec.affinity.nodeAffinity.
requiredDuringSchedulingIgnoredDuringExecution.nodeSelectorTerms` to
the proto terms (field-by-field copy). Empty `node_affinity` MUST leave
`spec.affinity` unset so non-hw-targeted intents (Static-mode, FOD,
feature-gated) bin-pack freely (I-090).

r[ctrl.pool.hw-bench-needed+2]
The pool reconciler MUST stamp `rio.build/hw-bench-needed` on the pod
template at create time and expose it as `RIO_HW_BENCH_NEEDED` via a
downward-API env var. The annotation is `"true"` iff
`intent.mem_bytes ≥ hw_bench_mem_floor` AND any `h` in the intent's
admissible-set `A` (from `intent.hw_class_names`) has fewer than
`HwClassSampledResponse.trust_threshold` distinct tenants in any K=3
dimension per `AdminService.HwClassSampled` (one RPC per reconcile
tick over the union of all `A`). When `A` is empty the annotation MUST
be `"false"` — the actual `h` is unknown until kube-scheduler bind. On
RPC failure unknown `h` MUST read as 0 (over-bench, never
under-bench); on `trust_threshold` field absence (old-scheduler skew)
the controller MUST fall back to `5`.

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
| `rio.build` | pools | get, list, watch, create, update, patch, delete |
| `rio.build` | componentscalers | get, list, watch, patch, update |
| `rio.build` | pools/status | get, patch |
| `rio.build` | componentscalers/status | get, patch, update |
| `rio.build` | pools/finalizers | update — `OwnerReferencesPermissionEnforcement` checks this when creating children with `blockOwnerDeletion: true` |
| `apps` | deployments | get, list, watch — ComponentScaler reads current `.spec.replicas` |
| `apps` | deployments/scale | get, patch, update — ComponentScaler `/scale` subresource patch |
| `""` (core) | pods | get, list, watch, patch — `patch` for node_informer's `rio.build/hw-class` annotation stamp |
| `""` (core) | nodes | get, list, watch — node_informer `NodeLabelCache` (hw-band label join) |
| `""` (core) | events | list, watch — node_informer spot-interrupt watcher (`reason=SpotInterrupted`) |
| `events.k8s.io` | events | create, patch |
| `batch` | jobs | get, list, watch, create, delete |

Lease permissions (`coordination.k8s.io/leases`: get, create, update) are granted to the **scheduler's** ServiceAccount via a namespaced Role, not the controller (the controller has no leader election).

> **Note:** The controller does NOT hold permissions for `NetworkPolicies`, `ConfigMaps`, or `Leases`. NetworkPolicies are deployed as static manifests via the Helm chart (see below).

## NetworkPolicy

NetworkPolicy resources are deployed via the Helm chart (`infra/helm/rio-build/templates/networkpolicy.yaml`, gated on `networkPolicy.enabled`), not controller-managed. The controller has no `networking.k8s.io` RBAC permissions. Intended policies:

- **Executors**: egress to rio-scheduler and rio-store only (gRPC ports). No access to the Kubernetes API server or cloud metadata service (`fd00:ec2::254` / `169.254.169.254`). DNS egress to kube-system (CoreDNS) required for service resolution.
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

> **Note:** This requirement applies to the **client-side balancer** (`rio-proto/src/client/balance.rs`), which probes the named service to find the leader. It does NOT apply to K8s probes: `infra/helm/rio-build/templates/scheduler.yaml` intentionally uses `tcpSocket` for both readiness and liveness --- standby replicas report `NOT_SERVING` on the gRPC health endpoint (they haven't won the lease), so gRPC-based K8s probes would crash-loop them. TCP-accept succeeding proves the process is live; leader-election is client-side routing's concern, not K8s'. `store.yaml` does use `grpc.service: rio.store.StoreService` for readiness (store is not leader-elected, so `NOT_SERVING` only means drain or PG/S3 unhealthy --- correct to take out of rotation). Controller and executor probes are HTTP (`/healthz`/`/readyz`) and are unrelated to this rule.

r[ctrl.health.ready-gates-connect]
The controller binds its HTTP health server **before** awaiting `connect_forever` for the scheduler. `/healthz` (liveness) returns 200 unconditionally once the kube client is constructed; `/readyz` (readiness) returns 503 until the scheduler admin channel connects, then 200. Spawning the health server *after* `connect_forever` would leave nothing listening during scheduler cold-start and the chart's livenessProbe (`periodSeconds:10`, `failureThreshold:3`, no `startupProbe`) would SIGTERM the pod at ~20-30s --- re-introducing the CrashLoopBackOff that `connect_forever` was added to fix.

| Component | Liveness | Readiness | Startup |
|---|---|---|---|
| Gateway | TCP check on SSH port | After scheduler gRPC connection established | — |
| Scheduler | TCP socket (gRPC health is leader-election-aware, unsuitable for K8s probes on HA standby) | TCP socket — process is live, port is bound. Leader election happens; client-side balancer routes to leader via named-service health. | TCP socket. Startup budget sized for state recovery (reload non-terminal builds from PG). |
| Store | TCP socket | gRPC health check on named service --- after PostgreSQL + S3 reachable | — |
| Controller | HTTP `/healthz` (unconditional 200; bound before `connect_forever`) | HTTP `/readyz` — 503 until scheduler gRPC connected, then 200 (single-replica, no leader election) | — |
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

r[ctrl.scaler.component+2]
The controller reconciles `ComponentScaler` CRs into `apps/v1 Deployment {targetRef} /scale` patches. `desired_replicas = clamp(ceil(Σ(queued+running+substituting) / status.learnedRatio), spec.replicas.min, spec.replicas.max)` where `Σ(queued+running+substituting)` comes from `AdminService.ClusterStatus` (the **predictive** signal — scheduler knows N builders are about to exist before they exist; store scales ahead of the burst). Scale-down is held for 5 minutes after the last scale-up and limited to −1/tick. Reconcile interval: 10s.

r[ctrl.scaler.signal-substituting]

The predictive `builders` signal MUST include `substituting_derivations` at 1:1
weight with `queued`/`running`. A substitution cascade with zero queued/running
MUST NOT produce `builders=0` — that scales the store toward `min` exactly when
it is the bottleneck.

r[ctrl.scaler.ratio-learn+2]
`status.learnedRatio` self-calibrates against `max(StoreAdminService.GetLoad().pg_pool_utilization)` over the `spec.loadEndpoint` headless-service endpoints (the **observed** signal). Asymmetric correction: `load > spec.loadThresholds.high` (default 0.8) → immediate `current+1` AND `learnedRatio *= 0.95` (under-provisioning is dangerous — I-105 cascade); `load < spec.loadThresholds.low` (default 0.3) for 30 consecutive ticks → `learnedRatio *= 1.02` (over-provisioning is cheap). Growth is gated on `builders > 0 && current > min` (idle ≠ over-provisioned) and the ratio is clamped to `[RATIO_FLOOR, RATIO_CEILING]`. The ratio persists in `.status` so a controller restart resumes from the learned value, not `spec.seedRatio`.

r[store.admin.get-load+2]
`StoreAdminService.GetLoad` returns `pg_pool_utilization = (pool.size − pool.num_idle) / max_connections` and `substitute_admission_utilization = (capacity − available_permits) / capacity` for the replica it's called on. The ComponentScaler reconciler polls every store pod (DNS-resolving the headless service); per-pod load is `max(pg_pool_utilization, substitute_admission_utilization)` (substitution can saturate independently — upstream HTTP bottleneck while PG sits idle), and `observedLoadFactor` is the max across pods. The handler also publishes `rio_store_pg_pool_utilization` and `rio_store_substitute_admission_utilization` so Prometheus sees the same values the controller acted on.

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

## NodeClaim pool (ADR-023 §13b)

r[ctrl.nodeclaim.ffd-sim]
The per-tick NodeClaim-pool reconcile simulates placement via
first-fit-decreasing — intents sorted `(eta=0, c*)` descending (Ready
before forecast, large before small), bin-select `MostAllocated` on
the `allocatable` divisor — so the deficit is the unplaced residual
and matches the `schedulerName: rio-packed` instance.

r[ctrl.nodeclaim.anchor-bulk]
Unplaced intents per `(h,cap)` cell are covered by `1×anchor`
(smallest type fitting `max_U(c*,M,D)`) plus
`⌈(Σc* − anchor.cores)/bulk.cores⌉ × bulk` (cheapest \$/core type
meeting `median_U(M/c*)`). NodeClaim creation is capped at
`sla.maxNodeClaimsPerCellPerTick` and the `sla.maxFleetCores` budget;
cells are iterated round-robin from a rotating start so no cell
starves under sustained pressure.

r[ctrl.nodeclaim.lead-time-ddsketch]
`lead_time[h,cap] = q_0.9(boot − eta_error)` is read from a sliding
active/shadow DDSketch pair per cell, persisted to PG as
`u32`-version-tagged BYTEA. Sketches are seeded from
`sla.leadTimeSeed[h,cap]` at synthetic count `n_seed = 1/(1-q) = 10`;
the closed-loop `forecast_warm_hit_ratio` Schmitt widens/narrows the
quantile by `Δq=0.02` per firing, capped at `q ≤ 0.99` and
`lead_time ≤ sla.maxLeadTime`.

r[ctrl.nodeclaim.consolidate-na]
An empty NodeClaim is kept while
`λ(t)·𝔼[c_arrival·𝟙{c_arrival ≤ cores}] > cores/q_0.5(boot[h,cap])`.
The hazard `λ(t)` is the Nelson–Aalen estimate over right-censored
`idle_gap[h,cap]`; the fitting-core term is the current tick's
per-cell mean over `intents` (defined as 0 when intents is ⊥ or
empty). A floor `consolidate_after ≥ q_0.5(boot)/2` prevents a
transient lull from collapsing to always-delete.

r[ctrl.nodeclaim.shim-nodepool]
A single shim NodePool (`limits:{cpu:0}`,
`disruption.budgets:[{nodes:"0"}]`) satisfies Karpenter's
state-tracking lookup; the controller stamps
`karpenter.sh/nodepool: rio-nodeclaim-shim` plus `rio.build/*` on
`NodeClaim.metadata.labels`. NodeClaims reference EC2NodeClass
directly (`rio-nvme` / `rio-default` by storage); rio owns deletion.

r[ctrl.nodeclaim.priority-bucket]
Builder pods MUST set `priorityClassName=rio-builder-prio-{⌊log₂c*⌋}`
(10 fixed PriorityClasses, buckets 0–9, `globalDefault:false`,
`preemptionPolicy:Never`) and `schedulerName=rio-packed`. Config-load
asserts `maxCores < 1024`.

r[ctrl.nodeclaim.placeable-gate]
When `nodeclaim_pool.enabled`, the Pool reconciler creates Jobs only
for intents the nodeclaim_pool reconciler's last FFD simulation placed
on a `Registered=True` NodeClaim. The §13a `ready` retain is replaced;
Job count is bounded by Registered-node capacity, not Ready-set size.
An unarmed gate (no FFD tick yet) is fail-closed for both spawn and
reap.

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
- `rio-controller/src/reconcilers/pool/` --- Pool reconcile loop + Job/pod-spec builders
- `rio-controller/src/reconcilers/componentscaler/` --- ComponentScaler (rio-store/rio-gateway Deployment scaling)

## CRD Versioning

CRDs follow a `v1alpha1` → `v1beta1` → `v1` progression. The initial implementation uses `v1alpha1` with no stability guarantees. Plan a conversion webhook before promoting to `v1beta1` to support zero-downtime upgrades from `v1alpha1`.

## CRD Validation

CRDs use CEL validation rules (`x-kubernetes-validations`) for structural constraints:

| CRD | Rule | Reject reason |
|---|---|---|
| Pool | `size(self.systems) > 0` | A pool with no target systems accepts no work. |
| Pool | `hostNetwork ⇒ privileged` | See `r[ctrl.crd.host-users-network-exclusive]`. |
| Pool | `kind=Fetcher ⇒ features empty` | See `r[ctrl.crd.fetcher-no-features]`. |
| SeccompProfileKind | type ∈ allowed; `Localhost ⇔ has(localhostProfile)` | See `r[ctrl.crd.seccomp-cel]`. |
| ComponentScaler.Replicas | `self.min >= 0 && self.min <= self.max` | Clamp range must be non-empty and non-negative (`/scale` subresource rejects negative replicas). |
| ComponentScaler.TargetRef | `self.kind == 'Deployment'` | Reconciler patches `apps/v1 deployments/scale` only. |
| ComponentScaler.LoadThresholds | `0.0 < low < high <= 1.0` | Threshold ordering for ratio correction. |

r[ctrl.crd.fetcher-no-features]
The controller MUST reject `Pool` specs with `kind: Fetcher` and a non-empty `features` list. FODs route by `is_fixed_output` alone, not features (ADR-019). The reconciler forces `effective_features() == []` for Fetchers regardless (belt-and-suspenders for pre-CEL specs); without it, a non-empty `features` on a Fetcher pool hits the I-181 ∅-guard at scheduler `snapshot.rs` and filters out every featureless FOD --- the fetcher pool never spawns and all fetches stall silently. The single `effective_features` chokepoint ensures the spawn-decision query (`GetSpawnIntents.features`) and the spawned worker's `RIO_FEATURES` cannot diverge.

r[ctrl.crd.seccomp-cel]
`SeccompProfileKind` is a struct (`{type, localhostProfile?}`), not a Rust enum: kube-core's structural-schema rewriter rejects oneOf-variant subschemas with non-identical shared properties, so the type/localhostProfile coupling is enforced by CEL instead of the Rust type system. Two rules: `self.type in ['RuntimeDefault', 'Localhost', 'Unconfined']`, and `self.type == 'Localhost' ? has(self.localhostProfile) : !has(self.localhostProfile)`. The struct mirrors `pod.spec.securityContext.seccompProfile` exactly so operators can copy-paste; nested `KubeSchema` carries the rules through into the `Pool` schema.

r[ctrl.crd.host-users-network-exclusive]
The controller MUST reject `Pool` specs with `hostNetwork: true` and `privileged` unset or false. Kubernetes admission rejects pod specs combining `hostUsers: false` with `hostNetwork: true` (user-namespace UID remapping is incompatible with the host network namespace). Since the non-privileged path sets `hostUsers: false` unconditionally (ADR-012, `r[sec.pod.host-users-false]`), `hostNetwork: true` implies the `privileged: true` escape hatch. CRD CEL validation enforces this at `kubectl apply` time; the builder additionally suppresses `hostUsers` when the combination is encountered in pre-existing specs (emitting a Warning event).

r[ctrl.event.spec-degrade]
The Pool reconciler MUST emit a `Warning`-type Kubernetes Event for every spec field the builder silently degrades. CEL validation rejects NEW specs with invalid combinations; existing specs applied before the CEL rule landed are defensively corrected at pod-template time (e.g., `hostUsers` suppressed for `hostNetwork: true`). Without a Warning event, the operator has no signal that their spec is stale — `kubectl get pool -o yaml` shows the original value; the pod template shows the corrected value. The Warning names the field, the spec value, and the remediation.

The FUSE cache emptyDir `sizeLimit` is `PoolSpec.fuseCacheBytes` (default `BUILDER_FUSE_CACHE_BYTES` = 8Gi / `FETCHER_FUSE_CACHE_BYTES` = 4Gi; helm `poolDefaults.fuseCacheBytes` overrides to 50Gi in prod). The same value is added to the container's `ephemeral-storage` request/limit so the two cannot drift. Pods are one-shot so the cache never outlives one build's input closure.

## Pool Finalizer

Pool CRDs carry a `pool.rio.build/drain` finalizer. The
finalizer's `cleanup()` removes the finalizer immediately; in-flight Jobs
finish their one build naturally and ownerRef GC removes them after the
pool is gone. The reconciler's `apply()` path short-circuits when
`deletionTimestamp` is set (finalizer wraps it) so no new Jobs are
spawned during deletion.
