# rio-controller

Manages rio-build lifecycle on Kubernetes via CRDs.

## CRDs

### WorkerPool

r[ctrl.crd.workerpool]
```yaml
apiVersion: rio.build/v1alpha1
kind: WorkerPool
metadata:
  name: default
spec:
  ephemeral: false                             # default true (one Job per build); false = long-lived StatefulSet
  replicas:
    min: 2
    max: 20
  autoscaling:
    metric: queueDepth
    targetValue: 5                            # scale up when > 5 queued derivations per worker
  resources:
    requests: { cpu: "4", memory: "8Gi" }
    limits: { cpu: "8", memory: "16Gi" }      # the build's limits — one build per pod
  fuseCacheSize: 100Gi                         # local SSD cache for rio-fuse
  features: [big-parallel, kvm]               # maps to requiredSystemFeatures
  systems: [x86_64-linux]
  image: rio-worker:dev                        # required — container image ref
  imagePullPolicy: IfNotPresent                # optional — K8s default if omitted
  sizeClass: small                             # maps to RIO_SIZE_CLASS env
  tlsSecretName: rio-worker-tls                # optional — Secret with tls.crt/tls.key/ca.crt for mTLS
  fodProxyUrl: http://fod-proxy.rio.svc:3128   # optional — injected as http(s)_proxy for FOD builds
  hostNetwork: false                           # optional — true only for VM-test/airgap escapes; requires privileged:true (hostUsers:false incompatible with host netns)
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
  replicas: 5
  readyReplicas: 4
  desiredReplicas: 8
  lastScaleTime: 2026-02-13T06:00:00Z
  conditions:
    - type: Scaling                            # single condition type; reason distinguishes state
      status: "True"
      lastTransitionTime: 2026-02-13T05:55:00Z
      reason: ScaledUp                         # also: ScaledDown, UnknownMetric (status=False)
      message: "scaled from 5 to 8"
```

### Ephemeral WorkerPools

r[ctrl.pool.ephemeral]
When `WorkerPoolSpec.ephemeral: true`, the controller does NOT create a
StatefulSet, headless Service, or PodDisruptionBudget. Instead,
`reconcile_ephemeral` polls `AdminService.ClusterStatus` each requeue tick
(10s, vs 5min for STS pools) and spawns K8s Jobs when
`queued_derivations > 0` and active Jobs < `spec.replicas.max`. Each Job
runs one rio-worker pod with `RIO_EPHEMERAL=1` → worker's main loop exits
after one `CompletionReport` (no second event-loop iteration) → pod
terminates → Job goes Complete → `ttlSecondsAfterFinished: 60` reaps. Job
settings: `backoffLimit: 0` (scheduler owns retry), `restartPolicy:
Never`, `parallelism: 1`. `spec.replicas.min` MUST be 0 (CEL-enforced);
`spec.replicas.max` is the concurrent-Job ceiling, not a standing set.

**Isolation guarantee:** zero cross-build state. Fresh pod means fresh
emptyDir for FUSE cache and overlayfs upper, fresh filesystem. Untrusted
tenants cannot leave poisoned cache entries for subsequent builds — there
is no "subsequent build" on that pod. Strongest isolation when combined
with `hostUsers: false` + non-privileged (see `docs/src/security.md`
§ Ephemeral Builders).

**Cost:** per-build cold start (pod scheduling + container pull + FUSE
mount + scheduler registration — typically 10–30s) plus one reconcile
tick (~10s) before the Job is spawned. Locality scoring is moot (every
ephemeral worker has an empty cache; `count_missing` returns the full
closure for every candidate, so `best_executor` ties on locality and
falls through to first-fit). Pod churn may require Karpenter tuning
(consolidation policy). Long-lived (`ephemeral: false`) trades the
isolation freshness for warm-cache locality on trusted-tenant
workloads.

**Dispatch path unchanged:** from the scheduler's perspective, an
ephemeral Job pod is indistinguishable from an STS pod — it heartbeats
in, gets dispatched, sends CompletionReport, disconnects. The
"ephemeral" property is purely worker-side (`RIO_EPHEMERAL` → exit
after one build) and controller-side (Job lifecycle). The active
mechanism is ClusterStatus polling; a push-mode RPC was considered and
rejected (see `ephemeral.rs` § Why not a Scheduler→Controller RPC).

