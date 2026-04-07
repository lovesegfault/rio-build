# rio-controller

Manages rio-build lifecycle on Kubernetes via CRDs.

## CRDs

### BuilderPool

r[ctrl.crd.builderpool]
```yaml
apiVersion: rio.build/v1alpha1
kind: BuilderPool
metadata:
  name: default
spec:
  maxConcurrent: 20                            # concurrent-Job ceiling (one Job = one build)
  resources:
    requests: { cpu: "4", memory: "8Gi" }
    limits: { cpu: "8", memory: "16Gi" }      # the build's limits — one build per pod
  fuseCacheSize: 100Gi                         # local SSD cache for rio-fuse
  features: [big-parallel, kvm]               # maps to requiredSystemFeatures
  systems: [x86_64-linux]
  image: rio-builder:dev                       # required — container image ref
  imagePullPolicy: IfNotPresent                # optional — K8s default if omitted
  sizeClass: small                             # maps to RIO_SIZE_CLASS env
  tlsSecretName: rio-builder-tls               # optional — Secret with tls.crt/tls.key/ca.crt for mTLS
  hostNetwork: false                           # optional — true only for VM-test/airgap escapes; requires privileged:true (hostUsers:false incompatible with host netns)
  hostUsers: false                             # optional — false runs in a user namespace (production default; ADR-012)
  seccompProfile: Localhost                    # optional — RuntimeDefault | Localhost | Unconfined
  fuseThreads: 4                               # optional — fuser worker threads (RIO_FUSE_THREADS)
  fusePassthrough: true                        # optional — kernel passthrough mode for backed reads (RIO_FUSE_PASSTHROUGH)
  daemonTimeoutSecs: 300                       # optional — local nix-daemon RPC timeout (RIO_DAEMON_TIMEOUT_SECS)
  terminationGracePeriodSeconds: 7200          # optional — K8s grace; defaults to 2h to let in-flight build complete
  topologySpread: true                         # optional — add topologySpreadConstraints across zones
  # securityContext: NOT a CRD field. The controller hardcodes
  # capabilities (SYS_ADMIN + SYS_CHROOT) in build_pod_spec().
  # Only `privileged: bool` is exposed (for VM-test/airgap escape
  # hatches); production pods use granular caps, not privileged.
  nodeSelector:
    rio.build/worker: "true"
  tolerations:
    - key: rio.build/worker
      operator: Equal
      value: "true"
      effect: NoSchedule
status:
  replicas: 5                                  # active Jobs
  readyReplicas: 4
  desiredReplicas: 8
```

### Job lifecycle

r[ctrl.pool.ephemeral]
The reconciler polls `AdminService.ClusterStatus` each requeue tick (10s)
and spawns K8s Jobs when `queued_derivations > 0` and active Jobs <
`spec.maxConcurrent`. Each Job runs one rio-worker pod whose main loop
exits after one `CompletionReport` → pod terminates → Job goes Complete →
`ttlSecondsAfterFinished: 60` reaps. Job settings: `backoffLimit: 0`
(scheduler owns retry), `restartPolicy: Never`, `parallelism: 1`.
`spec.maxConcurrent` is the concurrent-Job ceiling, not a standing set;
zero queued derivations means zero workers.

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
excess-Pending reap (`r[ctrl.ephemeral.reap-excess-pending]`) and
manifest mode's per-bucket scale-down (`r[ctrl.pool.manifest-scaledown]`).
`ttlSecondsAfterFinished` reaps Completed/Failed Jobs; ownerRef GC
handles pool-delete cleanup.

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
or present with `running_builds == 0` --- the controller MUST delete the
Job. This is the controller-side backstop for I-165: a builder process
stuck in uninterruptible sleep (D-state FUSE wait, OOM-loop) cannot
self-exit via the 120s `RIO_IDLE_SECS` idle-timeout, never disconnects
from the scheduler, and would otherwise sit until `activeDeadlineSeconds`
(default 1h). The grace MUST exceed the builder's idle-timeout so the
process-level exit is given first chance; the controller reap fires only
when the process cannot act on its own. A Job whose executor reports
`running_builds > 0` is NOT reaped --- the scheduler believes a build is
in progress; `activeDeadlineSeconds` is the backstop for stuck-mid-build.
The reap is **skipped entirely** when `ListExecutors` fails (scheduler
unreachable) --- fail-closed, same posture as
`r[ctrl.ephemeral.reap-excess-pending]`.

