#import "/lib/rio.typ": *
#show: rio.with(domains: ("ctrl", "store"))


Manages rio-build lifecycle on Kubernetes via CRDs.

= CRDs

== Pool

#r("ctrl.crd.pool+1")[
  ```yaml
  apiVersion: rio.build/v1alpha1
  kind: Pool
  metadata:
    name: x86-64
  spec:
    kind: Builder                                # Builder | Fetcher, required
    maxConcurrent: 20                            # u32?, optional — concurrent-Job ceiling (one Job = one build); omit = uncapped Job count (the §13b placeable gate + per-class maxFleetCores caps bound fanout, not Job count)
    image: rio-builder:dev                       # string, required — container image ref
    systems: [x86_64-linux]                      # list<string>, required (non-empty per CEL)
    hostUsers: false                             # bool?, optional — None ⇒ hostUsers:false (userns per ADR-012); CEL-forbidden for Fetcher
    nodeSelector:
      rio.build/builder: "true"
    tolerations:
      - { key: rio.build/builder, operator: Equal, value: "true", effect: NoSchedule }
    features: [big-parallel, kvm]                # list<string>, default [] — maps to requiredSystemFeatures (Builder-only)
    imagePullPolicy: IfNotPresent                # string?, optional — K8s default if omitted
    fuseThreads: 4                               # u32?, optional — Builder-only, CEL-forbidden for Fetcher; the only FUSE knob left post-castore-cutover (ADR-022 P0560)
    terminationGracePeriodSeconds: 7200          # i64?, optional — K8s grace; defaults per-kind (r[ctrl.pod.tgps-default])
    privileged: false                            # bool?, optional — CEL-forbidden for Fetcher
    hostNetwork: false                           # bool?, optional — true requires privileged:true; CEL-forbidden for Fetcher
    seccompProfile:                              # SeccompProfileKind?, optional — CEL-forbidden for Fetcher
      type: Localhost                            #   RuntimeDefault | Localhost | Unconfined
      localhostProfile: operator/rio-builder.json#   required iff type=Localhost (r[ctrl.crd.seccomp-cel])
    # NOT CRD fields: resources (per-pod cpu/mem/disk come from the
    # scheduler's per-drv SpawnIntent — ADR-023); securityContext (caps
    # hardcoded in build_executor_pod_spec()); topologySpread (one-shot
    # Jobs don't anti-affine); cache sizing (the castore digest/chunk
    # caches are node-level hostPaths owned by rio-mountd, not per-pod
    # — ADR-022 P0560).
  status:
    replicas: 5                                  # i32 — active Jobs
    readyReplicas: 4                             # i32 — Jobs whose pod passed readinessProbe
    desiredReplicas: 8                           # i32 — concurrent-Job target the reconciler is converging on
    conditions:                                  # []Condition — see r[ctrl.condition.sched-unreachable]
      - type: SchedulerUnreachable
        status: "False"
        reason: ClusterStatusOK
  ```
]

== Job lifecycle

#r("ctrl.pool.ephemeral+1")[
  The reconciler polls `AdminService.ClusterStatus` each requeue tick (10s) and
  spawns K8s Jobs when `queued_derivations > 0` and active Jobs <
  `spec.maxConcurrent` (or unconditionally when `maxConcurrent` is unset). Each
  Job runs one rio-builder pod whose main loop exits after one
  `CompletionReport` → pod terminates → Job goes Complete →
  `ttlSecondsAfterFinished: 600` reaps (10min postmortem window for `kubectl
  logs` on failed builders). Job settings: `backoffLimit: 0` (scheduler owns
  retry), `restartPolicy: Never`, `parallelism: 1`. `spec.maxConcurrent` is an
  optional concurrent-Job ceiling, not a standing set; when omitted, fanout is
  bounded provisioning-side --- the §13b placeable gate (#glspl("pool") spawn
  Jobs only for FFD-placed-on-`Registered` intents) and `cover_deficit`'s
  per-class `maxFleetCores` budget caps bound the NodeClaim mint, not the Job
  count. The Karpenter NodePool is NOT a fanout gate post-§13b:
  `rio-nodeclaim-shim` carries `limits.cpu:0` and the controller mints
  NodeClaims directly. Zero queued derivations means zero pods.
]