**RBAC:** the controller's ClusterRole grants `batch/jobs` verbs
`[get, list, watch, create, delete]`. For ephemeral mode alone,
`delete` is unneeded (`ttlSecondsAfterFinished` reaps; ownerRef GC
handles pool-delete cleanup). `delete` is required for manifest mode's
per-bucket scale-down (`r[ctrl.pool.manifest-scaledown]`) — manifest
pods don't self-terminate so there's no TTL to reap them.

**Cleanup:** the finalizer's `cleanup()` branches on `spec.ephemeral` and
returns immediately (no STS to scale to 0, no long-lived workers to
DrainWorker). In-flight Jobs finish their one build naturally.

> **Marker granularity:** This marker spans 5+ `r[impl]` sites across 3 files (controller branch / Job builder / worker single-shot gate). For finer traceability, use the sub-marker `ctrl.pool.ephemeral-deadline` — it covers the cross-cutting backstop behavior under the ephemeral umbrella.

r[ctrl.pool.ephemeral-deadline]
Ephemeral Jobs MUST set `spec.activeDeadlineSeconds` from
`WorkerPoolSpec.ephemeralDeadlineSeconds` (default 3600). This is a
backstop: `reconcile_ephemeral`'s spawn decision reads the
**cluster-wide** `ClusterStatus.queued_derivations`, not pool-matching
depth. A queue full of `x86_64-linux` work on an `aarch64-darwin`
ephemeral pool triggers a Job spawn; the spawned worker heartbeats in,
never matches dispatch (wrong `system`), and would hang indefinitely
without a deadline. K8s kills the pod at deadline, `backoffLimit: 0`
marks the Job Failed, `ttlSecondsAfterFinished` reaps. Per-pool queue
depth (the proper fix — benefits the STS autoscaler too; see
`scaling.rs` "per-pool wired in phase4 WorkerPoolSet") is deferred to
phase5's ClusterStatus proto extension. CEL validation rejects
`ephemeralDeadlineSeconds` set on non-ephemeral pools (field only makes
sense in Job mode).

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
For ephemeral pools with `spec.sizeClass` set, the spawn decision MUST
pass `spec.features` as the `GetSizeClassStatus` feature filter
(`r[sched.sizeclass.feature-filter]`) so the per-class count reflects
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
The manifest reconciler MUST delete Failed Jobs alongside idle-surplus deletes. With `backoff_limit=0` and no TTL (`r[ctrl.pool.manifest-long-lived]`), a crash-looping pod produces up to `replicas.max` Failed Jobs per reconcile tick (the spawn pass fires `headroom` replacements, all of which may fail); the ceiling (`spec.replicas.max`) does not cap accumulation because Failed Jobs are not active supply. The sweep is bounded per-tick to `max(20, spec.replicas.max)` — the cap tracks the pool's own spawn ceiling so the sweep converges under full crash-loop (net accumulation ≤ 0 per tick). A `CrashLoopDetected` Warning event is emitted when the Failed count crosses 3.

r[ctrl.pool.manifest-long-lived]

Manifest-spawned pods do NOT set `RIO_EPHEMERAL=1`. The worker's main loop does not exit after one build — it heartbeats, accepts any derivation that fits its `memory_total_bytes`, and idles. `ttlSecondsAfterFinished` is not set on the Job (the pod never self-terminates). Scale-down is entirely controller-driven.

r[ctrl.pool.manifest-fairness]
When the manifest ceiling (`spec.replicas.max - active`) is less than
total demand, the reconciler MUST apply per-bucket-floor truncation:
every bucket with nonzero deficit (including cold-start) gets at least
one spawn before any bucket gets a second. Prevents starvation under
sustained small-bucket-heavy load where BTreeMap small-first iteration
would never reach large buckets or cold-start. Cold-start starvation is
a livelock: derivations that never build never get a `build_history`
sample, so they never graduate out of cold-start.