**Cleanup:** the finalizer's `cleanup()` returns immediately. In-flight
Jobs finish their one build naturally; ownerRef GC removes them after
the pool is gone.

r[ctrl.pool.ephemeral-deadline]
Jobs MUST set `spec.activeDeadlineSeconds` from
`BuilderPoolSpec.deadlineSeconds` (default 3600). This is a backstop:
the spawn decision reads the **cluster-wide**
`ClusterStatus.queued_derivations`, not pool-matching depth. A queue
full of `x86_64-linux` work on an `aarch64-darwin` pool triggers a Job
spawn; the spawned worker heartbeats in, never matches dispatch (wrong
`system`), and would hang indefinitely without a deadline. K8s kills
the pod at deadline, `backoffLimit: 0` marks the Job Failed,
`ttlSecondsAfterFinished` reaps.

r[ctrl.ephemeral.per-class-deadline]
When `BuilderPoolSpec.sizeClassCutoffSecs` is set (stamped onto every
BuilderPoolSet child from `SizeClassSpec.cutoffSecs`), the Job's
`activeDeadlineSeconds` MUST be `cutoffSecs * DEADLINE_MULTIPLIER` (5)
instead of the flat 3600 default --- so a `tiny` (cutoff 30s) pod gets
150s, `xlarge` (cutoff 7200s) gets 36000s. An explicit
`deadlineSeconds` on the pool overrides the computed value verbatim. Rationale (I-200): with a flat 1h deadline, a tiny-class build
that hangs holds a node for 3600s before the K8s deadline kills it ->
`ExecutorDisconnected` -> `r[sched.reassign.no-promote-on-ephemeral-
disconnect+2]` promotes; tying the deadline to the class cutoff makes
the deadline a per-class hung-build detector. K8s killing the pod at
deadline routes through the same disconnect-promotes path as an OOMKill,
so the next dispatch lands on a larger class. `FetcherSizeClass` has no
`cutoffSecs` (FODs route by reactive floor only) so fetcher Jobs keep
the flat 300s default.

r[ctrl.pool.per-system-class-depth]
For pools with `spec.sizeClass` set, the spawn/scale decision MUST
intersect the class's `SizeClassStatus.queued_by_system` with
`spec.systems` rather than reading the class-wide `queued` scalar.
Size-class pools are per-arch (one BuilderPoolSet per arch), so the
class scalar includes other-arch derivations the pool can never build —
an x86-64-tiny pool spawned at ceiling for an aarch64-only backlog
floods the scheduler with wrong-arch idle workers (I-143). Falls back
to the scalar when `queued_by_system` is empty (scheduler predates the
breakdown) — over-spawn rather than never spawn, same posture as
I-107's cluster-wide filter. This narrows but does not eliminate the
`r[ctrl.pool.ephemeral-deadline]` backstop.

r[ctrl.pool.per-feature-class-depth]
For pools with `spec.sizeClass` set, the spawn/scale decision MUST
pass `spec.features` as the `GetSizeClassStatus` feature filter
(`r[sched.sizeclass.feature-filter+2]`) so the per-class count reflects
derivations whose `requiredSystemFeatures` this pool's workers can
satisfy. When `spec.features` is non-empty, the pool sums the filtered
count across ALL classes rather than only `spec.sizeClass`: dispatch
overflow walks up the class chain, so a `tiny`-classified kvm derivation
will route to the `xlarge`-kvm pool if that is the only kvm-capable
pool. Without this, the kvm pool reads `queued{xlarge}=0` and never
spawns while the featureless `tiny` pool spawns a builder that
`hard_filter` rejects (I-176). Feature-gated pools may over-count
no-feature derivations (`∅ ⊆ pool_features`); `spawn_count`'s
active-subtraction and the `r[ctrl.pool.ephemeral-deadline]` backstop
bound the waste.