*Isolation guarantee:* zero cross-build state inside the pod. Fresh pod means a
fresh emptyDir for the @overlayfs upper and a fresh per-build castore-FUSE
mount, fresh filesystem --- there is no "subsequent build" on that pod. The
node-shared castore caches (`/var/rio/{cache,chunks}`) are mounted read-only
and admit content only through rio-mountd's digest-verified Promote, so an
untrusted build cannot poison them either. Strongest isolation when combined
with `hostUsers: false` + non-privileged (see
#rref("sec.pod.host-users-false")).

*Cost:* per-build cold start (pod scheduling + container pull + FUSE mount +
scheduler registration --- typically 10--30s) plus one reconcile tick (\~10s)
before the Job is spawned. Nodes outlive pods (@karpenter consolidation policy),
so the node-level FSx cache survives pod churn --- the cold-start cost is pod
overhead, not refetching the @closure.

*Dispatch path:* the scheduler sees a Job pod heartbeat in, dispatches one
derivation, receives `CompletionReport`, then the pod disconnects. The active
mechanism is ClusterStatus polling; a push-mode RPC was considered and rejected
(see `ephemeral.rs` § Why not a Scheduler→Controller RPC).

*RBAC:* the controller's ClusterRole grants `batch/jobs` verbs `[get, list,
watch, create, delete]`. `delete` is required for the excess-Pending reap
(#rref("ctrl.ephemeral.reap-excess-pending")) and the orphan-Running reap.
`ttlSecondsAfterFinished` reaps Completed/Failed Jobs; ownerRef GC handles
pool-delete cleanup.

#r("ctrl.ephemeral.reap-excess-pending+3")[
  When the per-class queued count drops below the count of Pending-phase Jobs
  for that class, the controller MUST delete the excess Pending Jobs
  (orphan-by-intent first; residual excess oldest-first). Running Jobs are not
  touched --- those have or may receive assignments; the scheduler handles them
  via cancel-on-disconnect. "Pending" is `JobStatus.ready == 0` with
  `parallelism: 1` and no readiness probe: the pod has not been scheduled, or
  is scheduled but the container has not started --- either way it has never
  connected to the scheduler and never received an assignment, so deletion
  loses no work. Jobs younger than one requeue tick (10s) are excluded ---
  `JobStatus.ready` is set asynchronously by the K8s Job controller and can lag
  a freshly-started container. Before issuing each DELETE the controller MUST
  re-check `Pod.status.phase` via a live (non-informer) lookup and skip the Job
  if any pod is `Running`: after a Karpenter cold start the 10s creation-age
  grace is already exhausted when the pod first runs, and the scheduler's
  `queued` count drops on assignment before `JobStatus.ready` propagates. The
  reap is *skipped entirely* when the queued poll failed (scheduler
  unreachable): spawn treats that as `queued=0` (fail-open: don't spawn); reap
  MUST treat it as unknown (fail-closed: don't delete). Without this reap, a
  cancelled build leaves already-spawned Pending Jobs sitting until
  `activeDeadlineSeconds` (default 1h), and Karpenter keeps provisioning nodes
  for them.
]

#r("ctrl.ephemeral.reap-orphan-running+3")[
  When a Running Job (`JobStatus.ready > 0`) is older than the orphan grace
  (default 5min) AND the scheduler does not consider its executor busy ---
  either the pod's `executor_id` is absent from `ListExecutors`, or present
  with `busy == false` --- the controller MUST delete the Job. This is the
  controller-side backstop for I-165: a builder process stuck in
  uninterruptible sleep (D-state FUSE wait, OOM-loop) cannot self-exit via the
  120s `RIO_IDLE_SECS` idle-timeout, never disconnects from the scheduler, and
  would otherwise sit until `activeDeadlineSeconds` (default 1h). The grace
  MUST exceed the builder's idle-timeout so the process-level exit is given
  first chance; the controller reap fires only when the process cannot act on
  its own. A Job whose executor reports `busy == true` is NOT reaped --- the
  scheduler believes a build is in progress; `activeDeadlineSeconds` is the
  backstop for stuck-mid-build. The reap is *skipped entirely* when
  `ListExecutors` fails, returns empty, *or reports `leader_for_secs` below the
  orphan grace* (scheduler unreachable, new-leader pre-reconnect, or
  partial-reconnect window) --- fail-closed, same posture as
  #rref("ctrl.ephemeral.reap-excess-pending"). The new leader's executor map
  fills incrementally as workers reconnect over a 1--10s spread; a non-empty
  partial list cannot prove absence.
]

*Cleanup:* the finalizer's `cleanup()` returns immediately. In-flight Jobs
finish their one build naturally; ownerRef GC removes them after the pool is
gone.

#r("ctrl.ephemeral.intent-deadline")[
  Jobs MUST set `spec.activeDeadlineSeconds` to `SpawnIntent.deadline_secs`
  verbatim (floored at 180 as defense against the proto default). The scheduler
  computes `deadline_secs` per-derivation (D7): for fitted keys, `wall_p99 * 5`
  at the solved core count; for unfitted (probe/explore),
  `[sla].probe.deadline_secs`; both clamped `[resource_floor.deadline_secs,
  86400]`. `SlaConfig::validate` enforces `probe.deadline_secs >= 180`, so the
  intent value is always positive --- the controller has no per-kind fallback.
  A `DeadlineExceeded` kill triggers #rref("ctrl.terminated.deadline-exceeded")
  → `bump_floor_or_count` doubles `floor.deadline_secs` → the next intent gets
  a longer `activeDeadlineSeconds`. The 5× headroom is scheduler-side; the
  controller adds no multiplier or margin. Backstop purpose: a wrong-pool spawn
  (executor heartbeats in, never matches dispatch) would hang indefinitely
  without it; K8s kills at deadline, `backoffLimit: 0` marks Failed,
  `ttlSecondsAfterFinished` reaps.
]

#r("ctrl.terminated.deadline-exceeded+2")[
  The Job-mode reconciler MUST report each Job with `status.conditions`
  containing `type=Failed, reason=DeadlineExceeded` to the scheduler via
  `AdminService.ReportExecutorTermination{executor_id = job.metadata.name,
  reason = DeadlineExceeded}`. With `restartPolicy:Never` + `backoffLimit:0` +
  the `job-tracking` finalizer the SIGKILL'd Pod IS listable with
  `terminated.reason="Error"` for the grace window, but `Error` is
  non-promoting so `report_terminated_pods` skips it; the Job condition is
  observable for `JOB_TTL_SECS=600` (\~60 reconcile ticks). The scheduler
  prefix-matches the Job name against its `recently_disconnected` map
  (#rref("sched.termination.deadline-exceeded")). Iterates the already-listed
  `jobs.items` --- no extra apiserver call. Best-effort (RPC error logged,
  reconcile continues). Defense-in-depth behind the worker-side `daemon_timeout`
  → `TimedOut` primary path.
]

#r("ctrl.pod.arch-selector+2")[
  When the pool's `spec.systems` resolves to a single _host_ CPU architecture,
  the controller MUST inject `kubernetes.io/arch={amd64|arm64}` into the Job
  pod's `nodeSelector` (operator-set value wins via `or_insert`). 32-bit guest
  systems map to their 64-bit host (`i686`→`amd64`, `armv7l`/`armv6l`→`arm64`)
  so an `extra-platforms` pool like `[x86_64-linux, i686-linux]` still
  constrains to amd64. Applies to BOTH Builder and Fetcher Pools (r35 bug_039):
  fetchers run `builtin` (arch-agnostic) AND arch-typed FODs from
  `pool.spec.systems` --- the `systems`→arch resolution skips `builtin`, so a
  `["builtin"]`-only fetcher Pool stays arch-agnostic. Without this, an
  `x86_64-linux` pool can land on an arm64 node (unconstrained fallback
  NodePool --- I-098), register as `x86_64` from `RIO_SYSTEMS`, accept
  dispatch, and have the local nix-daemon refuse the build. Multi-arch and
  `builtin`-only pools get no selector and rely on the executor's startup arch
  check (applied to BOTH kinds since r35) as the safety net.
]

#r("ctrl.pod.tgps-default")[
  The Job pod spec MUST default `terminationGracePeriodSeconds` to `7200` (2h)
  when `PoolSpec.terminationGracePeriodSeconds` is unset. SIGTERM → executor
  drain (#rref("ctrl.drain.sigterm")) waits for the in-flight build to complete
  before exit; nix builds can legitimately take 2h (LLVM, full NixOS closure
  from cold cache). Clusters with known-shorter builds set this lower so pool
  deletion doesn't stall on a stuck pod for 2h.
]

#r("ctrl.pool.kvm-device+2")[
  When `PoolSpec.features` intersects the set of features that route to a
  kvm-tainted hw-class --- `{"kvm"} ∪ ⋃_{h: kvm-tainted} provides_features(h)`,
  per `HwClassConfig::features_routing_to_taint` --- the controller MUST append
  a toleration for every taint of every kvm-tainted hw-class (per
  `HwClassConfig::taints_routing_to`), with the literal
  `rio.build/kvm=true:NoSchedule` as the unloaded-config floor. The controller
  MUST NOT add a pool-static `rio.build/kvm` nodeSelector: the toleration is
  _permissive_ (over-firing when the predicate mis-predicts is a harmless
  unused grant), but a nodeSelector is _restrictive_ (over-firing removes
  nodes) and `pool.spec.features` is not a universal predicate over the Pool's
  intents --- a feature shared by metal + non-metal hw-classes routes some
  intents to non-metal cells, where a pool-static nodeSelector would deadlock
  against the per-intent affinity. Restrictive metal placement is the
  per-intent `nodeAffinity` only (#rref("ctrl.pool.node-affinity-from-intent"));
  the toleration is the cold-start fallback for `hw_class_names=[]` intents
  whose per-intent toleration loop (#rref("ctrl.pool.intent-tolerations"))
  produces nothing. containerd `base_runtime_spec` injects `/dev/kvm` into
  every pod's `/dev` (same mechanism as #rref("sec.pod.fuse-device-plugin")),
  but only EC2 `.metal` instance types expose host KVM (nested virt) --- on
  non-metal the device node ENXIOs on open. The toleration is unconditional wrt
  `privileged` so privileged kvm pods still tolerate the metal taint.
]

== Reconciler

#r("ctrl.pool.reconcile")[
  One reconciler handles both kinds. Each tick: poll
  `GetSpawnIntents{kind=spec.kind, systems, features}` → spawn one Job per
  intent (resources stamped from the intent) up to `spec.maxConcurrent` → reap
  excess Pending / orphan Running → patch `.status`. Finalizer-wrapped;
  ownerRef GC handles Job cleanup on Pool delete.
]

#r("ctrl.pool.fetcher-hardening+2")[
  For `kind=Fetcher`, `executor_params` MUST apply ADR-019 hardening regardless
  of spec: `readOnlyRootFilesystem: true`, `seccompProfile: Localhost
  operator/rio-fetcher.json`, `hostUsers: false`, `privileged: false`, default
  `rio.build/fetcher: true` nodeSelector (§13e key, restored in B4) +
  `rio.build/fetcher:NoSchedule` toleration, `terminationGracePeriodSeconds:
  600`. CRD CEL rejects fetcher specs that set the overridden fields at
  admission time; the reconciler override is belt-and-suspenders for pre-CEL
  specs the apiserver already accepted.
]

#r("ctrl.pool.fetcher-spawn-builtin")[
  For `kind=Fetcher` pools, `spec.systems` SHOULD include `"builtin"` so
  `system="builtin"` FODs are counted in the spawn signal. Every executor
  unconditionally advertises `"builtin"`
  (#rref("sched.dispatch.fod-builtin-any-arch")); omitting it from the spawn
  signal would stall a cold-store bootstrap.
]

#r("ctrl.pool.disruption")[
  The DisruptionTarget watcher selects on `POOL_LABEL` (which the reconciler
  stamps on every kind=Builder AND kind=Fetcher pod), so fetcher pods gain the
  same fast-preemption behavior as builders: K8s sets `DisruptionTarget=True` →
  `DrainExecutor{force:true}` → reassign in seconds instead of burning the
  grace period.
]

== ComponentScaler

#r("ctrl.crd.componentscaler")[
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
]

Why not k8s #gls("hpa"): no metrics-server / custom.metrics.k8s.io adapter in-cluster,
and the controller already has the demand signal (`ClusterStatus`). See
#rref("ctrl.scaler.component+2") / #rref("ctrl.scaler.ratio-learn") for
reconciler behavior.

= Reconciliation Loops

#r("ctrl.admin.rpc-timeout")[
  Every `AdminServiceClient` RPC issued from a controller reconcile or watcher
  loop MUST be bounded by a 5-second timeout. `build_endpoint` sets
  `.connect_timeout()` only; h2 keepalive detects dead transport (\~40s) but
  not a live-but-stalled scheduler (actor mailbox backlog, slow PG). A hung
  await would block the watcher loop indefinitely (missed `DisruptionTarget`
  fast-preemption) or block the kube-runtime reconciler with no requeue. On
  timeout the call site treats it as the RPC's existing `Err` arm (best-effort:
  log + continue / requeue). The bound is NOT applied at the channel level ---
  builder/gateway data-plane RPCs (long-poll, streaming) legitimately exceed
  5s.
]

#r("ctrl.pool.hw-class-annotation")[
  The `rio.build/hw-class` pod annotation MUST be exposed to the builder via a
  downward-API *volume* (`/etc/rio/downward/hw-class`), not an env var. The
  annotation is stamped reactively by `run_pod_annotator` after `spec.nodeName`
  binds; the env-var form resolves once at container-create and races the
  annotator permanently. The builder reads the file with a bounded 30s poll
  (`hw_class::resolve`) so a late stamp still reaches a running pod.
]

#r("ctrl.pool.node-affinity-from-intent")[
  When `SpawnIntent.node_affinity` is non-empty the pool reconciler MUST set
  `pod.spec.affinity.nodeAffinity.
  requiredDuringSchedulingIgnoredDuringExecution.nodeSelectorTerms` to the
  proto terms (field-by-field copy). Empty `node_affinity` MUST leave
  `spec.affinity` unset so non-hw-targeted intents (Static-mode, FOD,
  feature-gated) bin-pack freely (I-090).
]

#r("ctrl.pool.intent-tolerations")[
  For every `h ∈ SpawnIntent.hw_class_names` the pool reconciler MUST append a
  toleration for every taint in `[sla.hw_classes.$h].taints` (via
  `HwClassConfig::taints_for(h)`) --- the same map `cover::build_nodeclaim`
  uses to taint Nodes --- deduplicated against the pod's existing tolerations.
  The intent's `node_affinity` (#rref("ctrl.pool.node-affinity-from-intent"))
  pins the pod to nodes carrying the hw-class labels, including taint-paired
  keys; without the matching toleration the affinity-pinned pod is permanently
  Pending (`TaintToleration` rejects every node the affinity allows). Deriving
  tolerations from the same `[sla.hw_classes.$h]` source as the taint producer
  means a future tainted hw-class (`gpu`, `secure-boot`) routes its toleration
  automatically. This covers fitted intents (`hw_class_names ≠ []`); the
  pool-static toleration (#rref("ctrl.pool.kvm-device+2")) covers
  `hw_class_names=[]` cold-start intents.
]

#r("ctrl.pool.fetcher-affinity-from-intent+5")[
  The fetcher pod's restrictive placement constraint MUST be the merge of the
  per-intent `nodeAffinity` derived from `intent.hw_class_names` via
  `cells_to_selector_terms` (#rref("ctrl.pool.node-affinity-from-intent")) AND
  the pool-static `nodeSelector{rio.build/fetcher: true}` (§13e B4) AND the
  operator's `pool.spec.node_selector` (r35 bug_044). The operator selector
  ADDS constraints (AZ pin, instance type); it MUST NOT replace or weaken the
  pool-static fetcher constraint --- the controller unconditionally inserts
  `rio.build/fetcher: true` after the operator-supplied keys, and the CEL
  admission rule rejects an operator-set `nodeSelector["rio.build/fetcher"]`
  value other than `"true"`. The pool-static and per-intent constraints cannot
  drift: the pool-static constraint keys on `pool.spec.kind == Fetcher` (a
  Pool-level invariant), and the per-intent affinity (`intent.hw_class_names ∩
  {fetcher-*}`) is a PROJECTION of that invariant, not an independent opinion
  of it. This is NOT the #rref("ctrl.pool.kvm-device+2")/r33-bug_002 redundancy
  anti-pattern: the kvm nodeSelector keyed on `pool.spec.features` (existential
  --- an intent may need only one feature, which routes to a non-metal class),
  so the two places COULD disagree. Here they cannot. The pool-static
  constraint is the LAST-RESORT restrictive constraint for the window between
  intent-emit and node-provision: the per-intent affinity is also present once
  `hw_class_names` is populated by the arch-agnostic feature fall-through (r35
  B1 --- `system="builtin"` FODs route by `required_features=["fetcher"]`; the
  arch axis is a no-op for arch-unmappable systems). See
  #rref("ctrl.pool.fetcher-tolerations") for the tolerations dual.
]

#r("ctrl.pool.fetcher-tolerations")[
  The fetcher pod's tolerations MUST be the merge of the pool-static fetcher
  tolerations (every taint of every hw-class carrying `rio.build/fetcher`, per
  `HwClassConfig::taints_routing_to`, with the literal
  `rio.build/fetcher:Exists:NoSchedule` as the unloaded-config floor) AND the
  operator's `pool.spec.tolerations` (r37 bug_001), deduplicated. The operator
  MUST NOT be able to drop the pool-static set:
  #rref("ctrl.pool.fetcher-affinity-from-intent+5") makes the
  `{rio.build/fetcher: true}` nodeSelector unconditional, so a fetcher pod is
  pinned to nodes carrying `rio.build/fetcher:NoSchedule` --- a missing
  toleration is a permanent Pending with no warn/metric, not a "harmless
  permissive over-fire." Tolerations are purely additive (no operator-set value
  defeats an unconditional merge), so no CEL admission guard is needed.
]

#r("ctrl.pool.builder-tolerations")[
  The builder pod's tolerations MUST be the merge of the structural builder
  toleration (`rio.build/builder=true:NoSchedule`, the taint
  `cover::builder_taint()` stamps on every builder cell's NodeClaim) and the
  operator's `pool.spec.tolerations` (r38 bug_027 --- sibling of
  #rref("ctrl.pool.fetcher-tolerations")), deduplicated. The operator MUST NOT
  be able to drop the structural toleration: the per-intent `nodeAffinity`
  (#rref("ctrl.pool.node-affinity-from-intent")) pins the pod to cover-minted
  nodes, every one of which carries the builder taint --- a missing toleration
  is a permanent Pending with no warn/metric. Helm `mergeOverwrite` replaces
  list-typed `tolerations:`, so the merge MUST happen controller-side.
]

#r("ctrl.pool.hw-bench-needed+2")[
  The pool reconciler MUST stamp `rio.build/hw-bench-needed` on the pod
  template at create time and expose it as `RIO_HW_BENCH_NEEDED` via a
  downward-API env var. The annotation is `"true"` iff `intent.mem_bytes ≥
  hw_bench_mem_floor` AND any `h` in the intent's admissible-set `A` (from
  `intent.hw_class_names`) has fewer than `HwClassSampledResponse.
  trust_threshold` distinct tenants in any K=3 dimension per
  `AdminService.HwClassSampled` (one RPC per reconcile tick over the union of
  all `A`). When `A` is empty the annotation MUST be `"false"` --- the actual
  `h` is unknown until kube-scheduler bind. On RPC failure unknown `h` MUST
  read as 0 (over-bench, never under-bench); on `trust_threshold` field absence
  (old-scheduler skew) the controller MUST fall back to `5`.
]

#r("ctrl.reconcile.owner-refs")[
  - *Pool reconciler*: spawn/reap one-shot Jobs (builder or fetcher per
    `spec.kind`) based on scheduler `SpawnIntent`s. All Jobs carry
    `ownerReferences` to the Pool CRD with `controller: true`, ensuring garbage
    collection on Pool deletion.
  - *GC reconciler*: trigger store garbage collection on schedule.
]

#r("ctrl.backoff.per-object")[
  `error_policy` requeues transient reconcile errors with per-object
  exponential backoff: `5s × 2^(n-1)` capped at `300s`, keyed by
  `{kind}/{ns}/{name}`. A persistent apiserver 5xx backs off to the cap in \~6
  rounds (5→10→20→40→80→160→300s) instead of retrying every 30s indefinitely.
  The counter resets on the next successful reconcile so a fresh failure
  restarts the curve from 5s. `Error::InvalidSpec` is NOT on the curve --- it
  requeues at a fixed per-reconciler interval (300s for pools, 30s for
  ComponentScaler where 5min of no scaling under a builder burst is the I-105
  cliff).
]

#r("ctrl.condition.sched-unreachable")[
  `Pool.status.conditions[]` MUST carry a `SchedulerUnreachable` condition
  reflecting the reconciler's poll-phase RPC result: `status="True",
  reason="ClusterStatusFailed"` with the gRPC error in `message` when the poll
  failed; `status="False", reason="ClusterStatusOK"` otherwise. Written every
  reconcile (SSA with the `rio-controller-ephemeral` field manager owns the
  condition --- omitting it would leave a stale `True` after recovery).
  `lastTransitionTime` is preserved across same-status writes so operators see
  when the scheduler actually went down, not "\~10s ago". Without this,
  `replicas=0` is indistinguishable between "scheduler idle, queued=0" and
  "scheduler down, queued unknown".
]

= RBAC

The controller requires a dedicated ServiceAccount with a ClusterRole granting
(see `infra/helm/rio-build/templates/rbac.yaml`):

#figure(
  table(
    columns: 3,
    align: (left, left, left),
    table.header([API Group], [Resources], [Verbs]),
    [`rio.build`], [pools], [get, list, watch, create, update, patch, delete],
    [`rio.build`], [componentscalers], [get, list, watch, patch, update],
    [`rio.build`], [pools/status], [get, patch],
    [`rio.build`], [componentscalers/status], [get, patch, update],
    [`rio.build`],
    [pools/finalizers],
    [update --- `OwnerReferencesPermissionEnforcement` checks this when
      creating children with `blockOwnerDeletion: true`],

    [`apps`],
    [deployments],
    [get, list, watch --- ComponentScaler reads current `.spec.replicas`],

    [`apps`],
    [deployments/scale],
    [get, patch, update --- ComponentScaler `/scale` subresource patch],

    [`""` (core)],
    [pods],
    [get, list, watch, patch --- `patch` for node_informer's
      `rio.build/hw-class` annotation stamp],

    [`""` (core)],
    [nodes],
    [get, list, watch --- node_informer `NodeLabelCache` (hw-band label join)],

    [`""` (core)],
    [events],
    [list, watch --- node_informer spot-interrupt watcher
      (`reason=SpotInterrupted`)],

    [`events.k8s.io`], [events], [create, patch],
    [`batch`], [jobs], [get, list, watch, create, delete],
    [`karpenter.sh`],
    [nodeclaims],
    [get, list, watch, create, delete --- `nodeclaim_pool` reconciler (ADR-023
      §13b)],

    [`coordination.k8s.io`],
    [leases],
    [create (any), get/update (scoped to `nodeclaim_pool.lease_name` via
      `resourceNames`) --- namespaced Role `rio-controller-lease`],
  ),
)

Lease permissions (`coordination.k8s.io/leases`) are granted to the scheduler's
ServiceAccount via the `rio-scheduler-lease` namespaced Role AND, since ADR-023
§13b, to the controller's via the `rio-controller-lease` namespaced Role ---
the `nodeclaim_pool` reconciler is leader-elected (see @sec-nodeclaim-pool).
Both Roles split `create` (unrestricted --- RBAC cannot scope `create` by
`resourceNames`) from `get`/`update` (scoped to the one Lease each component
owns). All other controller reconcilers remain non-leader-gated.

#info(title: [Note])[
  The controller does NOT hold permissions for `NetworkPolicies` or
  `ConfigMaps`. NetworkPolicies are deployed as static manifests via the Helm
  chart (see below).
]

= NetworkPolicy

@networkpolicy resources are deployed via the Helm chart
(`infra/helm/rio-build/templates/networkpolicy.yaml`, gated on
`networkPolicy.enabled`), not controller-managed. The controller has no
`networking.k8s.io` RBAC permissions. Intended policies:

- *Executors*: egress to rio-scheduler and rio-store only (gRPC ports). No
  access to the Kubernetes API server or cloud metadata service
  (`fd00:ec2::254` / `169.254.169.254`). DNS egress to kube-system (CoreDNS)
  required for service resolution.
- *Gateway*: ingress from external (Service type LoadBalancer/NodePort for
  SSH). Egress to rio-scheduler and rio-store. DNS egress to kube-system.
- *Scheduler*: egress to PostgreSQL. DNS egress to kube-system.
- *Store*: egress to PostgreSQL and S3. DNS egress to kube-system.
- *Controller*: egress to rio-scheduler (gRPC, for
  `AdminService.ClusterStatus`/`GetSpawnIntents` queue-depth queries) and to
  the Kubernetes API server (for @crd watches and Job management). DNS egress to
  kube-system.

= PodDisruptionBudget

#figure(
  table(
    columns: 4,
    align: (left, left, left, left),
    table.header([Component], [Managed by], [Policy], [Rationale]),
    [Scheduler],
    [Static manifest],
    [`maxUnavailable: 1`],
    [Leader election handles failover; at most one pod unavailable],

    [Gateway],
    [Static manifest],
    [`minAvailable: 1`],
    [At least one pod must remain for SSH connectivity],
  ),
)

Scheduler and gateway PDBs are static manifests in the Helm chart
(`infra/helm/rio-build/templates/pdb.yaml`, gated on
`podDisruptionBudget.enabled`). Executor pods are one-shot Jobs --- a @pdb on
Jobs is meaningless (eviction of a Job pod just reschedules the build via
#rref("ctrl.drain.disruption-target")).

= Service Definitions

#figure(
  table(
    columns: 3,
    align: (left, left, left),
    table.header([Service], [Type], [Purpose]),
    [`rio-gateway`],
    [LoadBalancer or NodePort],
    [SSH ingress for `nix copy` / `nix build --store ssh://`],

    [`rio-scheduler`],
    [ClusterIP],
    [Internal gRPC for executors, gateway, and controller],

    [`rio-store`], [ClusterIP], [gRPC for internal components],
  ),
)

