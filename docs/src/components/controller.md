# rio-controller

Manages rio-build lifecycle on Kubernetes via CRDs.

## CRDs

### Build

```yaml
apiVersion: rio.build/v1alpha1
kind: Build
metadata:
  name: my-build-001
spec:
  derivation: /nix/store/abc...-hello.drv   # must be a store path (evaluation is external)
  priority: 100                               # inter-build priority
  timeout: 3600s
  tenant: team-infra
status:
  phase: Building                             # Pending/Building/Succeeded/Failed
                                               # Evaluating is an optional phase set by external eval
                                               # orchestrators; the controller does not set this phase itself
  totalDerivations: 47
  completedDerivations: 31
  cachedDerivations: 12
  criticalPathRemaining: 45s
  startedAt: 2026-02-13T06:00:00Z
  workers: [worker-0, worker-2, worker-5]
  conditions:
    - type: Scheduled                          # also: InputsResolved, Building, Succeeded, Failed
      status: "True"
      lastTransitionTime: 2026-02-13T06:00:00Z
      reason: WorkerAssigned
      message: "Build scheduled on worker-0"
```

Condition types: `Scheduled`, `InputsResolved`, `Building`, `Succeeded`, `Failed`. Each carries `lastTransitionTime`, `reason`, and `message`. `Succeeded` and `Failed` are terminal and mutually exclusive.

### WorkerPool

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
  securityContext:
    capabilities:
      add: [SYS_ADMIN, SYS_CHROOT]            # NOT privileged: true
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
    - type: ScalingActive                      # also: AtMaxCapacity, AtMinCapacity
      status: "True"
      lastTransitionTime: 2026-02-13T05:55:00Z
      reason: QueueDepthAboveTarget
```

### WorkerPoolSet

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

- **WorkerPool reconciler**: scale worker StatefulSet based on scheduler queue depth. Create/delete pods. Manage per-worker ephemeral storage for the FUSE cache. All resources created by this reconciler carry `ownerReferences` to the WorkerPool CRD with `controller: true`, ensuring garbage collection on WorkerPool deletion.
- **WorkerPoolSet reconciler**: manages multiple `WorkerPool` sub-resources (one per size class). Queries the scheduler's `CutoffRebalancer` for learned cutoffs and updates the `WorkerPoolSet` status. Autoscales each class independently based on per-class queue depth from the scheduler.
- **Build reconciler**: create Build CRDs from API/webhook triggers, track status, update conditions.
- **GC reconciler**: trigger store garbage collection on schedule, clean up completed Build resources.

## RBAC

The controller requires a dedicated ServiceAccount with a ClusterRole granting:

| API Group | Resources | Verbs |
|---|---|---|
| `rio.build` | Build, WorkerPool | get, list, watch, create, update, patch, delete |
| `rio.build` | Build/status, WorkerPool/status | get, patch |
| `apps/v1` | StatefulSets | get, list, watch, create, update, patch, delete |
| `core/v1` | Pods | get, list, watch |
| `core/v1` | Services, ConfigMaps | get, list, watch, create, update, patch, delete |
| `core/v1` | Events | create, patch |
| `policy/v1` | PodDisruptionBudgets | get, list, watch, create, update, patch, delete |
| `networking.k8s.io/v1` | NetworkPolicies | get, list, watch, create, update, patch, delete |
| `coordination.k8s.io/v1` | Leases | get, list, watch, create, update |

## NetworkPolicy

The controller provisions NetworkPolicy resources for each component:

- **Workers**: egress to rio-scheduler and rio-store only (gRPC ports). No access to the Kubernetes API server or cloud metadata service (`169.254.169.254`). DNS egress to kube-system (CoreDNS) required for service resolution.
- **Gateway**: ingress from external (Service type LoadBalancer/NodePort for SSH). Egress to rio-scheduler and rio-store. DNS egress to kube-system.
- **Scheduler**: egress to PostgreSQL. DNS egress to kube-system.
- **Store**: egress to PostgreSQL and S3. DNS egress to kube-system.
- **Controller**: egress to rio-scheduler (gRPC, for `AdminService.ClusterStatus` autoscaling queries) and to the Kubernetes API server (for CRD watches and StatefulSet management). DNS egress to kube-system.

## PodDisruptionBudget

The controller creates PDBs for each component:

| Component | Policy | Rationale |
|---|---|---|
| Workers | `minAvailable` based on `WorkerPool.spec.replicas.min` | Maintain minimum build capacity during node drain |
| Scheduler | `maxUnavailable: 1` | Leader election handles failover; at most one pod unavailable |
| Gateway | `minAvailable: 1` | At least one pod must remain for SSH connectivity |

## Service Definitions

| Service | Type | Purpose |
|---|---|---|
| `rio-gateway` | LoadBalancer or NodePort | SSH ingress for `nix copy` / `nix build --store ssh://` |
| `rio-scheduler` | ClusterIP | Internal gRPC for workers, gateway, and controller |
| `rio-store` | ClusterIP + optional Ingress | gRPC for internal components; HTTP for binary cache serving |
| Workers (headless) | ClusterIP (headless) | Individual pod addressing; scheduler tracks workers by pod name |

