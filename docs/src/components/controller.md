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
  replicas:
    min: 2
    max: 20
  autoscaling:
    metric: queueDepth
    targetValue: 5                            # scale up when > 5 queued derivations per worker
  resources:
    requests: { cpu: "4", memory: "8Gi" }
    limits: { cpu: "8", memory: "16Gi" }
  maxConcurrentBuilds: 4
  fuseCacheSize: 100Gi                         # local SSD cache for rio-fuse
  features: [big-parallel, kvm]               # maps to requiredSystemFeatures
  systems: [x86_64-linux]
  image: rio-worker:dev                        # required — container image ref
  imagePullPolicy: IfNotPresent                # optional — K8s default if omitted
  sizeClass: small                             # maps to RIO_SIZE_CLASS env
  tlsSecretName: rio-worker-tls                # optional — Secret with tls.crt/tls.key/ca.crt for mTLS
  fodProxyUrl: http://fod-proxy.rio.svc:3128   # optional — injected as http(s)_proxy for FOD builds
  hostNetwork: false                           # optional — true only for VM-test/airgap escapes
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

### WorkerPoolSet

> **Scheduled:** [P0232](../../.claude/work/plan-0232-wps-crd-struct-crdgen.md) (CRD types) + [P0233](../../.claude/work/plan-0233-wps-child-builder-reconciler.md) (reconciler) + [P0229](../../.claude/work/plan-0229-cutoff-rebalancer-gauge-convergence.md) (CutoffRebalancer). Until those land: size-class routing via multiple independent `WorkerPool` CRs, cutoffs static in `scheduler.toml`.

r[ctrl.wps.reconcile]
The WorkerPoolSet reconciler creates one child WorkerPool per `spec.classes[i]`, named `{wps}-{class.name}`, with `ownerReferences[0].controller=true` pointing at the WPS. SSA-apply with force (field manager `rio-controller-wps`). On deletion, the finalizer-wrapped cleanup explicitly deletes children for deterministic timing; k8s ownerRef GC is the fallback.

r[ctrl.wps.cutoff-status]
The WPS reconciler writes per-class `effective_cutoff_secs` + `queued` to WPS status via SSA patch (field manager `rio-controller-wps-status`). Values come from the `GetSizeClassStatus` admin RPC. SSA patch body MUST include `apiVersion` + `kind`.