= Health Probes

#r("ctrl.probe.named-service")[
  Health probes against `grpc.health.v1.Health/Check` MUST target a named
  service (e.g., `rio.scheduler.SchedulerService`), not the empty-string
  default. `set_not_serving` only affects named services, not `""` --- a probe
  on `""` stays green through drain and through standby.
]

#info(title: [Note])[
  This requirement applies to the *client-side balancer*
  (`rio-proto/src/client/balance.rs`), which probes the named service to find
  the leader. It does NOT apply to K8s probes:
  `infra/helm/rio-build/templates/scheduler.yaml` intentionally uses
  `tcpSocket` for both readiness and liveness --- standby replicas report
  `NOT_SERVING` on the gRPC health endpoint (they haven't won the lease), so
  gRPC-based K8s probes would crash-loop them. TCP-accept succeeding proves the
  process is live; leader-election is client-side routing's concern, not K8s'.
  `store.yaml` does use `grpc.service: rio.store.StoreService` for readiness
  (store is not leader-elected, so `NOT_SERVING` only means drain or PG/S3
  unhealthy --- correct to take out of rotation). Controller and executor
  probes are HTTP (`/healthz`/`/readyz`) and are unrelated to this rule.
]

#r("ctrl.health.ready-gates-connect")[
  The controller binds its HTTP health server *before* awaiting
  `connect_forever` for the scheduler. `/healthz` (liveness) returns 200
  unconditionally once the kube client is constructed; `/readyz` (readiness)
  returns 503 until the scheduler admin channel connects, then 200. Spawning
  the health server _after_ `connect_forever` would leave nothing listening
  during scheduler cold-start and the chart's livenessProbe (`periodSeconds:10`,
  `failureThreshold:3`, no `startupProbe`) would SIGTERM the pod at \~20--30s
  --- re-introducing the CrashLoopBackOff that `connect_forever` was added to
  fix.
]