## Health Probes

| Component | Liveness | Readiness | Startup |
|---|---|---|---|
| Gateway | TCP check on SSH port | After scheduler gRPC connection established | — |
| Scheduler | gRPC health check | After leader election won + PostgreSQL connected | gRPC check, `failureThreshold × periodSeconds ≥ 60s` (state recovery) |
| Store | gRPC health check | After PostgreSQL + S3 reachable | — |
| Controller | HTTP `/healthz` | After CRD watches established | — |
| Worker | gRPC health check to scheduler | After overlay mounted + registered with scheduler | gRPC check, `failureThreshold × periodSeconds ≥ 120s` (FUSE mount + cache warm) |

## Worker Lifecycle

**Scale-down:** `terminationGracePeriodSeconds` is set to `7200` (2 hours) to allow in-flight builds to complete.

**preStop hook sequence:**

1. Deregister from scheduler (stop accepting new builds).
2. Wait for current build(s) to complete.
3. Upload build outputs to rio-store.
4. Exit cleanly.

**Autoscaling:** The controller queries the scheduler's `AdminService.ClusterStatus` gRPC RPC to obtain current queue depth and worker utilization. It directly patches StatefulSet replicas based on this data. This is an internal mechanism, not HPA. The scaling logic requires stabilization windows and anti-flapping thresholds to avoid oscillation:

- Scale-up: react quickly (e.g., 30s window) when queue depth exceeds target.
- Scale-down: react slowly (e.g., 10m window) to avoid killing workers that may be needed again soon.
- Never scale below `WorkerPool.spec.replicas.min`.

## Build CRD Lifecycle

Build CRDs are created by:
1. **The gateway** (for SSH-initiated builds, optional) --- the gateway can create a Build CRD for visibility/tracking, but SSH-initiated builds always go through the scheduler's PostgreSQL directly.
2. **External systems** via `kubectl` or the Kubernetes API --- for K8s-native workflows that want to submit builds programmatically.

SSH-initiated builds do NOT require Build CRDs; Build CRDs are an alternative submission path for Kubernetes-native workflows.

**Finalizer:** All Build CRDs carry a `rio.build/build-cleanup` finalizer. Before a Build CRD is deleted, the controller sends `CancelBuild` to the scheduler to ensure in-flight work is properly handled.

## Component Deployment Model

The controller manages:
- **WorkerPool** CRD → reconciles worker StatefulSet (replicas, resources, security context)
- **Build** CRD → tracks lifecycle, updates status conditions

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
- `spec.timeout > 0`
- `spec.maxConcurrentBuilds >= 1`
- `spec.fuseCacheSize` must parse as a valid Kubernetes quantity
- `spec.systems` must be non-empty

## WorkerPool Finalizer

WorkerPool CRDs carry a `rio.build/workerpool-drain` finalizer. Before a WorkerPool is deleted, the controller:

1. Sets `spec.replicas.min` and `spec.replicas.max` to 0 (prevents new pods).
2. Sends drain requests to all running workers (deregister from scheduler, finish in-flight builds).
3. Waits for all workers to complete and upload outputs.
4. Deletes the StatefulSet and associated resources.
5. Removes the finalizer, allowing the WorkerPool CRD to be garbage-collected.