r[ctrl.pool.manifest-reconcile]
Under `spec.sizing=Manifest`, the reconciler polls `GetCapacityManifest`,
groups estimates by `(est_memory_bytes, est_cpu_millicores)` buckets,
diffs against live Job inventory (by label), and spawns Jobs for the
deficit. Each spawned Job's pod gets `ResourceRequirements` from the
manifest bucket, not from `spec.resources`. `spec.resources` becomes the
cold-start floor (used for derivations the manifest omits — no
`build_history` sample yet).

r[ctrl.pool.manifest-labels]
Manifest-spawned Jobs carry `rio.build/memory-class={n}Gi` and
`rio.build/cpu-class={n}m` labels. Operators can `kubectl get job -l
rio.build/memory-class=48Gi` to see the 48Gi fleet; the reconciler uses
the same labels for its inventory count.

r[ctrl.pool.manifest-scaledown]

Manifest-mode scale-down is per-bucket: when `supply > demand` for a `(memory-class, cpu-class)` bucket for `SCALE_DOWN_WINDOW` (600s default), the controller deletes `surplus` Jobs from that bucket. Deletion skips Jobs whose pods are mid-build (`running_builds > 0` from `ListExecutors`). Demand returning before the window elapses resets the clock.

r[ctrl.pool.manifest-failed-sweep+2]
The manifest reconciler MUST delete Failed Jobs alongside `ttlSecondsAfterFinished` reaping. With `backoff_limit=0`, a crash-looping pod produces up to `maxConcurrent` Failed Jobs per reconcile tick (the spawn pass fires `headroom` replacements, all of which may fail); the ceiling (`spec.maxConcurrent`) does not cap accumulation because Failed Jobs are not active supply. The sweep is bounded per-tick to `max(20, spec.maxConcurrent)` — the cap tracks the pool's own spawn ceiling so the sweep converges under full crash-loop (net accumulation ≤ 0 per tick). A `CrashLoopDetected` Warning event is emitted when the Failed count crosses 3.

Manifest-spawned pods are one-shot like static-sizing pods: the worker exits after one build, the Job completes, `ttlSecondsAfterFinished` reaps. The only difference from `sizing: Static` is the per-derivation `ResourceRequirements` taken from the manifest bucket instead of `spec.resources`.

r[ctrl.pool.manifest-fairness]
When the manifest ceiling (`spec.maxConcurrent - active`) is less than
total demand, the reconciler MUST apply per-bucket-floor truncation:
every bucket with nonzero deficit (including cold-start) gets at least
one spawn before any bucket gets a second. Prevents starvation under
sustained small-bucket-heavy load where BTreeMap small-first iteration
would never reach large buckets or cold-start. Cold-start starvation is
a livelock: derivations that never build never get a `build_history`
sample, so they never graduate out of cold-start.

r[ctrl.builderpool.kvm-device]
When `BuilderPoolSpec.features` contains `"kvm"`, the controller MUST
append `rio.build/kvm: "true"` to the pod's `nodeSelector` AND append a
`rio.build/kvm=true:NoSchedule` toleration. containerd `base_runtime_spec`
injects `/dev/kvm` into every pod's `/dev` (same mechanism as
`r[sec.pod.fuse-device-plugin]`), but only EC2 `.metal` instance types
expose host KVM (nested virt) — on non-metal the device node ENXIOs on
open. The nodeSelector + toleration land kvm pods exclusively on the
metal NodePool while non-kvm pods stay on the cheaper general builder
NodePools. The nodeSelector + toleration are unconditional wrt
`privileged` so privileged kvm pods still land on metal.

### BuilderPoolSet

> **Implemented:** CRD types, child-builder reconciler, status aggregation (`r[ctrl.wps.cutoff-status]`), CutoffRebalancer (`r[sched.rebalancer.sita-e]`).

r[ctrl.wps.reconcile]
The BuilderPoolSet reconciler creates one child BuilderPool per `spec.classes[i]`, named `{bps}-{class.name}`, with `ownerReferences[0].controller=true` pointing at the BPS. SSA-apply with force (field manager `rio-controller-wps`). On deletion, the finalizer-wrapped cleanup explicitly deletes children for deterministic timing; k8s ownerRef GC is the fallback.