#figure(
  table(
    columns: 4,
    align: (left, left, left, left),
    table.header([Component], [Liveness], [Readiness], [Startup]),
    [Gateway],
    [TCP check on SSH port],
    [After scheduler gRPC connection established],
    [---],

    [Scheduler],
    [TCP socket (gRPC health is leader-election-aware, unsuitable for K8s
      probes on HA standby)],
    [TCP socket --- process is live, port is bound. Leader election happens;
      client-side balancer routes to leader via named-service health.],
    [TCP socket. Startup budget sized for state recovery (reload non-terminal
      builds from PG).],

    [Store],
    [TCP socket],
    [gRPC health check on named service --- after PostgreSQL + S3 reachable],
    [---],

    [Controller],
    [HTTP `/healthz` (unconditional 200; bound before `connect_forever`)],
    [HTTP `/readyz` --- 503 until scheduler gRPC connected, then 200
      (single-replica; only the `nodeclaim_pool` reconciler is leader-gated
      (ADR-023 §13b) --- all others run unconditionally)],
    [---],

    [Executor],
    [HTTP `/healthz` + `/readyz` (no gRPC server)],
    [`/readyz` 200 after first accepted heartbeat],
    [HTTP check, `failureThreshold × periodSeconds ≥ 120s` (FUSE mount + cache
      warm)],
  ),
)