r[ctrl.builderpool.kvm-device]
When `WorkerPoolSpec.features` contains `"kvm"`, the controller MUST add
`smarter-devices/kvm: 1` to the builder container's `resources.{requests,
limits}` (device-plugin injects `/dev/kvm` into the container's device
cgroup — same mechanism as `r[sec.pod.fuse-device-plugin]`), AND append
`rio.build/kvm: "true"` to the pod's `nodeSelector`, AND append a
`rio.build/kvm=true:NoSchedule` toleration. Only EC2 `.metal` instance
types expose `/dev/kvm` (nested virt); the nodeSelector + toleration land
kvm pods exclusively on the metal NodePool while non-kvm pods stay on the
cheaper general builder NodePools. The device resource is omitted under
`privileged: true` (device cgroup bypassed); the nodeSelector + toleration
are unconditional so privileged kvm pods still land on metal.

r[ctrl.pool.bloom-knob]
`WorkerPoolSpec.bloomExpectedItems` (optional) injects
`RIO_BLOOM_EXPECTED_ITEMS` into the worker container env. Unset →
worker uses its compile-time default (50k). Same only-inject-when-set
semantics as `fuseThreads` and `daemonTimeoutSecs`: injecting the
default would pin it at controller-build time, not worker-build time.

### WorkerPoolSet

> **Implemented:** CRD types, child-builder reconciler, status aggregation (`r[ctrl.wps.cutoff-status]`), per-class autoscaling (`r[ctrl.wps.autoscale]`), CutoffRebalancer (`r[sched.rebalancer.sita-e]`).

r[ctrl.wps.reconcile]
The WorkerPoolSet reconciler creates one child WorkerPool per `spec.classes[i]`, named `{wps}-{class.name}`, with `ownerReferences[0].controller=true` pointing at the WPS. SSA-apply with force (field manager `rio-controller-wps`). On deletion, the finalizer-wrapped cleanup explicitly deletes children for deterministic timing; k8s ownerRef GC is the fallback.

r[ctrl.fetcherpool.classes]
When `FetcherPool.spec.classes[]` is non-empty, the FetcherPool reconciler stamps one StatefulSet + headless Service per class, named `rio-fetcher-{class.name}`, each with `RIO_SIZE_CLASS={class.name}` injected via env so the executor reports it in `HeartbeatRequest.size_class`. Per-class `resources` apply; `min_replicas`/`max_replicas` override the pool-wide `spec.replicas` bounds. Security posture (`readOnlyRootFilesystem`, fetcher seccomp, node placement) is identical across classes. When `classes` is empty (default), single STS at `spec.resources` --- back-compat with pre-I-170 FetcherPools.

r[ctrl.fetcherpool.ephemeral-per-class]
When `FetcherPool.spec.ephemeral=true` AND `classes[]` is non-empty, the reconciler spawns Jobs per class: for each `class`, list active Jobs labeled `rio.build/pool={class.name}`, read `GetSizeClassStatusResponse.fod_classes[class.name].queued` (in-flight FOD demand bucketed by `size_class_floor`), and spawn `spawn_count(queued, active, class.maxReplicas)` Jobs with `RIO_SIZE_CLASS={class.name}` + per-class `resources`. Missing class in the RPC response (scheduler `[[fetcher_size_classes]]` unconfigured or out of sync) falls back to the flat `queued_fod_derivations` count for `classes[0]` only --- better to over-spawn the smallest class than to never spawn.

r[ctrl.wps.cutoff-status]
The WPS reconciler writes per-class `effective_cutoff_secs` + `queued` to WPS status via SSA patch (field manager `rio-controller-wps-status`). Values come from the `GetSizeClassStatus` admin RPC. SSA patch body MUST include `apiVersion` + `kind`.

r[ctrl.wps.autoscale]
Per-class autoscaling: for each WPS child WorkerPool, compute `desired = clamp(queued / target_queue_per_replica, min_replicas, max_replicas)` and SSA-patch `spec.replicas` with field manager `rio-controller-wps-autoscaler` (distinct from the reconciler's field manager — SSA merges field ownership). Skip children with non-nil `deletionTimestamp`.

r[ctrl.wps.prune-stale]
The WPS reconciler prunes child WorkerPools whose `size_class` no longer appears in `spec.classes`. Prune matches by ownerRef UID (not name-prefix — robust to WPS renames); deletes are best-effort (404-tolerant, logged on other errors, non-fatal so a stuck child doesn't wedge the whole reconcile). Without prune, a removed-class child is orphaned: the standalone autoscaler skips it (has ownerRef), the per-class autoscaler skips it (not in `spec.classes` iteration) — neither scales it.

For deployments with heavy-tailed build workloads, `WorkerPoolSet` defines multiple size-class worker pools with different resource allocations. The scheduler routes derivations to the appropriate pool based on estimated duration. See [ADR-015](../decisions/015-size-class-routing.md) for the design rationale.

```yaml
apiVersion: rio.build/v1alpha1
kind: WorkerPoolSet
metadata:
  name: default