r[ctrl.fetcherpool.classes]
When `FetcherPool.spec.classes[]` is non-empty, the FetcherPool reconciler spawns Jobs per class, labeled `rio.build/pool={fp.name}-{class.name}` (see `r[ctrl.fetcherpool.multiarch]`), each with `RIO_SIZE_CLASS={class.name}` injected via env so the executor reports it in `HeartbeatRequest.size_class`. Per-class `resources` apply; `max_replicas` overrides the pool-wide `spec.maxConcurrent`. Security posture (`readOnlyRootFilesystem`, fetcher seccomp, node placement) is identical across classes. When `classes` is empty (default), Jobs use `spec.resources` directly.

r[ctrl.fetcherpool.ephemeral-per-class]
When `classes[]` is non-empty, the reconciler spawns Jobs per class: for each `class`, list active Jobs labeled `rio.build/pool={fp.name}-{class.name}`, read `GetSizeClassStatusResponse.fod_classes[class.name].queued` (in-flight FOD demand bucketed by `size_class_floor`), and spawn `spawn_count(queued, active, class.maxReplicas)` Jobs with `RIO_SIZE_CLASS={class.name}` + per-class `resources`. Missing class in the RPC response (scheduler `[[fetcher_size_classes]]` unconfigured or out of sync) falls back to the flat `queued_fod_derivations` count for `classes[0]` only --- better to over-spawn the smallest class than to never spawn.

r[ctrl.fetcherpool.multiarch]
Multiple `FetcherPool` CRs MAY coexist (typically one per arch). The reconciler derives the `rio.build/pool` label as `{fp.name}-{class.name}` so pools sharing a class name don't collide in the same namespace. Each pool's `spec.systems` and `spec.nodeSelector["kubernetes.io/arch"]` MUST agree (a pool advertising `x86_64-linux` MUST select `amd64` nodes). Dispatch is pool-agnostic: the scheduler scores across all registered fetchers regardless of which `FetcherPool` spawned them.

r[ctrl.fetcherpool.spawn-builtin]
The spawn signal (`class_queued_for_systems`) counts `queued_by_system["builtin"]` for every `FetcherPool` regardless of whether `"builtin"` appears in `spec.systems`. Every executor unconditionally advertises `"builtin"` (`r[sched.dispatch.fod-builtin-any-arch]`), so a `system="builtin"` FOD can land on any pool; omitting it from the spawn signal would stall a cold-store bootstrap. Accept ≤N× spawn for `system="builtin"` FODs across N pools --- `reap_excess_pending` reclaims the surplus once dispatch drains the queue.

r[ctrl.wps.cutoff-status]
The WPS reconciler writes per-class `effective_cutoff_secs` + `queued` to WPS status via SSA patch (field manager `rio-controller-wps-status`). Values come from the `GetSizeClassStatus` admin RPC. SSA patch body MUST include `apiVersion` + `kind`.