= Executor Lifecycle

#r("ctrl.drain.sigterm")[
  *Scale-down:* `terminationGracePeriodSeconds` is set to `7200` (2 hours) to
  allow in-flight builds to complete.
]

*SIGTERM drain (no preStop hook needed):* the executor's main loop has a
`select!` arm on SIGTERM. On signal:

+ Set heartbeat `draining=true` (the executor-authoritative drain source ---
  I-063); the scheduler's `has_capacity()` reads it via `is_draining()`.
+ Keep the `BuildExecution` stream connected (and reconnect across scheduler
  restart) until `BuildSlot::wait_idle()` returns --- the in-flight build's
  completion is reported.
+ Send `AdminService.DrainExecutor` (best-effort exit deregister) and exit 0.

A preStop hook doing the same is redundant: K8s sends SIGTERM on pod
termination regardless of preStop, and the executor's signal handler implements
the drain. The Job pod template does NOT define a preStop.

#r("ctrl.drain.disruption-target")[
  *Eviction-triggered preemption:* the controller runs a Pod watcher filtered
  to `rio.build/pool`-labeled pods with
  `status.conditions[type=DisruptionTarget,status=True]`. When K8s marks a pod
  for eviction (node drain, spot interrupt), the watcher calls
  `AdminService.DrainExecutor{force:true}` --- the scheduler sends
  `CancelSignal` for the in-flight build → executor `cgroup.kill()`s → the
  build reassigns to a healthy executor within seconds. Without this, the
  evicting pod would self-drain with `force=false` and wait up to
  `terminationGracePeriodSeconds` (2h) for the in-flight build to complete
  naturally before SIGKILL loses it anyway. The SIGTERM self-drain path (above)
  is the fallback if the watcher misses the window.
]

= ComponentScaler

#r("ctrl.scaler.component+2")[
  The controller reconciles `ComponentScaler` CRs into `apps/v1 Deployment
  {targetRef} /scale` patches. `desired_replicas =
  clamp(ceil(Σ(queued+running+substituting) / status.learnedRatio),
  spec.replicas.min, spec.replicas.max)` where `Σ(queued+running+substituting)`
  comes from `AdminService.ClusterStatus` (the *predictive* signal ---
  scheduler knows N builders are about to exist before they exist; store scales
  ahead of the burst). Scale-down is held for 5 minutes after the last scale-up
  and limited to −1/tick. Reconcile interval: 10s.
]

#r("ctrl.scaler.signal-substituting")[
  The predictive `builders` signal MUST include `substituting_derivations` at
  1:1 weight with `queued`/`running`. A substitution cascade with zero
  queued/running MUST NOT produce `builders=0` --- that scales the store toward
  `min` exactly when it is the bottleneck.
]

#r("ctrl.scaler.ratio-learn+2")[
  `status.learnedRatio` self-calibrates against
  `max(StoreAdminService.GetLoad().pg_pool_utilization)` over the
  `spec.loadEndpoint` headless-service endpoints (the *observed* signal).
  Asymmetric correction: `load > spec.loadThresholds.high` (default 0.8) →
  immediate `current+1` AND `learnedRatio *= 0.95` (under-provisioning is
  dangerous --- I-105 cascade); `load < spec.loadThresholds.low` (default 0.3)
  for 30 consecutive ticks → `learnedRatio *= 1.02` (over-provisioning is
  cheap). Growth is gated on `builders > 0 && current > min` (idle ≠
  over-provisioned) and the ratio is clamped to `[RATIO_FLOOR, RATIO_CEILING]`.
  The ratio persists in `.status` so a controller restart resumes from the
  learned value, not `spec.seedRatio`.
]

#r("store.admin.get-load+2")[
  `StoreAdminService.GetLoad` returns `pg_pool_utilization = (pool.size −
  pool.num_idle) / max_connections` and `substitute_admission_utilization =
  (capacity − available_permits) / capacity` for the replica it's called on.
  The ComponentScaler reconciler polls every store pod (DNS-resolving the
  headless service); per-pod load is `max(pg_pool_utilization,
  substitute_admission_utilization)` (substitution can saturate independently
  --- upstream HTTP bottleneck while PG sits idle), and `observedLoadFactor` is
  the max across pods. The handler also publishes
  #(refs.metric)("rio_store_pg_pool_utilization") and
  #(refs.metric)("rio_store_substitute_admission_utilization") so Prometheus
  sees the same values the controller acted on.
]

When `componentScaler.store.enabled=true`, the helm chart MUST omit
`Deployment.spec.replicas` from the rendered store template --- otherwise `helm
upgrade` resets the replica count and fights the controller. The controller's
`/scale` patches use field-manager `rio-controller-componentscaler` (distinct
from helm's apply manager).

= GC Cron

#r("ctrl.gc.startup-delay")[
  The GC cron's FIRST tick MUST be delayed by `STARTUP_DELAY` (300 s) plus
  0--60 s jitter from controller start. Firing at t≈0 collides GC mark with
  post-deploy validation traffic --- every helm rollout caused `nix copy` to
  exhaust the gateway's `Aborted` retry budget (I-168). 5 minutes clears the
  deploy-then-stress window while keeping the "controller restart doesn't delay
  GC by 24 h" property.
]

#r("ctrl.gc.cron-schedule")[
  Controller runs a GC cron reconciler: `tokio::select!` on
  `shutdown.cancelled()` vs `interval.tick()` (default 24h, configurable via
  `controller.toml gc_interval_hours`; 0 = disabled). Each tick: connect to
  store-admin with `tokio::time::timeout(30s, ...)` --- on connect failure,
  `warn!` + increment
  #(refs.metric)("rio_controller_gc_runs_total")`{result="connect_failure"}` +
  `continue` (NEVER `?`-propagate out of the loop --- tonic has no default
  connect timeout and a stale IP hangs on SYN). On success: `TriggerGC`, drain
  the `GcProgress` stream, increment `{result="success"}`. Implementation at
  `rio-controller/src/reconcilers/gc_schedule.rs`; wired via
  `spawn_monitored("gc-cron", ...)` gated on `gc_interval_hours > 0`. No
  leader-gate: controller is single-replica by design (only the
  `nodeclaim_pool` reconciler is leader-gated, ADR-023 §13b --- all others,
  including this one, run unconditionally); `replicas>1` misconfig is
  serialized by the store's `GC_LOCK_ID` advisory lock.
]

= NodeClaim pool (ADR-023 §13b)
<sec-nodeclaim-pool>