r[ctrl.wps.autoscale]
Per-class autoscaling: for each WPS child WorkerPool, compute `desired = clamp(queued / target_queue_per_replica, min_replicas, max_replicas)` and SSA-patch `spec.replicas` with field manager `rio-controller-wps-autoscaler` (distinct from the reconciler's field manager — SSA merges field ownership). Skip children with non-nil `deletionTimestamp`.

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
        maxConcurrentBuilds: 8
        fuseCacheSize: 50Gi
    - name: medium
      durationCutoff: 600s
      pool:
        replicas: { min: 2, max: 15 }
        resources:
          requests: { cpu: "4", memory: "8Gi" }
          limits: { cpu: "8", memory: "16Gi" }
        maxConcurrentBuilds: 4
        fuseCacheSize: 100Gi
    - name: large
      durationCutoff: null           # unbounded (everything > 600s)
      pool:
        replicas: { min: 1, max: 10 }
        resources:
          requests: { cpu: "8", memory: "16Gi" }
          limits: { cpu: "16", memory: "32Gi" }
        maxConcurrentBuilds: 2
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

> **Scheduled:** WorkerPoolSet reconciler → [P0233](../../.claude/work/plan-0233-wps-child-builder-reconciler.md); scheduled GC → [P0212](../../.claude/work/plan-0212-gc-automation-cron.md). Until those land: GC is manual via `AdminService.TriggerGC`.

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
K8s readiness probes on gRPC components MUST target a named health check service (e.g., `grpc.health.v1.Health/Check` with `service: rio-scheduler`), not the empty-string default. `set_not_serving` only affects named services, not `""` --- readiness would stay green during drain otherwise.

> **Note:** This requirement is satisfied by the static Helm templates (`infra/helm/rio-build/templates/scheduler.yaml` and `store.yaml`), which set `readinessProbe.grpc.service` to `rio.scheduler.SchedulerService` and `rio.store.StoreService` respectively. It is not controller-managed (the controller does not deploy scheduler/store). Worker probes are HTTP (`/healthz`/`/readyz`) and are unrelated to this rule.

| Component | Liveness | Readiness | Startup |
|---|---|---|---|
| Gateway | TCP check on SSH port | After scheduler gRPC connection established | — |
| Scheduler | gRPC health check | After leader election won + PostgreSQL connected + recovery_complete | gRPC check. Startup budget sized for state recovery (reload non-terminal builds from PG). |
| Store | gRPC health check | After PostgreSQL + S3 reachable | — |
| Controller | HTTP `/healthz` | After CRD watches established | — |
| Worker | HTTP `/healthz` + `/readyz` (no gRPC server) | `/readyz` 200 after first accepted heartbeat | HTTP check, `failureThreshold × periodSeconds ≥ 120s` (FUSE mount + cache warm) |

## Worker Lifecycle

r[ctrl.drain.sigterm]
**Scale-down:** `terminationGracePeriodSeconds` is set to `7200` (2 hours) to allow in-flight builds to complete.

**SIGTERM drain (no preStop hook needed):** the worker's main loop has a `select!` arm on SIGTERM. On signal:

1. Send `AdminService.DrainWorker` to the scheduler (stop accepting new assignments — `has_capacity()` returns false for this worker).
2. `acquire_many(max_builds)` on the build semaphore — succeeds when all in-flight build tasks have dropped their permits (completed + uploaded).
3. Exit 0.

A preStop hook doing the same is redundant: K8s sends SIGTERM on pod termination regardless of preStop, and the worker's signal handler implements the drain. The StatefulSet does NOT define a preStop.

r[ctrl.autoscale.direct-patch]
**Autoscaling:** The controller queries the scheduler's `AdminService.ClusterStatus` gRPC RPC to obtain current queue depth and worker utilization. It directly patches StatefulSet replicas based on this data. This is an internal mechanism, not HPA. The scaling logic requires stabilization windows and anti-flapping thresholds to avoid oscillation:

r[ctrl.autoscale.separate-field-manager]
The autoscaler uses **separate SSA field managers** from the reconciler (`rio-controller`): `rio-controller-autoscaler` for `StatefulSet.spec.replicas` patches, and `rio-controller-autoscaler-status` for `WorkerPool.status.{lastScaleTime,conditions}` patches. The reconciler owns `status.replicas`/`readyReplicas`/`desiredReplicas`; the autoscaler owns `status.lastScaleTime` + scaling conditions. Using the same field manager would cause 400 "conflict" on concurrent patches.

r[ctrl.autoscale.skip-deleting]
The autoscaler MUST check `metadata.deletionTimestamp` and skip pools being deleted --- otherwise it rescales the StatefulSet while the finalizer is scaling to 0, causing ping-pong.

- Scale-up: react quickly (e.g., 30s window) when queue depth exceeds target.
- Scale-down: react slowly (e.g., 10m window) to avoid killing workers that may be needed again soon.
- Never scale below `WorkerPool.spec.replicas.min`.

## GC Cron

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

- `rio-controller/src/crds/` --- CRD type definitions
- `rio-controller/src/reconcilers/` --- Reconciliation loops
- `rio-controller/src/scaling.rs` --- Autoscaling logic

## CRD Versioning

CRDs follow a `v1alpha1` → `v1beta1` → `v1` progression. The initial implementation uses `v1alpha1` with no stability guarantees. Plan a conversion webhook before promoting to `v1beta1` to support zero-downtime upgrades from `v1alpha1`.

## CRD Validation

CRDs use CEL validation rules (`x-kubernetes-validations`) for structural constraints:

- `spec.replicas.min <= spec.replicas.max`
- `spec.maxConcurrentBuilds >= 1`
- `spec.systems` must be non-empty

`spec.fuseCacheSize` is NOT a CEL rule — it is validated at reconcile time (`parse_quantity_to_gb` in `builders.rs` returns `InvalidSpec` on unparseable input, which fails the reconcile and emits an event).

## WorkerPool Finalizer

r[ctrl.drain.all-then-scale]
WorkerPool CRDs carry a `rio.build/workerpool-drain` finalizer. Before a WorkerPool is deleted, the controller's `cleanup()`:

1. Lists pods by `rio.build/pool=<name>` label; sends `AdminService.DrainWorker` for each (scheduler stops assigning to them). **ALL workers must be drained BEFORE scaling to 0** --- StatefulSet `OrderedReady` terminates pods one-by-one in reverse ordinal, so if you scale before draining, pod-N gets SIGTERM while pod-0 is still receiving assignments.
2. Patches `StatefulSet.spec.replicas = 0`.
3. Waits for all pods to terminate (each worker's SIGTERM handler completes in-flight builds before exit — see Worker Lifecycle above).
4. Removes the finalizer, allowing K8s GC to delete the WorkerPool CRD and its owned children (StatefulSet, Service) via `ownerReference`.

The reconciler's normal `apply()` path short-circuits when `deletionTimestamp` is set (finalizer wraps it) — no need to clamp `min`/`max` to zero as a separate step.