spec:
  sizeClasses:
    - name: small
      durationCutoff: 60s
      pool:
        replicas: { min: 4, max: 40 }
        resources:
          requests: { cpu: "2", memory: "4Gi" }
          limits: { cpu: "4", memory: "8Gi" }
        fuseCacheSize: 50Gi
    - name: medium
      durationCutoff: 600s
      pool:
        replicas: { min: 2, max: 15 }
        resources:
          requests: { cpu: "4", memory: "8Gi" }
          limits: { cpu: "8", memory: "16Gi" }
        fuseCacheSize: 100Gi
    - name: large
      durationCutoff: null           # unbounded (everything > 600s)
      pool:
        replicas: { min: 1, max: 10 }
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

The existing `WorkerPool` CRD remains valid for single-pool deployments without size-class routing.

## Reconciliation Loops

r[ctrl.reconcile.owner-refs]
- **WorkerPool reconciler**: scale worker StatefulSet based on scheduler queue depth. Create/delete pods. Manage per-worker ephemeral storage for the FUSE cache. All resources created by this reconciler carry `ownerReferences` to the WorkerPool CRD with `controller: true`, ensuring garbage collection on WorkerPool deletion.
- **WorkerPoolSet reconciler**: manages multiple `WorkerPool` sub-resources (one per size class). Queries the scheduler's `CutoffRebalancer` for learned cutoffs and updates the `WorkerPoolSet` status. Autoscales each class independently based on per-class queue depth from the scheduler.
- **GC reconciler**: trigger store garbage collection on schedule.

## RBAC

The controller requires a dedicated ServiceAccount with a ClusterRole granting (see `infra/helm/rio-build/templates/rbac.yaml`):

| API Group | Resources | Verbs |
|---|---|---|
| `rio.build` | workerpools | get, list, watch, create, update, patch, delete |
| `rio.build` | workerpools/status | get, patch |
| `apps` | statefulsets | get, list, watch, create, update, patch, delete |
| `policy` | poddisruptionbudgets | get, list, watch, create, update, patch, delete |
| `""` (core) | pods | get, list, watch |
| `""` (core) | services | get, list, watch, create, update, patch, delete |
| `""` (core) | events | create, patch |
| `batch` | jobs | get, list, watch, create, delete |

Lease permissions (`coordination.k8s.io/leases`: get, create, update) are granted to the **scheduler's** ServiceAccount via a namespaced Role, not the controller (the controller has no leader election).

> **Note:** The controller holds `policy/poddisruptionbudgets` permissions (get, list, watch, create, update, patch, delete) for per-pool PDB management. It does NOT hold permissions for `NetworkPolicies`, `ConfigMaps`, or `Leases`. NetworkPolicies are deployed as static kustomize manifests (see below).

## NetworkPolicy

NetworkPolicy resources are deployed via the Helm chart (`infra/helm/rio-build/templates/networkpolicy.yaml`, gated on `networkPolicy.enabled`), not controller-managed. The controller has no `networking.k8s.io` RBAC permissions. Intended policies:

- **Workers**: egress to rio-scheduler and rio-store only (gRPC ports). No access to the Kubernetes API server or cloud metadata service (`169.254.169.254`). DNS egress to kube-system (CoreDNS) required for service resolution.
- **Gateway**: ingress from external (Service type LoadBalancer/NodePort for SSH). Egress to rio-scheduler and rio-store. DNS egress to kube-system.
- **Scheduler**: egress to PostgreSQL. DNS egress to kube-system.
- **Store**: egress to PostgreSQL and S3. DNS egress to kube-system.
- **Controller**: egress to rio-scheduler (gRPC, for `AdminService.ClusterStatus` autoscaling queries) and to the Kubernetes API server (for CRD watches and StatefulSet management). DNS egress to kube-system.