r[ctrl.wps.prune-stale]
The BPS reconciler prunes child BuilderPools whose `size_class` no longer appears in `spec.classes`. Prune matches by ownerRef UID (not name-prefix — robust to BPS renames); deletes are best-effort (404-tolerant, logged on other errors, non-fatal so a stuck child doesn't wedge the whole reconcile). Without prune, a removed-class child is orphaned: the BPS reconciler iterates `spec.classes` and won't touch it, and ownerRef GC doesn't fire while the parent still exists.

For deployments with heavy-tailed build workloads, `BuilderPoolSet` defines multiple size-class builder pools with different resource allocations. The scheduler routes derivations to the appropriate pool based on estimated duration. See [ADR-015](../decisions/015-size-class-routing.md) for the design rationale.

```yaml
apiVersion: rio.build/v1alpha1
kind: BuilderPoolSet
metadata:
  name: default
spec:
  sizeClasses:
    - name: small
      durationCutoff: 60s
      pool:
        maxConcurrent: 40
        resources:
          requests: { cpu: "2", memory: "4Gi" }
          limits: { cpu: "4", memory: "8Gi" }
        fuseCacheSize: 50Gi
    - name: medium
      durationCutoff: 600s
      pool:
        maxConcurrent: 15
        resources:
          requests: { cpu: "4", memory: "8Gi" }
          limits: { cpu: "8", memory: "16Gi" }
        fuseCacheSize: 100Gi
    - name: large
      durationCutoff: null           # unbounded (everything > 600s)
      pool:
        maxConcurrent: 10
        resources:
          requests: { cpu: "8", memory: "16Gi" }
          limits: { cpu: "16", memory: "32Gi" }
        fuseCacheSize: 200Gi
  cutoffLearning:
    enabled: true
    algorithm: sita-e
    recomputeInterval: 1h
    minSamples: 100
    smoothingFactor: 0.1
status:
  classes:
    - name: small
      effectiveCutoff: 45s           # learned cutoff (may differ from spec)
      replicas: 12
      readyReplicas: 12
    - name: medium
      effectiveCutoff: 480s
      replicas: 5
      readyReplicas: 4
    - name: large
      effectiveCutoff: null
      replicas: 3
      readyReplicas: 3
  lastCutoffUpdate: 2026-02-13T07:00:00Z
```

The existing `BuilderPool` CRD remains valid for single-pool deployments without size-class routing.

## Reconciliation Loops

r[ctrl.reconcile.owner-refs]
- **BuilderPool reconciler**: spawn/reap builder Jobs based on scheduler queue depth. All Jobs carry `ownerReferences` to the BuilderPool CRD with `controller: true`, ensuring garbage collection on BuilderPool deletion.
- **BuilderPoolSet reconciler**: manages multiple `BuilderPool` sub-resources (one per size class). Queries the scheduler's `CutoffRebalancer` for learned cutoffs and updates the `BuilderPoolSet` status.
- **GC reconciler**: trigger store garbage collection on schedule.

## RBAC

The controller requires a dedicated ServiceAccount with a ClusterRole granting (see `infra/helm/rio-build/templates/rbac.yaml`):

| API Group | Resources | Verbs |
|---|---|---|
| `rio.build` | builderpools | get, list, watch, create, update, patch, delete |
| `rio.build` | builderpools/status | get, patch |
| `""` (core) | pods | get, list, watch |
| `""` (core) | events | create, patch |
| `batch` | jobs | get, list, watch, create, delete |

Lease permissions (`coordination.k8s.io/leases`: get, create, update) are granted to the **scheduler's** ServiceAccount via a namespaced Role, not the controller (the controller has no leader election).

> **Note:** The controller does NOT hold permissions for `NetworkPolicies`, `ConfigMaps`, or `Leases`. NetworkPolicies are deployed as static manifests via the Helm chart (see below).

## NetworkPolicy

NetworkPolicy resources are deployed via the Helm chart (`infra/helm/rio-build/templates/networkpolicy.yaml`, gated on `networkPolicy.enabled`), not controller-managed. The controller has no `networking.k8s.io` RBAC permissions. Intended policies:

- **Workers**: egress to rio-scheduler and rio-store only (gRPC ports). No access to the Kubernetes API server or cloud metadata service (`169.254.169.254`). DNS egress to kube-system (CoreDNS) required for service resolution.
- **Gateway**: ingress from external (Service type LoadBalancer/NodePort for SSH). Egress to rio-scheduler and rio-store. DNS egress to kube-system.
- **Scheduler**: egress to PostgreSQL. DNS egress to kube-system.
- **Store**: egress to PostgreSQL and S3. DNS egress to kube-system.
- **Controller**: egress to rio-scheduler (gRPC, for `AdminService.ClusterStatus`/`GetSizeClassStatus` queue-depth queries) and to the Kubernetes API server (for CRD watches and Job management). DNS egress to kube-system.

## PodDisruptionBudget

| Component | Managed by | Policy | Rationale |
|---|---|---|---|
| Scheduler | Static manifest | `maxUnavailable: 1` | Leader election handles failover; at most one pod unavailable |
| Gateway | Static manifest | `minAvailable: 1` | At least one pod must remain for SSH connectivity |

Scheduler and gateway PDBs are static manifests in the Helm chart (`infra/helm/rio-build/templates/pdb.yaml`, gated on `podDisruptionBudget.enabled`). Worker pods are one-shot Jobs — a PDB on Jobs is meaningless (eviction of a Job pod just reschedules the build via `r[ctrl.drain.disruption-target]`).

## Service Definitions

| Service | Type | Purpose |
|---|---|---|
| `rio-gateway` | LoadBalancer or NodePort | SSH ingress for `nix copy` / `nix build --store ssh://` |
| `rio-scheduler` | ClusterIP | Internal gRPC for workers, gateway, and controller |
| `rio-store` | ClusterIP + optional Ingress | gRPC for internal components; HTTP for binary cache serving |

## Health Probes

r[ctrl.probe.named-service]
Health probes against `grpc.health.v1.Health/Check` MUST target a named service (e.g., `rio.scheduler.SchedulerService`), not the empty-string default. `set_not_serving` only affects named services, not `""` --- a probe on `""` stays green through drain and through standby.

> **Note:** This requirement applies to the **client-side balancer** (`rio-proto/src/client/balance.rs`), which probes the named service to find the leader. It does NOT apply to K8s probes: `infra/helm/rio-build/templates/scheduler.yaml` intentionally uses `tcpSocket` for both readiness and liveness --- standby replicas report `NOT_SERVING` on the gRPC health endpoint (they haven't won the lease), so gRPC-based K8s probes would crash-loop them. TCP-accept succeeding proves the process is live; leader-election is client-side routing's concern, not K8s'. `store.yaml` does use `grpc.service: rio.store.StoreService` for readiness (store is not leader-elected, so `NOT_SERVING` only means drain or PG/S3 unhealthy --- correct to take out of rotation). Worker probes are HTTP (`/healthz`/`/readyz`) and are unrelated to this rule.

| Component | Liveness | Readiness | Startup |
|---|---|---|---|
| Gateway | TCP check on SSH port | After scheduler gRPC connection established | — |
| Scheduler | TCP socket (gRPC health is leader-election-aware, unsuitable for K8s probes on HA standby) | TCP socket — process is live, port is bound. Leader election happens; client-side balancer routes to leader via named-service health. | TCP socket. Startup budget sized for state recovery (reload non-terminal builds from PG). |
| Store | TCP socket | gRPC health check on named service --- after PostgreSQL + S3 reachable | — |
| Controller | HTTP `/healthz` | After CRD watches established | — |
| Worker | HTTP `/healthz` + `/readyz` (no gRPC server) | `/readyz` 200 after first accepted heartbeat | HTTP check, `failureThreshold × periodSeconds ≥ 120s` (FUSE mount + cache warm) |

## Worker Lifecycle

r[ctrl.drain.sigterm]
**Scale-down:** `terminationGracePeriodSeconds` is set to `7200` (2 hours) to allow in-flight builds to complete.

**SIGTERM drain (no preStop hook needed):** the worker's main loop has a `select!` arm on SIGTERM. On signal:

1. Set heartbeat `draining=true` (the worker-authoritative drain source — I-063); the scheduler's `has_capacity()` reads it via `is_draining()`.
2. Keep the `BuildExecution` stream connected (and reconnect across scheduler restart) until `BuildSlot::wait_idle()` returns — the in-flight build's completion is reported.
3. Send `AdminService.DrainExecutor` (best-effort exit deregister) and exit 0.

A preStop hook doing the same is redundant: K8s sends SIGTERM on pod termination regardless of preStop, and the worker's signal handler implements the drain. The Job pod template does NOT define a preStop.

r[ctrl.drain.disruption-target]
**Eviction-triggered preemption:** the controller runs a Pod watcher filtered to `rio.build/pool`-labeled pods with `status.conditions[type=DisruptionTarget,status=True]`. When K8s marks a pod for eviction (node drain, spot interrupt), the watcher calls `AdminService.DrainExecutor{force:true}` — the scheduler sends `CancelSignal` for the in-flight build → executor `cgroup.kill()`s → the build reassigns to a healthy executor within seconds. Without this, the evicting pod would self-drain with `force=false` and wait up to `terminationGracePeriodSeconds` (2h) for the in-flight build to complete naturally before SIGKILL loses it anyway. The SIGTERM self-drain path (above) is the fallback if the watcher misses the window.

## ComponentScaler

r[ctrl.scaler.component]
The controller reconciles `ComponentScaler` CRs into `apps/v1 Deployment {targetRef} /scale` patches. `desired_replicas = clamp(ceil(Σ(queued+running) / status.learnedRatio), spec.replicas.min, spec.replicas.max)` where `Σ(queued+running)` comes from `AdminService.GetSizeClassStatus` (the **predictive** signal — scheduler knows N builders are about to exist before they exist; store scales ahead of the burst). Scale-down is held for 5 minutes after the last scale-up and limited to −1/tick. Reconcile interval: 10s.

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
- **BuilderPool** CRD → spawns/reaps builder Jobs (resources, security context)

The controller does **NOT** manage:
- Scheduler or store Deployments --- these are deployed via Helm/kustomize as standard Deployments
- Rationale: scheduler and store have simple lifecycle (single replica or leader-elected); CRD management adds complexity without benefit

## Key Files

- `rio-crds/src/` --- CRD type definitions (separate crate; BuilderPool, BuilderPoolSet, FetcherPool, ComponentScaler)
- `rio-controller/src/reconcilers/` --- Reconciliation loops
- `rio-controller/src/scaling/` --- ComponentScaler (rio-store/rio-gateway Deployment scaling)

## CRD Versioning

CRDs follow a `v1alpha1` → `v1beta1` → `v1` progression. The initial implementation uses `v1alpha1` with no stability guarantees. Plan a conversion webhook before promoting to `v1beta1` to support zero-downtime upgrades from `v1alpha1`.

## CRD Validation

CRDs use CEL validation rules (`x-kubernetes-validations`) for structural constraints:

- `spec.maxConcurrent > 0`
- `spec.systems` must be non-empty
- `spec.hostNetwork: true` requires `spec.privileged: true` — Kubernetes rejects `hostUsers: false` combined with `hostNetwork: true` at pod admission; the non-privileged path sets `hostUsers: false` per ADR-012

r[ctrl.crd.host-users-network-exclusive]
The controller MUST reject `BuilderPool` specs with `hostNetwork: true` and `privileged` unset or false. Kubernetes admission rejects pod specs combining `hostUsers: false` with `hostNetwork: true` (user-namespace UID remapping is incompatible with the host network namespace). Since the non-privileged path sets `hostUsers: false` unconditionally (ADR-012, `r[sec.pod.host-users-false]`), `hostNetwork: true` implies the `privileged: true` escape hatch. CRD CEL validation enforces this at `kubectl apply` time; the builder additionally suppresses `hostUsers` when the combination is encountered in pre-existing specs (emitting a Warning event).

r[ctrl.event.spec-degrade]
The BuilderPool reconciler MUST emit a `Warning`-type Kubernetes Event for every spec field the builder silently degrades. CEL validation rejects NEW specs with invalid combinations; existing specs applied before the CEL rule landed are defensively corrected at pod-template time (e.g., `hostUsers` suppressed for `hostNetwork: true`). Without a Warning event, the operator has no signal that their spec is stale — `kubectl get wp -o yaml` shows the original value; the pod template shows the corrected value. The Warning names the field, the spec value, and the remediation.

`spec.fuseCacheSize` is NOT a CEL rule — it is validated at reconcile time (`parse_quantity_to_gb` in `builders.rs` returns `InvalidSpec` on unparseable input, which fails the reconcile and emits an event).

## BuilderPool Finalizer

BuilderPool CRDs carry a `builderpool.rio.build/drain` finalizer. The
finalizer's `cleanup()` removes the finalizer immediately; in-flight Jobs
finish their one build naturally and ownerRef GC removes them after the
pool is gone. The reconciler's `apply()` path short-circuits when
`deletionTimestamp` is set (finalizer wraps it) so no new Jobs are
spawned during deletion.