#r("ctrl.nodeclaim.ffd-sim")[
  The per-tick NodeClaim-pool reconcile simulates placement via
  first-fit-decreasing --- intents sorted `(eta=0, c*)` descending (Ready
  before forecast, large before small), bin-select `MostAllocated` on the
  `allocatable` divisor --- so the deficit is the unplaced residual and matches
  the `schedulerName: kube-build-scheduler` instance.
]

#r("ctrl.nodeclaim.ffd-exclude-terminating")[
  NodeClaims with `metadata.deletionTimestamp` set (Karpenter's termination
  finalizer draining the node, \~60--90s) are NOT FFD placement candidates:
  kube-scheduler refuses to bind onto a cordoned/terminating node, so a
  simulated placement there overcounts capacity and `cover_deficit` under-mints
  the replacement until the finalizer clears (§13d "placement ⊇ provisioning").
  Terminating claims DO still count toward `max_fleet_cores` / per-class
  fleet-core budgets --- the EC2 instance bills until the finalizer removes it,
  and the replacement must consume headroom from the same budget the dying node
  still occupies. Surfaced as
  #(refs.metric)("rio_controller_nodeclaim_live")`{state="terminating"}`.
]

#r("ctrl.nodeclaim.anchor-bulk+5")[
  Unplaced intents per `(h,cap)` cell whose pod footprint fits the cell's
  per-class `(max_cores, max_mem)` and global `max_disk` cap are covered by `n`
  uniform claims at `(max(⌈Σc*/n⌉, max_i c*), max(Σm/n, max_i m), max(Σd_eph/n,
  max_i d_eph))`, where `n` iterates upward from the 3-axis lower bound
  `max(⌈Σc*/cell_cores⌉, ⌈Σm/cell_mem⌉, ⌈Σd_eph/maxDisk⌉)` until the production
  FFD's MostAllocated-cpu placement order packs every fitting intent; over-cap
  intents are dropped with `intent_dropped_total{reason=exceeds_cell_cap}` (`Σ/n`
  is a bin-packing lower bound, not a guarantee). §13c-3:
  `cell_cores`/`cell_mem` are the per-class effective ceiling
  `min(HwClassConfig::ceilings_for(h), HwClassConfig::global_ceilings())` (both
  shipped over `GetHwClassConfig`, not `controller.toml`), so each claim's `(c,
  m)` chunk is hostable by some instance in `h`'s `requirements` set;
  `cover_deficit` skips the tick when the global ceiling is not yet loaded
  (fail-closed, ≤300s self-heal). NodeClaim creation is capped at
  `sla.maxNodeClaimsPerCellPerTick` and the `sla.maxFleetCores` budget; cells
  are iterated round-robin from a rotating start so no cell starves under
  sustained pressure.
]

#r("ctrl.nodeclaim.lead-time-ddsketch")[
  `lead_time[h,cap] = q_0.9(boot − eta_error)` is read from a sliding
  active/shadow quantile-sketch pair (HdrHistogram, 1 ms–24 h at 2
  significant figures) per cell, persisted to PG as `u32`-version-tagged
  BYTEA in the HdrHistogram V2 format. Sketches are seeded from
  `sla.leadTimeSeed[h,cap]` at synthetic count `n_seed = 1/(1-q) = 10`; the
  closed-loop `forecast_warm_hit_ratio` Schmitt widens/narrows the quantile
  by `Δq=0.02` per firing, capped at `q ≤ 0.99` and
  `lead_time ≤ sla.maxLeadTime`.
]

#r("ctrl.nodeclaim.consolidate-na+6")[
  An empty NodeClaim is kept while `λ(t)·𝔼[c_arrival | c_arrival ≤ cores] >
  cores/q_0.5(boot[h,cap])`. `λ(t)` is the windowed empirical arrival rate over
  `[t, t+W)` (window `W = q_0.5(boot)/2 ≥ 5s`) on right-censored
  `idle_gap[h,cap]`; the fitting-core term is the current tick's per-cell mean
  over `placeable` (FFD's placed output --- design choice from r43 bug_028:
  FFD-unplaced intents are lead-time-gated forecasts and capacity-overflow
  demand; neither lands on the idle node THIS tick, though forecasts may land
  later, so the bias direction is ambiguous and was left matching the code over
  the original `intents` wording per §SCC(2)) restricted to those whose
  admissible `(hw-class, capacity-type)` cell set (`cells_of(i)`) contains the
  cell, or whose hw-class set is empty AND `hw_admits(cell, system, features)`
  holds --- the SAME predicate FFD's `simulate` uses for the agnostic-fallback
  gate (defined as 0 when `placeable` is ⊥ or empty). A floor `consolidate_after
  ≥ max(q_0.5(boot)/2, min_consolidation_time[h])` prevents a transient lull
  from collapsing to always-delete and lets the operator preserve the pre-§13e
  Karpenter `consolidateAfter` policy floor for cells the NA model would
  otherwise reap aggressively (default `{"fetcher-*": 600s, "*": 300s}` --- the
  `q_0.5(boot)/2` model floor is below the boot cost it is supposed to amortize
  for short-boot builder cells; the universal 300s floor covers the sequential
  inter-build dispatch gap and `>-<` DAG bottlenecks at ≈ \$0.0083/node-reap-avoided).
]

#r("ctrl.nodeclaim.shim-nodepool")[
  A single shim NodePool (`limits:{cpu:0}`, `disruption.budgets:[{nodes:"0"}]`)
  satisfies Karpenter's state-tracking lookup; the controller stamps
  `karpenter.sh/nodepool: rio-nodeclaim-shim` plus `rio.build/*` on
  `NodeClaim.metadata.labels`. NodeClaims reference EC2NodeClass directly
  (`rio-nvme` / `rio-default` by storage); rio owns deletion.
]

#r("ctrl.nodeclaim.priority-bucket")[
  Builder pods MUST set `priorityClassName=rio-builder-prio-{⌊log₂c*⌋}` (10
  fixed PriorityClasses, buckets 0--9, `globalDefault:false`,
  `preemptionPolicy:Never`) and `schedulerName=kube-build-scheduler`. The
  scheduler's `validate_shape()` asserts `maxCores < MAX_CORES_HARD = 1024`;
  the catalog-derived global is clamped at `MAX_CORES_GLOBAL = 1023`.
]

#r("ctrl.nodeclaim.taints.hwclass")[
  `build_nodeclaim` sets `spec.taints` to the universal builder taint followed
  by `hwClasses[h].taints` (chain order: builder first). §13c: metal hwClasses
  carry `rio.build/kvm=true:NoSchedule` so only kvm-tolerating pods land on
  metal nodes --- replacing the pre-§13c static metal NodePool's hardcoded
  taint.
]

#r("ctrl.nodeclaim.budget.per-class")[
  `cover_deficit` clamps each cell's per-tick mint at `min(global_remaining,
  hwClasses[cell.0].max_fleet_cores − class_live − class_created_this_tick)`
  where `class_live` and `class_created_this_tick` are summed across
  capacity-types (per-hwClass, NOT per-Cell --- a per-Cell cap would let
  spot+od each hit it independently → 2× \$/hr exposure). `max_fleet_cores=None`
  ⇒ global budget only.
]

#r("ctrl.nodeclaim.placeable-gate+4")[
  For Builder pools, the Pool reconciler creates Jobs only for intents the
  nodeclaim_pool reconciler's last FFD simulation placed on a `Registered=True`
  NodeClaim. The §13a `ready` retain is replaced; Job count is bounded by
  Registered-node capacity, not Ready-set size. An unarmed gate (no FFD tick
  yet) is fail-closed for both spawn and reap. The gate does NOT apply to
  Fetcher pools --- fetcher NodeClaims are minted by the same `cover_deficit`
  (§13e: the FFD simulation covers Builder and Fetcher Pools), so the placeable
  set DOES contain fetcher intent IDs, but the Builder-only retain is kept: the
  fetcher fan-out hazard is already bounded provisioning-side (`cover_deficit`'s
  per-tick + per-class `maxFleetCores` caps), and a fetcher pod is cheap (\~1
  core), so there is no 1226-Pending-Jobs problem for the gate to solve.
  (Production fetcher Pools do not set `spec.maxConcurrent` --- the bound is
  the NodeClaim budget, not the Job count.) Extending the retain to Fetcher
  pools is a follow-up. The gate also does NOT apply when the NodeClaim CRD is
  absent (the controller probes at startup; absent ⇒ static-node cluster, gate
  is pass-through).
]

= Build CRD (removed)

The `Build` CRD (`rio.build/v1alpha1 Build`) was removed in P0294. It was an
alternative K8s-native build submission path that duplicated the SSH
(`ssh-ng://`) flow. No known production users.

*Cluster upgrade:* existing `Build` CRs on running clusters are orphans after
controller upgrade (the reconciler no longer watches them). They can be safely
deleted: `kubectl delete builds.rio.build --all -A`. The CRD itself remains
installed (helm `crds/` directory is install-only, not upgrade-managed); delete
it manually: `kubectl delete crd builds.rio.build`.

= Component Deployment Model

The controller manages:
- *Pool* CRD → spawns/reaps one-shot builder/fetcher Jobs (per-intent
  resources, security context)

The controller does *NOT* manage:
- Scheduler or store Deployments --- these are deployed via Helm/kustomize as
  standard Deployments
- Rationale: scheduler and store have simple lifecycle (single replica or
  leader-elected); CRD management adds complexity without benefit

= Key Files

- `rio-crds/src/` --- CRD type definitions (separate crate; Pool,
  ComponentScaler)
- `rio-controller/src/reconcilers/pool/` --- Pool reconcile loop + Job/pod-spec
  builders
- `rio-controller/src/reconcilers/componentscaler/` --- ComponentScaler
  (rio-store/rio-gateway Deployment scaling)

= CRD Versioning

CRDs follow a `v1alpha1` → `v1beta1` → `v1` progression. The initial
implementation uses `v1alpha1` with no stability guarantees. Plan a conversion
webhook before promoting to `v1beta1` to support zero-downtime upgrades from
`v1alpha1`.

= CRD Validation

CRDs use CEL validation rules (`x-kubernetes-validations`) for structural
constraints:

#figure(
  table(
    columns: 3,
    align: (left, left, left),
    table.header([CRD], [Rule], [Reject reason]),
    [Pool],
    [`size(self.systems) > 0`],
    [A pool with no target systems accepts no work.],

    [Pool],
    [`hostNetwork ⇒ privileged`],
    [See #rref("ctrl.crd.host-users-network-exclusive").],

    [Pool],
    [`kind=Fetcher ⇒ features empty`],
    [See #rref("ctrl.crd.fetcher-no-features+2").],

    [SeccompProfileKind],
    [type ∈ allowed; `Localhost ⇔ has(localhostProfile)`],
    [See #rref("ctrl.crd.seccomp-cel").],

    [ComponentScaler.Replicas],
    [`self.min >= 0 && self.min <= self.max`],
    [Clamp range must be non-empty and non-negative (`/scale` subresource
      rejects negative replicas).],

    [ComponentScaler.TargetRef],
    [`self.kind == 'Deployment'`],
    [Reconciler patches `apps/v1 deployments/scale` only.],

    [ComponentScaler.LoadThresholds],
    [`0.0 < low < high <= 1.0`],
    [Threshold ordering for ratio correction.],
  ),
)

#r("ctrl.crd.fetcher-no-features+2")[
  The controller's spawn-decision query and the spawned worker's `RIO_FEATURES`
  MUST advertise `[fetcher]` (and ONLY `[fetcher]`) for Fetcher-kind Pools,
  derived from `pool.spec.kind` at a single chokepoint
  (`effective_features(spec)`). The Pool's declared `spec.features` MUST stay
  empty for Fetchers --- the CEL admission rule rejects a non-empty value, and
  `effective_features(spec)` ignores the field as a belt-and-suspenders for
  pre-CEL specs (a Fetcher Pool with declared `[kvm]` would otherwise hit the
  I-181 ∅-guard at scheduler `snapshot.rs` and filter out every `[fetcher]` FOD
  --- the fetcher pool never spawns and all fetches stall silently). The
  override is a fail-safe, not a permission. The single chokepoint ensures the
  spawn-decision query (`GetSpawnIntents.features`) and the spawned worker's
  `RIO_FEATURES` cannot diverge.
]

#r("ctrl.crd.seccomp-cel")[
  `SeccompProfileKind` is a struct (`{type, localhostProfile?}`), not a Rust
  enum: kube-core's structural-schema rewriter rejects oneOf-variant subschemas
  with non-identical shared properties, so the type/localhostProfile coupling
  is enforced by CEL instead of the Rust type system. Two rules: `self.type in
  ['RuntimeDefault', 'Localhost', 'Unconfined']`, and `self.type == 'Localhost'
  ? has(self.localhostProfile) : !has(self.localhostProfile)`. The struct
  mirrors `pod.spec.securityContext.seccompProfile` exactly so operators can
  copy-paste; nested `KubeSchema` carries the rules through into the `Pool`
  schema.
]

#r("ctrl.crd.host-users-network-exclusive")[
  The controller MUST reject `Pool` specs with `hostNetwork: true` and
  `privileged` unset or false. Kubernetes admission rejects pod specs combining
  `hostUsers: false` with `hostNetwork: true` (user-namespace UID remapping is
  incompatible with the host network namespace). Since the non-privileged path
  sets `hostUsers: false` unconditionally (ADR-012,
  #rref("sec.pod.host-users-false")), `hostNetwork: true` implies the
  `privileged: true` escape hatch. CRD CEL validation enforces this at `kubectl
  apply` time; the builder additionally suppresses `hostUsers` when the
  combination is encountered in pre-existing specs (emitting a Warning event).
]

#r("ctrl.event.spec-degrade")[
  The Pool reconciler MUST emit a `Warning`-type Kubernetes Event for every
  spec field the builder silently degrades. CEL validation rejects NEW specs
  with invalid combinations; existing specs applied before the CEL rule landed
  are defensively corrected at pod-template time (e.g., `hostUsers` suppressed
  for `hostNetwork: true`). Without a Warning event, the operator has no signal
  that their spec is stale --- `kubectl get pool -o yaml` shows the original
  value; the pod template shows the corrected value. The Warning names the
  field, the spec value, and the remediation.
]

There is no per-pod FUSE cache to size since the castore cutover (ADR-022
P0560). The digest/chunk caches are node-level hostPaths
(`/var/rio/{cache,chunks}`) owned and LRU'd by the rio-mountd DaemonSet; every
executor pod (both kinds) mounts them read-only alongside `/var/rio/castore`
(per-build FUSE mountpoints, `HostToContainer` propagation so mounts created
after pod start become visible) and `/var/rio/staging` (per-build write area).
The pod also mounts the mountd UDS directory and gets
`RIO_MOUNTD_SOCKET=/run/rio-mountd/rio-mountd.sock` so the builder can request
its per-build mount over the socket. None of this counts against the pod's
`ephemeral-storage` request --- the caches sit outside the kubelet's per-pod
accounting --- and the only FUSE knob left in the CRD is `fuseThreads`.

= Pool Finalizer

Pool CRDs carry a `pool.rio.build/drain` finalizer. The finalizer's `cleanup()`
removes the finalizer immediately; in-flight Jobs finish their one build
naturally and ownerRef GC removes them after the pool is gone. The reconciler's
`apply()` path short-circuits when `deletionTimestamp` is set (finalizer wraps
it) so no new Jobs are spawned during deletion.

= Failure modes

#figure(
  table(
    columns: (auto, 1fr),
    align: (left, left),
    [*Immediate effect*], [No autoscaling decisions; CRD reconciliation pauses],
    [*Cascading effect*], [Pool sizes remain static; no GC scheduling],
    [*Recovery*],
    [Restart controller. State is in CRDs and K8s API; no persistent state
      lost.],
  ),
  caption: [Controller-down failure mode (from the component failure matrix).],
)

= Rationale

== NixOS EKS worker node AMI // supersedes ADR-021
<sec-rationale-nixos-ami>

Builder/fetcher Karpenter NodePools previously ran `amiAlias:
bottlerocket@latest`. Bottlerocket gave a minimal, dm-verity-signed, read-only
root with a TOML settings API but no kernel control (the per-page FUSE /
`EROFS_FS_ONDEMAND` / `CACHEFILES_ONDEMAND` work needs custom Kconfig and a
future out-of-tree `riofs` kmod), no exposure of `cgroup_writable = true` on
the runc runtime (so `hostUsers: false` was undeployable on EKS), and no
reproducibility (`bottlerocket@latest` is a moving alias --- the only deploy
artifact that wasn't a #gls("ca", display: "content-addressed") flake output).

The worker-node AMI is built from nixpkgs
`maintainers/scripts/ec2/amazon-image.nix`, with `awslabs/amazon-eks-ami/
nodeadm` packaged for cluster bootstrap, declaring `amiFamily: AL2023` on the
EC2NodeClass so Karpenter emits the same NodeConfig MIME userData it would for
a real AL2023 node. The AMI is tag-selected by `rio.build/ami=<git-sha>`;
`cargo xtask k8s -p eks ami push` builds, `coldsnap`-uploads, registers, and
tags. Key choices:

+ *`amiFamily: AL2023`, not `Custom`.* Under `Custom`, Karpenter passes no
  cluster info at all --- re-implementing IMDSv2, max-pods-per-ENI, IPv6
  cluster handling, and the `aws:///<az>/<instance-id>` providerID format is
  \~3k LoC of edge cases nodeadm already owns. Karpenter validates only that
  `amiSelectorTerms` resolve to ≥1 AMI; the family controls userData
  generation, nothing else.
+ *Thin `services.rio.eksNode` module, not nixpkgs
  `services.kubernetes.kubelet`.* The nixpkgs module assumes a self-managed
  cluster (PKI generation, kubeconfig rendering). Here all kubelet config is
  nodeadm's output; the NixOS unit is \~40 lines pointing at the files nodeadm
  wrote.
+ *`nix.enable = false` on the node.* Builds run inside the builder _pod_,
  which carries its own `nix`. Dropping the daemon saves \~80 MB closure and
  removes a root-socket attack surface. Debugging is `kubectl debug node/…` +
  SSM Session Manager --- neither needs an on-image Nix.
+ *Pinned kernel minor in `nix/pins.toml` (`[node] kernel_minor`)*, not
  `linuxPackages_latest`. A nixpkgs flake-input bump can't surprise-rebuild
  the \~40 min kernel derivation; bump deliberately when the per-page-FUSE
  work needs a particular patch level.
+ *Both arches from P1.* The NodePool requirements already span x86_64 +
  aarch64; a single-arch AMI would leave arm64 pods on Bottlerocket during
  migration with two userData formats live at once.
+ *Seccomp profiles single-sourced at `nix/nixos-node/seccomp/`.* The AMI bakes
  them via `systemd.tmpfiles` (`hardening.nix`) --- no bootstrap container, no
  SPO DaemonSet, no chart-side copies. k3s VM tests use the same
  `systemd.tmpfiles` delivery.
+ *Three AMI variants --- UEFI/UKI for virtualized + arm64 `.metal`;
  legacy-bios/grub for x86_64 `.metal`.* AWS x86_64 bare-metal SKUs reject UEFI
  AMIs (`InvalidParameterValue`) --- every entry through gen 8 reports
  `["legacy-bios"]` only. A UEFI-only x86 AMI made x86 metal nodes
  unschedulable (I-205). `.#packages.x86_64-linux.ami-bios` builds the same module tree
  with `ec2.efi=false`; `xtask ami push` registers it alongside the UEFI
  variants and tags all three with `rio.build/boot={uefi,legacy-bios}`. A
  second EC2NodeClass `rio-metal` selects `(amd64, legacy-bios)` and `(arm64,
  uefi)`; only §13b metal NodeClaims reference it. The bios variant drops the
  perlless `forbiddenDependenciesRegexes` check (grub's installer is Perl); the
  perlless behaviors are restated in `minimal.nix`.
+ *`/dev/fuse` + `/dev/kvm` via containerd `base_runtime_spec`, not a device
  plugin.* runc `mknod`s the device nodes inside the container's `/dev`
  (container-namespace uid/gid --- no `hostUsers:false` idmap-mount rejection).
  Every pod gets `/dev/fuse`; `/dev/kvm` is host-conditional --- containerd's
  `ExecStartPre` symlinks `/run/base-runtime-spec.json` to the `withKvm` variant
  iff `test -c /dev/kvm` succeeds on the host, so non-`.metal` pods don't see a
  dead `mknod` that fools `test -c /dev/kvm` probes then ENXIOs on open. kvm
  pods route to §13b metal NodeClaims via per-intent `nodeAffinity`
  (#rref("ctrl.pool.node-affinity-from-intent")) plus the pool-static
  toleration the controller derives from `features:[kvm]`
  (#rref("ctrl.pool.kvm-device+2")) --- never a pool-static `nodeSelector`. No
  extended resource is requested --- Karpenter NodeOverlay capacity is
  simulation-only (nothing writes `Node.status.capacity`), so kube-scheduler
  can't bind pods that request it. Eliminates the I-184 goroutine-orphan
  failure mode and the watchdog it required.

*Security posture vs Bottlerocket:* dm-verity root is lost but irrelevant
under ephemeral single-build-per-node (\~5 min lifetime, no persistent state
--- an attacker with root-on-node already owns the build output, and dm-verity
protects against persistence across reboots, which doesn't apply). Read-only
system is a wash (`/nix/store` ro via stage-2 bind-mount). SELinux is lost (out
of scope). The big swing is that `hostUsers: false` becomes deployable
(`virtualisation.containerd.settings…cgroup_writable = true`). The `system`
managed nodegroup (long-lived, runs Karpenter/coredns) stays AL2023 managed ---
explicitly out of scope.

*Prebaked executor layer cache.* The AMI bakes a single multi-manifest OCI
archive containing the `rio-builder` and `rio-fetcher` images (deduplicated
layers --- they share every layer, only `config.Env` differs) and imports it
via a `containerd-seed-warm` oneshot concurrent with kubelet start. PodSpec
refs stay `<ECR>/rio-{builder,fetcher}:<git-sha>`; the seed is a content-store
warm only --- on first pod schedule, containerd checks each ECR-manifest layer
by digest against the local store and fetches only the absent ones. Net:
per-fresh-node ECR pull drops from \~400 MB to the delta since the AMI was cut
(typically one \~10 MB layer). A `localhost/` digest-pin alternative was
rejected: every `rio-builder` code change would become a 10--15 min AMI rebake
instead of `up --push --deploy`; the layer-cache design degrades gracefully
(delta-only pull) instead of failing hard.

*Consequences.* Kernel Kconfig + out-of-tree kmods become a `nix build` away
(unblocks the `riofs` / EROFS-ondemand track); `hostUsers: false` works on EKS;
the node image is content-addressed and pinned via `karpenter.amiTag=<sha>` the
same way `global.image.tag` pins the pod images. On the negative side: kernel
rebuilds (\~40 min cold) on every `extraStructuredConfig` change (cached after
first build); AMI-per-SHA snapshot storage (\~\$0.40/mo per 8 GB snapshot,
bounded by `xtask ami gc --keep 5`); and another moving part in `xtask k8s eks
up` (`ami push` between `push` and `deploy`).

== The hard part: cold start at scale

When many executors start simultaneously (scale-up event), all FUSE caches are
cold. Every executor needs to fetch the same common dependencies (glibc,
coreutils, etc.) from rio-store, creating a thundering herd on the store's S3
backend. Mitigations: an in-process LRU chunk cache on rio-store (`ChunkCache`,
moka-based, default 2 GiB) reduces S3 round-trips for hot chunks; and
per-derivation #glspl("prefetch-hint") --- the scheduler sends input-path prefetch to
the assigned executor before dispatch, so the FUSE cache warms during the
scheduling window rather than on first `read()`. Staggered scheduling with the
cold-start prefetch warm-gate (#rref("sched.assign.warm-gate")) is implemented
and is the relevant mechanism for ephemeral builders, where every build
cold-starts (#rref("ctrl.pool.ephemeral")).