## PodDisruptionBudget

r[ctrl.pdb.workers]
The WorkerPool reconciler creates a `PodDisruptionBudget` child for each pool with `maxUnavailable: 1`. The PDB carries `ownerReferences` to the WorkerPool (garbage-collected on pool deletion). See `build_pdb` in `rio-controller/src/reconcilers/workerpool/builders.rs`.

| Component | Managed by | Policy | Rationale |
|---|---|---|---|
| Workers | Controller (per-pool) | `maxUnavailable: 1` | Maintain build capacity during node drain; `ownerReferences` → pool |
| Scheduler | Static manifest | `maxUnavailable: 1` | Leader election handles failover; at most one pod unavailable |
| Gateway | Static manifest | `minAvailable: 1` | At least one pod must remain for SSH connectivity |

Scheduler and gateway PDBs remain static manifests in the Helm chart (`infra/helm/rio-build/templates/pdb.yaml`, gated on `podDisruptionBudget.enabled`) --- the controller does not deploy scheduler/store.

## Service Definitions

| Service | Type | Purpose |
|---|---|---|
| `rio-gateway` | LoadBalancer or NodePort | SSH ingress for `nix copy` / `nix build --store ssh://` |
| `rio-scheduler` | ClusterIP | Internal gRPC for workers, gateway, and controller |
| `rio-store` | ClusterIP + optional Ingress | gRPC for internal components; HTTP for binary cache serving |
| Workers (headless) | ClusterIP (headless) | Individual pod addressing; scheduler tracks workers by pod name |

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
3. Send `AdminService.DrainWorker` (best-effort exit deregister) and exit 0.

A preStop hook doing the same is redundant: K8s sends SIGTERM on pod termination regardless of preStop, and the worker's signal handler implements the drain. The StatefulSet does NOT define a preStop.

r[ctrl.drain.disruption-target]
**Eviction-triggered preemption:** the controller runs a Pod watcher filtered to `rio.build/pool`-labeled pods with `status.conditions[type=DisruptionTarget,status=True]`. When K8s marks a pod for eviction (node drain, spot interrupt, PDB-mediated disruption), the watcher calls `AdminService.DrainWorker{force:true}` — the scheduler sends `CancelSignal` for the in-flight build → worker `cgroup.kill()`s → the build reassigns to a healthy worker within seconds. Without this, the evicting pod would self-drain with `force=false` and wait up to `terminationGracePeriodSeconds` (2h) for the in-flight build to complete naturally before SIGKILL loses it anyway. The SIGTERM self-drain path (above) is the fallback if the watcher misses the window.

r[ctrl.autoscale.direct-patch]
**Autoscaling:** The controller queries the scheduler's `AdminService.ClusterStatus` gRPC RPC to obtain current queue depth and worker utilization. It directly patches StatefulSet replicas based on this data. This is an internal mechanism, not HPA. The scaling logic requires stabilization windows and anti-flapping thresholds to avoid oscillation:

r[ctrl.autoscale.separate-field-manager]
The autoscaler uses **separate SSA field managers** from the reconciler (`rio-controller`): `rio-controller-autoscaler` for `StatefulSet.spec.replicas` patches, and `rio-controller-autoscaler-status` for `WorkerPool.status.{lastScaleTime,conditions}` patches. The reconciler owns `status.replicas`/`readyReplicas`/`desiredReplicas`; the autoscaler owns `status.lastScaleTime` + scaling conditions. Using the same field manager would cause 400 "conflict" on concurrent patches.

r[ctrl.autoscale.skip-deleting]
The autoscaler MUST check `metadata.deletionTimestamp` and skip pools being deleted --- otherwise it rescales the StatefulSet while the finalizer is scaling to 0, causing ping-pong.

- Scale-up: react quickly (e.g., 30s window) when queue depth exceeds target.
- Scale-down: react slowly (e.g., 10m window) to avoid killing workers that may be needed again soon.
- Never scale below `WorkerPool.spec.replicas.min`.

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
- **WorkerPool** CRD → reconciles worker StatefulSet (replicas, resources, security context)

The controller does **NOT** manage:
- Scheduler or store Deployments --- these are deployed via Helm/kustomize as standard Deployments
- Rationale: scheduler and store have simple lifecycle (single replica or leader-elected); CRD management adds complexity without benefit

**HPA restriction:** HPA must NOT be configured on worker StatefulSets. The controller manages scaling directly based on scheduler queue depth. Conflicting autoscalers cause oscillation.

## Key Files

- `rio-crds/src/` --- CRD type definitions (separate crate; WorkerPool, WorkerPoolSet)
- `rio-controller/src/reconcilers/` --- Reconciliation loops
- `rio-controller/src/scaling/` --- Autoscaling logic (standalone + per-class)

## CRD Versioning

CRDs follow a `v1alpha1` → `v1beta1` → `v1` progression. The initial implementation uses `v1alpha1` with no stability guarantees. Plan a conversion webhook before promoting to `v1beta1` to support zero-downtime upgrades from `v1alpha1`.

## CRD Validation

CRDs use CEL validation rules (`x-kubernetes-validations`) for structural constraints:

- `spec.replicas.min <= spec.replicas.max`
- `spec.systems` must be non-empty
- `spec.hostNetwork: true` requires `spec.privileged: true` — Kubernetes rejects `hostUsers: false` combined with `hostNetwork: true` at pod admission; the non-privileged path sets `hostUsers: false` per ADR-012

r[ctrl.crd.host-users-network-exclusive]
The controller MUST reject `WorkerPool` specs with `hostNetwork: true` and `privileged` unset or false. Kubernetes admission rejects pod specs combining `hostUsers: false` with `hostNetwork: true` (user-namespace UID remapping is incompatible with the host network namespace). Since the non-privileged path sets `hostUsers: false` unconditionally (ADR-012, `r[sec.pod.host-users-false]`), `hostNetwork: true` implies the `privileged: true` escape hatch. CRD CEL validation enforces this at `kubectl apply` time; the builder additionally suppresses `hostUsers` when the combination is encountered in pre-existing specs (emitting a Warning event).

r[ctrl.event.spec-degrade]
The WorkerPool reconciler MUST emit a `Warning`-type Kubernetes Event for every spec field the builder silently degrades. CEL validation rejects NEW specs with invalid combinations; existing specs applied before the CEL rule landed are defensively corrected at pod-template time (e.g., `hostUsers` suppressed for `hostNetwork: true`). Without a Warning event, the operator has no signal that their spec is stale — `kubectl get wp -o yaml` shows the original value; the pod template shows the corrected value. The Warning names the field, the spec value, and the remediation. Emission happens before the ephemeral/STS-mode branch so both reconcile paths have identical visibility.

`spec.fuseCacheSize` is NOT a CEL rule — it is validated at reconcile time (`parse_quantity_to_gb` in `builders.rs` returns `InvalidSpec` on unparseable input, which fails the reconcile and emits an event).

## WorkerPool Finalizer

r[ctrl.drain.all-then-scale]
WorkerPool CRDs carry a `rio.build/workerpool-drain` finalizer. Before a WorkerPool is deleted, the controller's `cleanup()`:

1. Lists pods by `rio.build/pool=<name>` label; sends `AdminService.DrainWorker` for each (scheduler stops assigning to them). **ALL workers must be drained BEFORE scaling to 0** --- StatefulSet `OrderedReady` terminates pods one-by-one in reverse ordinal, so if you scale before draining, pod-N gets SIGTERM while pod-0 is still receiving assignments.
2. Patches `StatefulSet.spec.replicas = 0`.
3. Waits for all pods to terminate (each worker's SIGTERM handler completes in-flight builds before exit — see Worker Lifecycle above).
4. Removes the finalizer, allowing K8s GC to delete the WorkerPool CRD and its owned children (StatefulSet, Service) via `ownerReference`.

The reconciler's normal `apply()` path short-circuits when `deletionTimestamp` is set (finalizer wraps it) — no need to clamp `min`/`max` to zero as a separate step.
