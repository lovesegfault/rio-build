#import "/lib/rio.typ": *
#show: rio.with(domains: ("ctrl", "store"))


Manages rio-build lifecycle on Kubernetes via CRDs.

= CRDs

== Pool

#r("ctrl.crd.pool")[
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
    fuseThreads: 4                               # u32?, optional — Builder-only, CEL-forbidden for Fetcher
    fusePassthrough: true                        # bool?, optional — Builder-only, CEL-forbidden for Fetcher
    privileged: false                            # bool?, optional — CEL-forbidden for Fetcher
    hostNetwork: false                           # bool?, optional — true requires privileged:true; CEL-forbidden for Fetcher
    seccompProfile:                              # SeccompProfileKind?, optional — CEL-forbidden for Fetcher
      type: Localhost                            #   RuntimeDefault | Localhost | Unconfined
      localhostProfile: operator/rio-builder.json#   required iff type=Localhost (r[ctrl.crd.seccomp-cel])
    # fuseCacheBytes: <bytes>                    # u64?, REJECTED by CEL for BOTH kinds since r35 (merged_bug_024) — pools read the per-kind `[nodeclaim_pool].{fuse_cache_bytes,fetcher_fuse_cache_bytes}` (helm `poolDefaults.{fuseCacheBytes,fetcherFuseCacheBytes}`, 50Gi/4Gi prod). See r[ctrl.event.spec-degrade].
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
]

== Job lifecycle

#r("ctrl.pool.ephemeral+2")[
  The reconciler polls `AdminService.GetSpawnIntents` each requeue tick (10s)
  and spawns one K8s Job per returned intent (#rref("ctrl.pool.reconcile")),
  subject to active Jobs < `spec.maxConcurrent` (no Job-count ceiling when
  `maxConcurrent` is unset). Each Job runs one rio-builder pod whose main loop
  exits after one pulled assignment's outcome report (`PullAssignment` → build
  → `ReportOutcome`) → pod terminates → Job goes Complete →
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

*Isolation guarantee:* zero cross-build state. Fresh pod means fresh emptyDir
for @fuse cache and @overlayfs upper, fresh filesystem. Untrusted tenants cannot
leave poisoned cache entries for subsequent builds --- there is no "subsequent
build" on that pod. Strongest isolation when combined with `hostUsers: false` +
non-privileged (see #rref("sec.pod.host-users-false")).

*Cost:* per-build cold start (pod scheduling + container pull + FUSE mount +
scheduler registration --- typically 10--30s) plus one reconcile tick (\~10s)
before the Job is spawned. Nodes outlive pods (@karpenter consolidation policy),
so the node-level FSx cache survives pod churn --- the cold-start cost is pod
overhead, not refetching the @closure.

*Dispatch path:* the Job pod's builder pulls one assignment
(`PullAssignment`), builds it, reports the outcome (`ReportOutcome`), then
exits. The controller's active mechanism is `GetSpawnIntents` polling; a
push-mode Scheduler→Controller RPC was considered and rejected (rationale
preserved in git history with the retired `ephemeral.rs`).

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

#r("ctrl.ephemeral.reap-orphan-running+6")[
  When a Running Job (`JobStatus.ready > 0`) is older than the orphan grace
  (default 5min) AND no open pull-mode attempt from
  `AdminService.ListOpenAttempts` covers it (match key: the Job's
  `rio.build/intent-id` annotation), the controller MUST delete the Job. This
  is the controller-side backstop for I-165: a builder process stuck in
  uninterruptible sleep (D-state FUSE wait, OOM-loop) cannot self-exit via the
  rendered `RIO_IDLE_SECS` idle bound, never completes a pull, and
  would otherwise sit until `activeDeadlineSeconds` (default 1h). The grace
  MUST exceed the Job's RENDERED idle bound (per
  #rref("ctrl.job.idle-render-coupled") --- eta-priced for forecast spawns)
  so the process-level exit is given first chance; the controller reap fires
  only when the process cannot act on its own. A Job covered by an open attempt is NOT reaped --- the ledger says
  a build is in progress; `activeDeadlineSeconds` is the
  backstop for stuck-mid-build. The reap is *skipped entirely* when the
  `ListOpenAttempts` read fails (scheduler unreachable / standby) ---
  fail-closed, same posture as
  #rref("ctrl.ephemeral.reap-excess-pending"). A successful read is
  authoritative about what it CONTAINS (durable ledger state that survives
  scheduler failover), but absence is only actionable once the serving
  leader is older than the grace (#rref("ctrl.job.orphan-leader-age")).
]

#r("ctrl.job.orphan-leader-age+2")[
  The orphan-Running reap MUST NOT act on the absence of an open attempt
  for any CANDIDATE while the serving leader's own age (`leader_for_secs`
  on the `ListOpenAttempts` response) is below that candidate's OWN
  effective orphan grace (the per-candidate wait envelope + slack ---
  #rref("ctrl.pool.wait-envelope")), with the flat global grace as the
  cheap whole-tick floor. A never-pulled pod has no attempt row by
  construction (the builder retries pull transport errors indefinitely;
  a row exists only after a successful mint), so a freshly-failed-over
  leader's view is durably, truthfully empty of rows for pods that are
  about to pull --- and a forecast pod's pull may lawfully be up to its
  envelope late, so a leader younger than the candidate's envelope has
  not yet observed one full candidate-grace and its empty view is not
  absence evidence for that candidate (merged_bug_136: the flat
  comparison exposed lawfully-waiting forecast pods to foreground
  delete at any new-leader age past the flat grace but inside the
  candidate's envelope+slack bound). Every pod gets
  one full grace OF ITS OWN measured against the NEW leader before
  absence becomes evidence.
]
The gate is per-tick and self-clearing: once `leader_for_secs` crosses the
grace the reap proceeds with no state to reconcile --- the cost of the gate
is at most one extra grace window of delay for a genuine D-state orphan
after a failover (accepted; `activeDeadlineSeconds` still bounds it).

*Cleanup:* the finalizer's `cleanup()` returns immediately. In-flight Jobs
finish their one build naturally; ownerRef GC removes them after the pool is
gone.

#r("ctrl.ephemeral.intent-deadline+2")[
  Jobs MUST set `spec.activeDeadlineSeconds` to the BUILD WINDOW
  (`SpawnIntent.deadline_secs`, floored at 180 as defense against the
  proto default) PLUS the intent's wait envelope
  (#rref("ctrl.pool.wait-envelope")) --- the k8s deadline clock spans
  Pending AND Running, so the deadline MUST price the lawful wait; an
  eta-blind deadline kills a forecast pod mid-lawful-wait
  (merged_bug_136). The scheduler computes `deadline_secs`
  per-derivation (D7): for fitted keys, `wall_p99 * 5` at the solved
  core count; for unfitted (probe/explore), `[sla].probe.deadline_secs`;
  both clamped `[resource_floor.deadline_secs, 86400]`.
  `SlaConfig::validate` enforces `probe.deadline_secs >= 180`, so the
  intent value is always positive --- the controller has no per-kind
  fallback, and the only controller-side additions are the floor and
  the envelope term. The worker's `daemon_timeout` MUST be denominated
  in the BUILD WINDOW alone (window − 90, never the wait-padded Job
  deadline), so the fires-first contract holds for every lawful wait:
  the worker fires at `wait + window − 90 < window + envelope`. A
  `DeadlineExceeded` kill triggers
  #rref("ctrl.terminated.deadline-exceeded") → `bump_floor_or_count`
  doubles `floor.deadline_secs` → the next intent gets a longer
  `activeDeadlineSeconds`. The 5× headroom is scheduler-side. Backstop
  purpose: a wrong-pool spawn (executor heartbeats in, never matches
  dispatch) would hang indefinitely without it; K8s kills at deadline,
  `backoffLimit: 0` marks Failed, `ttlSecondsAfterFinished` reaps.
]

#r("ctrl.terminated.deadline-exceeded+3")[
  The Job-mode reconciler MUST report each Job with `status.conditions`
  containing `type=Failed, reason=DeadlineExceeded` to the scheduler via the
  unified idempotent `AdminService.ReportAttemptOutcome{job_name =
  job.metadata.name, reason = DEADLINE_EXCEEDED}` (the C4/C5 unification ---
  the controller no longer calls `ReportExecutorTermination` for any pod;
  that RPC stays served for the stream path until it retires). With
  `restartPolicy:Never` + `backoffLimit:0` +
  the `job-tracking` finalizer the SIGKILL'd Pod IS listable with
  `terminated.reason="Error"` for the grace window, but `Error` is
  non-promoting so `report_terminated_pods` skips it; the Job condition is
  observable for `JOB_TTL_SECS=600` (\~60 reconcile ticks). The scheduler
  resolves the report against the open-attempt view: a matching open attempt
  gets the idempotent `termination_reason` second-installment fill
  (#rref("sched.executor.report-idempotent")), and a report that matches no
  attempt is acknowledged charge-free
  (#rref("sched.attempt.no-attempt-no-op")). Iterates the
  already-listed
  `jobs.items` --- no extra apiserver call. Best-effort (RPC error logged,
  reconcile continues). Defense-in-depth behind the worker-side `daemon_timeout`
  → `TimedOut` primary path.
]

#r("ctrl.pool.eviction-grammar-pinned")[
  A classifier quantifying over an EXTERNAL system's message grammar
  MUST pin that grammar as in-repo fixtures sourced from the upstream
  constants, with the upstream file and version named: the
  pod-attributed eviction discriminator carries one needle per
  kubelet local-storage grammar (emptyDir sizeLimit, pod-aggregate
  ephemeral, per-container ephemeral --- `helpers.go`, kubernetes
  v1.33), including lanes unreachable for the current fleet pod
  shape, and the version-stamped battery renders every upstream
  format --- positives and the ambient node-condition negatives alike
  --- so upstream drift or a future pod-shape change flips a test
  instead of silently inflating the `shape=other` readback.
]

The round-13 instance (merged_bug_036, the R31′(iv) jurisdiction
exemplar): the inherited needle set matched two of kubelet's three
pod-attributed grammars, the doc claimed "the two grammars are
kubelet's own", and the fixtures were derived from the
implementation's own needles --- structurally unable to detect the
enumeration miss (the same class had already bitten pre-campaign:
2acd1b32's string mismatch). The miss is LATENT for the current
single-container requests==limits builder pod (kubelet checks
emptyDir → pod → container in order, so the pod-aggregate lane fires
first) and arms the moment a sidecar lands. The wire letter is
unchanged: all three pod-attributed grammars fold to the same
in-process split letter and `EvictedDiskPressure` wire reason --- the
recorded carrier conditional did not fire (zero wire this close).

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

#r("ctrl.pod.tgps-default+4")[
  Every executor Job pod template MUST set the AD5 abort grace of `45`
  seconds as its `terminationGracePeriodSeconds` --- SIGTERM is an abort
  (cgroup-kill plus one bounded report attempt plus log finalization,
  #rref("ctrl.drain.sigterm")), not a drain, so the grace is sized to the
  abort-and-report bound. The pod template MUST NOT render a dispatch-mode
  discriminator (`RIO_DISPATCH_MODE`): pull is the only delivery protocol,
  the Pool CRD carries no `dispatchMode` field, and the stream-era
  `terminationGracePeriodSeconds` spec override (the 2 h / 600 s drain
  graces for the deleted finish-if-you-can semantics) is retired with it.
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

#r("ctrl.pool.tick-ordering")[
  Within one Pool reconcile tick the controller MUST (1) poll
  `GetSpawnIntents` BEFORE listing Jobs, so `queued` and the Job census the
  reap arms compare against come from the same tick; (2) run
  `reap_stale_for_intents` over the FULL intent set (not the
  headroom-truncated spawn slice) before the spawn pass, exclude the names it
  reaped from the existing-name skip set so the post-reap respawn attempt
  goes out the same tick, and subtract reaped active Jobs from the active
  count before the headroom clamp so freed slots are spendable the same tick
  without overshooting the ceiling; and (3) judge every destructive arm
  against state no older than this tick's intent poll --- the excess-pending
  delete additionally re-checks the live pod phase at delete time
  (#rref("ctrl.ephemeral.reap-excess-pending")), and spawn, ack, and reap
  within one tick all act on that tick's poll, never the previous tick's.
]

The ordering constraints above are the I-183 lesson: spawn-only is half a
control loop, and a reap that compares this tick's Job census against last
tick's queue (or vice versa) deletes Jobs for work that arrived between the
two reads.

#r("ctrl.pool.degraded-polarity")[
  A failed or distrusted input degrades each consumer in that consumer's
  pre-registered direction --- per consumer, not per RPC. For a tick whose
  `GetSpawnIntents` poll failed: spawn MUST treat the queue as empty
  (fail-open: spawn nothing new), the excess-pending and stale-intent reaps
  MUST treat `queued` as unknown and skip
  (#rref("ctrl.ephemeral.reap-excess-pending")), and the orphan-running reap
  MUST NOT trust `ListExecutors` output that is errored, empty, or from a
  leader younger than the orphan grace
  (#rref("ctrl.ephemeral.reap-orphan-running")). The placeable gate's
  unarmed and CRD-absent postures are per
  #rref("ctrl.nodeclaim.placeable-gate"). In the NodeClaim-pool reconciler:
  the Pool-coverage filter fails open (a transient Pool LIST error skips the
  filter for one tick rather than dropping every intent), `cover_deficit`
  fails closed when the global ceilings are not yet loaded
  (#rref("ctrl.nodeclaim.anchor-bulk")) and MUST drop --- never cover ---
  intents whose cell is absent from the loaded hw-class config, and after
  `BOT_TICKS_BEFORE_CONSOLIDATE_ONLY` (5) consecutive failed polls the
  reconciler MUST switch to consolidate-only mode
  (#rref("ctrl.nodeclaim.consolidate-only-degraded")). A new consumer of a
  shared input MUST pick its degraded direction explicitly; the same RPC
  error is fail-open for some consumers and fail-closed for others by
  design.
]

#r("ctrl.pool.demand-completeness")[
  The per-reconcile demand view is a PAGE of the scheduler's intent
  stream, and absence from a page is not absence from demand. (1) The
  producer MUST emit per-POPULATION-CLASS demand aggregates (Ready and
  forecast as distinct typed classes) at the same chokepoint that emits
  the intents, and a demand bound driving destructive reaping MUST sum
  the typed classes --- never a single-class aggregate against a
  mixed-class page. (2) Negative evidence (a membership/absence
  judgment over the view) MUST be obtainable only through an accessor
  that consumes the view's completeness witness; on an incomplete view
  the only lawful absence verdict is "unknowable", and every
  destructive absence-keyed arm (orphan-pending reap, excess reap)
  MUST suspend or re-derive from a controller-local complete inventory
  while the view is incomplete. (3) Consumers whose correctness
  depends on stream TOTALITY or cross-window CONTINUITY (re-ack/re-arm
  lanes, evidence-expiry clocks) are a third declared consumer class:
  each member MUST either derive from a controller-local complete
  inventory (e.g. the controller's own Job LIST) or suspend its
  evidence-expiry while the view is incomplete; membership in the
  class MUST recruit structurally (a typed page-scoped walk plus a
  generated consumer census), never by doc filing. Per-held-element
  walks remain lawful on any page exactly because their conclusions
  are per-element; a per-element walk that quietly aggregates into an
  absence judgment is class (2).
]

The wave-9 B3 window (limit 2048) made the page boundary real: a
forecast-backed Pending Job whose intent rotated off the priority head
was counted by neither the Ready aggregate nor the page, and the
orphan/excess reaps read its absence as demand-gone (merged_bug_006,
merged_bug_029); the re-ack lane and the streak touched-expiry filtered
the page despite restart-totality/continuity contracts (merged_bug_049).
The completeness witness already rode the wire (`truncated`); this rule
makes consuming it structural rather than per-consumer diligence.

#r("ctrl.pool.window-visibility+2")[
  Demand-visibility is the DEFAULT for any intent with a live Job:
  the FFD tick's `job_held` set MUST travel inside the published
  placeable tick, and the gate fold MUST keep a held intent
  demand-visible (want-map membership, queued count) whatever its
  placement disposition --- a held `PlacedInFlight` or unplaceable
  intent is filtered from the SPAWN lane only, never from demand.
  Structurally, a narrowing of a witnessed page MUST state its
  disposition: it either THREADS the held set (held intents survive
  the narrowing by page-type construction, regardless of the per-arm
  predicate) or declares itself LOSSY, which degrades the coverage
  letter at the page itself --- the letter's single read path, so
  every reader class binds the degraded letter
  (#rref("ctrl.pool.letter-degrades-whole")) --- a completeness
  witness binds to the view it was minted from, and a policy mutation
  after the mint must never type-check into true-negative absence
  evidence.
]
The `+2` revision relocated the degradation site: the letter now
degrades AT THE PAGE (the bug_064 close) rather than "inside the
absence lane's sole constructor" --- the old wording was the bug's
own quantifier (only absence consumers paid the lossy witness).
The premise this law replaces --- "an in-flight placement normally
has no Job" --- was falsified by the same wave-10 commit that stated
it: the admission window's `job_held` exemption admits held intents
to the fit-check, which can re-place them onto unregistered claims
(merged_bug_047). Pre-law, the gate fold stripped exactly those
placements while the transport `Complete` witness survived the page
mutation, so the still-wanted Pending Job read `AbsentFromDemand` and
was foreground-deleted single-tick at the 10s grace, inside the
50-90s claim-registration window --- delete/respawn churn during
capacity events, plus a queued undercount feeding the excess-pending
arithmetic.

#r("ctrl.pool.one-demand-source")[
  "Demand-holding" MUST have exactly one producing derivation, keyed
  on the Job inventory the destructive consumer iterates: the gate
  fold's held-threading and the orphan reap MUST consume one union
  --- every active, non-terminating Job's spawned intent plus the
  FFD tick's pod-annotation holds --- never a predicate re-derived
  from a single inventory; and whenever an active Job's intent is
  unrepresentable in that union the narrowing MUST degrade the
  coverage letter (absence becomes unknowable; destructive
  absence-keyed arms suspend) rather than keep a Complete witness
  the union does not entail.
]

The round-12 instance (bug_103) is the merged_bug_047 incident shape
one inventory over: the held-threading was keyed on the POD inventory
(`PodSnapshot::derive` inserts only pods carrying the intent
annotation) while the orphan reap keys candidacy on the JOB inventory
--- and Job-held strictly contains pod-held during pod-creation gaps
(Job-controller lag, ResourceQuota refusal, webhook failure,
eviction-recreate), which self-correlate with capacity pressure. A
live Pending Job whose pod did not exist yet was stripped by the
Unplaced arm, the page kept its Complete witness, and the still-wanted
Job read `AbsentFromDemand` --- foreground-deleted single-tick after
the 10s grace, no strike, no attempt veto. Two inventories defining
one predicate diverge exactly under the lag regime where the consumer
deletes; the union producer (`demand_held_intents`) is the R33 form:
one quantity, one producer, every consumer imports.

#r("ctrl.pool.letter-degrades-whole")[
  A degraded evidence letter MUST be degraded for every reader class
  before any consumer binds it: the demand-coverage letter lives ON
  the page, has exactly ONE read path (the page accessor, fusing
  transport truncation with every lossy narrowing since the mint),
  and no count, bound, or absence consumer can obtain a less-degraded
  copy. The letter's reader census MUST derive from the LETTER TYPE's
  consult grammar (every accessor consult and letter bind), never
  from the readers of one wrapper.
]

The round-13 instance (bug_064): the merged_bug_047 close fused the
lossy bit "inside the absence lane's sole constructor" and its census
quantified over readers-of-WantMap --- but the letter had a second
reader class: the bound lane read the raw transport copy
(`DemandEvidence.complete`) split off the page before the narrowings,
so a lossy gate fold (an active Job with no parseable intent-id)
suspended only the orphan reap while `reap_excess_pending`
foreground-deleted still-wanted Pending Jobs against the understated
count, and the warn disclosed only the suspended half. A plain census
over `.coverage()` consult sites would have surfaced the sibling
consumer; the reader census is now type-derived and the raw copy no
longer exists to read.

#r("ctrl.pool.echo-provenance")[
  Echo-integrity laws carry a PROVENANCE axis: the spawn-time
  `rio.build/intent-cells` stamp is the SINGLE echo source for every
  re-ack lane --- a re-ack channel MUST NOT re-derive its cell
  payload from live read-time state (the rendered page, a re-solve, a
  masked view); the live copy may CONFIRM the stamp but never
  SUBSTITUTE it, a celled Job without a stamp produces NO re-ack row
  (the scheduler keeps its last-armed truth), and the re-ack
  emission lanes MUST be a derived census (machine-walked over the
  lane grammar with the provenance named per lane), never a hand
  list.
]

The round-12 instance (bug_124): the on-page re-ack arm echoed the
read-time render --- `GetSpawnIntents` applies the scheduler's live
ICE mask OUTSIDE the solve memo --- while every Armed re-ack
unconditionally overwrites `dispatched_cells` and the first pull
clears the ICE ladder under an exactly-one-cell proof premised on the
pod's SPAWN-TIME affinity. A 2-cell spawn whose render shrank to 1
let a pod frozen on OR-of-both clear the WRONG cell's ladder with
zero launch evidence: the same N−1 forgery the merged_bug_134 law
forbids, reached through the unsealed echo-PROVENANCE axis (that seal
enumerated decode/skew only --- the quantifier-shape gap). A re-ack
channel re-deriving its payload from live state drifts precisely when
the system acts. The reader census is split-form: the in-crate
emission lanes are walked and tagged in the controller; the
scheduler-side `dispatched_cells` writer sites are expected members
of the round-12 registry's workspace-union row.

#r("ctrl.pool.wait-envelope")[
  An intent's lawful-wait envelope is ONE quantity with ONE eta-aware
  producer, and every lifetime horizon MUST derive from it by import,
  never by re-derivation: the Job deadline (the build window plus the
  envelope), the pod idle bound, the spawn-stamped idle-exit
  annotation, the orphan-reap grace, and the orphan leader-freshness
  gate (per-candidate) all consume the one producer; the scheduler's
  token expiry is the cross-crate sanctioning sibling (it already
  carries the eta term and MUST keep sanctioning every wait the
  envelope permits).
]

The round-12 instance (merged_bug_136) is the sibling-axis miss
hitting the same mechanism twice: the wave-10 bug_078 close
discharged the token and idle axes but left the Job-deadline horizon
and the freshness gate's leader-age conjunct eta-blind --- a forecast
pod with eta ≳ deadline was DeadlineExceeded-killed mid-lawful-wait
(k8s's clock spans Pending+Running) while two siblings sanctioned the
wait, and the `daemon_timeout = jobDeadline − 90` fires-first
contract inverted for any pod that waited >90s. Five hand-written
copies of one quantity were five independent miss opportunities; two
had hit. The producer is `pod::wait_envelope`; the worker timeout is
denominated in the build window alone so fires-first holds at every
lawful wait.

#r("ctrl.pool.container-overhead+2")[
  The container memory limit binds the WHOLE container --- the worker
  daemon, FUSE client, and log capture are resident beside the build,
  and the per-build sub-cgroup carries no limit of its own --- so the
  rendered container memory MUST exceed the solved build size by the
  worker overhead pad, floored at the container minimum:
  `container_mem = max(solved + WORKER_MEM_OVERHEAD_BYTES,
  CONTAINER_MEM_MIN_BYTES)`. The pad, the floor, and the law itself
  MUST live in the ONE shared cross-process home
  (`rio_common::footprint::container_mem_bytes`) --- the law is a
  process-boundary law, not a controller-local one --- and MUST be
  applied in the ONE footprint constructor consumed by the pod stamp,
  the FFD fit-check, and the NodeClaim floor alike (the
  simulator-shares-accounting contract on the memory axis); the
  container resource map MUST be buildable only from that footprint
  --- a raw solved size MUST NOT be writable into container resources
  at the stamp seam. The solve and its telemetry are untouched: the
  pad is additive at the container seam, applied after any
  floor-ladder clamp of the solved dimension, so ladder algebra is
  unchanged.
]

The live_058 incident is the production specimen: a warm tiny fit
(~45-69 MB solved) stamped request==limit landed below the worker's
own baseline; the kernel OOM-killed the container before the build,
and the same-size requeue looped at ~2.75 h per iteration. The
sub-cgroup refinement (a per-build `memory.max = solved` restoring
exact CgroupOom attribution) is a RULED named candidate --- builder
plane, trigger: CgroupOom attribution noise observed at the padded
limit. The shared-home clause is the merged_bug_016 amendment: the
round-10 close minted the constructor controller-locally and the
coupled cross-process admission predicates kept comparing the bare
solve --- the dead band the next rule exists to forbid.

#r("ctrl.pool.gate-superset")[
  Every predicate that decides whether a class or global memory
  ceiling can host demand --- the scheduler's producer gate and
  post-finalize chokepoint, the controller's fallback admission, and
  the provisioning partition --- MUST compare the ONE constructed
  container quantity (`rio_common::footprint::container_mem_bytes`)
  against the ceiling, never the bare solve, so that provisioning
  admits every solve placement admits and the admission/provisioning
  dead band is empty by construction. Every dispatch funnel that pins
  demand at a ceiling (the global dispatch clamp, the at-cap floor
  catch-up, the stale-solve re-solve clamp) MUST pin at the inverse
  map (`max_hostable_solve_mem(ceiling)`) so pinned demand renders a
  container of exactly the ceiling --- hostable, and the designed
  bounded at-cap terminal stays reachable (`sys.liveness.exit-edge`).
  A per-side numeric constant (pad, margin, reserve) introduced on
  one side of the process boundary MUST instead enter the shared
  home; the per-side gate-vs-law agreement witnesses MUST quantify
  over the band boundary cells rendered from the shared maps, so any
  per-side constant drift goes red at the knife edge.
]

The wire is deliberately untouched: `GetHwClassConfig` ships
container-domain ceilings (the controller-mirror law,
`scheduler.sla.ceiling.controller-mirror`), and both ends construct
their compared quantity from the same crate constants --- the
shared-constant arm of the close. The time axis (mirror refresh skew,
bounded at 300s) is carried by the mirror law at its own strength;
the transient skew population self-heals by requeue and is exactly
the population the OverCap advisory verdict was designed for. The
scheduler's solve-side candidate prefilter (`evaluate_cell`) remains
bare-solve by design: it is a producer-tier heuristic whose output
the padded post-finalize chokepoint re-checks in totality
("correctness-of-output regardless of correctness-of-producer"), so
its bareness can admit only candidates the chokepoint then strips
--- the complement is the chokepoint's totality plus the
band-boundary witnesses, stated here so the disposition is explicit.

#r("ctrl.pool.spawn-once")[
  Job identity is the deterministic name `job_name(pool, kind,
  intent_suffix(intent_id))` --- one intent maps to one Job name per Pool,
  and respawn is idempotent: a create that returns 409 AlreadyExists MUST be
  treated as "this intent's Job already exists" (skip; do not error, do not
  ack), never retried under a different name. Jobs are create-once; a
  re-queued derivation re-creates the SAME name, so the terminal-collision
  arm of `reap_stale_for_intents` is what unblocks a respawn, not a name
  change. A spawn error MUST NOT abort the remainder of the tick.
]

#r("ctrl.pool.intent-candidate-set")[
  Every destructive verdict that depends on "where can this intent run /
  what did the controller render" MUST consume the ONE intent-decided
  axis-set projection (`RenderInputs`): the pod render constructs it once
  and stamps `fingerprint()` --- which MUST cover every render-decided
  axis (selector, affinity, exclusions, resources, deadline) --- as the
  drift annotation; the AD2 spawn gate evaluates exhaustion over the same
  projection's admission predicate; and FFD packing MUST consult the
  exclusion axis through the same predicate (an excluded node is not
  simulated capacity, and a binding on a since-excluded node falls
  through to the fit-check instead of freezing as placed). A render axis
  absent from the projection is a defect by construction (the
  field-sensitivity contract test fails).
]

#r("ctrl.pool.no-eligible-persist+5")[
  The AD2 `NoEligibleSource` REPORT --- the verdict that poisons the
  derivation scheduler-side --- MUST NOT fire on a single-tick exhaustion
  observation NOR on a reconcile-count alone: the gate withholds the
  spawn from the first gated tick, and reports only after the exhaustion
  persists `NO_ELIGIBLE_SOURCE_PERSIST_TICKS` consecutive OBSERVED
  reconcile ticks OF THE OBSERVING POOL for that intent AND
  `NO_ELIGIBLE_SOURCE_PERSIST_FLOOR_SECS` of wall clock from the
  streak's first observation (reconciles are event-driven; a Job-event
  burst can deliver the count in under a second). Streak state MUST be
  keyed by (namespace-qualified pool, intent): one pool's tick MUST NOT
  clear or advance another pool's streaks, and same-named pools in
  distinct namespaces are distinct streak owners. Exhaustion evaluation
  MUST cover every wanted intent independent of the spawn window. A
  reconcile whose gate fold is skipped MUST retain streaks WITHOUT
  stepping them (an unobserved tick is evidence in neither direction);
  a pool's streak MUST reset only when a completed fold EVALUATED the
  intent and its gated set no longer contains it --- an intent absent
  from a fold's evaluated set (headroom-truncated, job-pending, or
  absent from the stream) is unobserved THIS fold and MUST retain
  without stepping --- and any entry unstepped for the bounded orphan
  window MUST expire (stale evidence MUST NOT complete a poison): this
  covers removed pools and frozen streaks alike. A controller restart
  MAY restart streaks (delaying a genuine poison by at most the
  persistence window). A fold-skip tick MUST NOT spawn an intent whose
  retained streak is live (younger than the orphan window): fail-open
  spawn applies only to intents bearing no live exhaustion evidence ---
  spawning a suspected-exhausted intent makes it structurally
  unobservable for at least its Job deadline, which exceeds the orphan
  window, destroying by spawning what the retain law preserved.
]

#r("ctrl.pool.respawn-backoff+5")[
  A pool MUST NOT respawn an intent whose previous Job died without
  the scheduler holding a verdict for an attempt of that intent,
  except behind the exponential backoff floor (base = reconcile
  cadence, documented cap). A respawn record MUST reset only on
  verdict-bearing evidence --- an ack witnessing that the scheduler
  resolved an attempt, an acked NoEligibleSource report, an open build
  attempt in the ledger view, or a recently-closed build attempt that
  postdates the death(s) it resets; reap-time coverage MUST bind the
  close to the reaped Job's generation (a close minted before the Job
  existed covers nothing). An acknowledgment carrying no
  attempt-resolution witness MUST NOT reset. A respawn record MUST NOT
  expire during any cycle phase in which its intent is structurally
  unobservable to the gate fold --- a live or terminal same-named Job
  in the tick's listing refreshes it; the orphan expiry applies only
  to jobless silence. A record at or past the give-up threshold MUST
  decay on a fresh demand epoch, keyed on epoch CHANGE, never order: a
  wanted intent presenting a resubmit cycle different from the one
  the record last observed --- newer or REWOUND --- decays the record
  the same tick under an evidence-carrying reset whose evidence is the
  demand epoch, not a pod (the gave-up state forbids pods, so a
  pod-derived exit edge is unreachable from inside it; and the epoch
  is a demand SIGNAL, not a monotone counter --- scheduler lanes
  lawfully rewind it to zero, so an order comparison imports a
  monotonicity contract the producer does not own and silently
  absorbs exactly the documented operator-recovery population);
  equality alone latches (anti-replay needs only equality); a failure
  streak under the changed epoch MUST re-latch at the full give-up
  budget. A mid-ladder record MUST NOT decay on resubmission --- its
  backoff window expiring is that state's own exit edge, and an
  epoch-triggered mid-window reset would let resubmission spam bypass
  the breaker.
]

#r("ctrl.pool.giveup-exit-mintable")[
  The gave-up latch's exit edge MUST be satisfiable by values its
  producer can mint from every latched configuration: each derivation
  state a verdict-free give-up can leave latched maps to a documented
  recovery action (explicit resubmission of the drv; `ClearPoison`)
  whose observed demand epoch provably differs from the latched one,
  and the decay seam MUST fire on any such changed observation the
  same tick. The latched-configuration-to-mintable-face mapping MUST
  be a derived census over the producer's state alphabet (the
  scheduler's resubmit classification), never a hand list of value
  faces.
]

The round-13 instance (bug_058, R30's producer-reachability face): the
wave-12 close enumerated the observed-VALUE faces {unseen, same,
newer, rewound} and keyed the decay on change --- correct at this
seam --- but never derived which faces the PRODUCER could mint per
latched state. The scheduler's only cycle mint sat behind the
retriable band, a verdict-free give-up leaves the drv `queued`/`ready`
(outside it), so "same" was the only mintable face for exactly the
latched population: the documented recovery contract was structurally
dead, and the build hung silently with both recovery actions consumed
(`ClearPoison`'s rewind-to-0 was an equality fixed point at the common
cycle-0 latch). The close is producer-side
(#rref("sched.resubmit.epoch-total")); this rule pins the consumer
half: the exit edge quantifies over (latched configuration ×
producer-emittable faces), not over the value domain.

#r("ctrl.pool.ack-spawned-soundness")[
  The Pool reconciler MUST ack `AckSpawnedIntents{spawned}` only for intents
  that have a Job behind them at ack time: intents whose create succeeded
  this tick, plus intents whose Job is already Pending from a prior tick
  (the re-ack that re-arms a restarted scheduler's `dispatched_cells`).
  Intents whose create failed, name-collided, or was skipped MUST NOT be
  acked --- acking a Job-less intent arms the scheduler's heartbeat-edge ICE
  clear for a pod that will never heartbeat. Names reaped this tick MUST be
  excluded from the already-Pending re-ack set in the same tick.
]

#r("ctrl.pool.fetcher-hardening+3")[
  For `kind=Fetcher`, `executor_params` MUST apply ADR-019 hardening regardless
  of spec: `readOnlyRootFilesystem: true`, `seccompProfile: Localhost
  operator/rio-fetcher.json`, `hostUsers: false`, `privileged: false`, default
  `rio.build/fetcher: true` nodeSelector (§13e key, restored in B4) +
  `rio.build/fetcher:NoSchedule` toleration. CRD CEL rejects fetcher specs
  that set the overridden fields at admission time; the reconciler override
  is belt-and-suspenders for pre-CEL specs the apiserver already accepted.
  (Fetcher pods carry the same 45 s AD5 abort grace as builders ---
  #rref("ctrl.pod.tgps-default"); the former 600 s stream-drain grace retired
  with the dispatch-mode knob.)
]

#r("ctrl.pool.fetcher-spawn-builtin")[
  For `kind=Fetcher` pools, `spec.systems` SHOULD include `"builtin"` so
  `system="builtin"` FODs are counted in the spawn signal. Every executor
  unconditionally advertises `"builtin"`
  (#rref("sched.dispatch.fod-builtin-any-arch")); omitting it from the spawn
  signal would stall a cold-store bootstrap.
]

#r("ctrl.pool.disruption+2")[
  The DisruptionTarget watcher selects on `POOL_LABEL` (which the reconciler
  stamps on every kind=Builder AND kind=Fetcher pod), so fetcher pods gain the
  same fast-preemption behavior as builders: K8s sets `DisruptionTarget=True` →
  the watcher synthesizes the preempted report and foreground-deletes the
  owning Job → the build aborts and requeues in seconds instead of burning the
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
    [get, list, watch, patch --- per-tick LIST inventory (`PodSnapshot`);
      `watch` for the DisruptionTarget preemption watcher and the pod
      annotator; `patch` for node_informer's `rio.build/hw-class`
      annotation stamp],

    [`""` (core)],
    [nodes],
    [get, list --- node_informer per-need GET / per-flush LIST (hw-band
      label join; the Node watch was retired with `NodeLabelCache`)],

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

#r("ctrl.informer.interrupt-sample-conservation+2")[
  Every `SpotInterrupted` Event observed by the spot-interrupt watcher MUST
  be either APPENDED to the scheduler as the λ-numerator interrupt sample
  or counted as a typed drop on
  #(refs.metric)("rio_controller_spot_interrupt_dropped_total")`{reason}` with a warning ---
  a silent skip is not an outcome, and the identity is total over
  DELIVERY: an attributed sample whose append RPC fails MUST exit
  through the same counted chokepoint (`reason = append_failed`), so
  `observed = appended + Σ dropped` holds even while the scheduler is
  unreachable. An attributed sample is constructible only from the
  attribution path and consumable only by the counted appender. Attribution resolves the node's labels
  by per-need GET first; when the node is already gone or unreadable (the
  COMMON reclaim case --- the instance is deleted moments after the Event)
  the watcher MUST fall back to the exposure flush's `name → hw_class`
  observation map (refreshed every flush LIST, pruned at 2× the Event TTL
  bound). A present node whose labels match no configured class is a
  `no_hw_class` drop even when a stale fallback entry exists --- labels or
  config changed, and the stale class would mis-attribute.
]
Every uncounted drop under-reports the spot-reclaim rate exactly while spot
is being reclaimed --- the anti-conservative direction for the SLA solver's
capacity-type decision.

#r("ctrl.informer.exposure-recredit+4")[
  The λ-denominator exposure leg of `AppendInterruptSample` MUST consume
  its evidence only on append acknowledgement, and every shipment MUST
  carry a deterministic per-(cluster, class, window) idempotency key
  (`exposure:{cluster}:{hw}:{window-slot}`, constrained by the same
  partial unique index as the interrupt leg's Event uids) so an ambiguous
  failure --- server committed, client timed out --- redelivers into the
  `ON CONFLICT` absorb instead of double-banking the denominator. The
  cluster axis is REQUIRED: the index is table-global in the shared-PG
  topology (ADR-023 §2.13), so an axis-free key from two clusters'
  informers collides and the absorb silently and permanently drops the
  losing cluster's window. The window component MUST be grid-aligned
  to the flush period (the slot START, a multiple of the flush
  cadence) so concurrent same-cluster informers --- the rollout surge
  twin --- converge on ONE row per logical window instead of
  double-banking it, and window identity MUST be strictly monotone per
  process (minted only through a gate that refuses a non-advancing
  slot) so a clock step backward cannot re-mint an already-shipped
  window under fresh seconds --- absorbed, counted delivered, lost. A
  banked slice whose append RPC fails TRANSIENTLY MUST be re-credited
  whole and
  IDENTICAL (uid and value; windows are never merged --- a merged value
  under an already-committed key would be absorbed and the fresh
  seconds lost) to the next flush, and a retained class with no fresh
  slice in a later round MUST still retry --- so PENDING provably means
  "deliverable by some future pass". Every node-second that
  leaves a cursor MUST exit through exactly one of BANK (append
  acknowledged), PENDING (the retained queue), or a COUNTED drop on
  #(refs.metric)("rio_controller_spot_exposure_dropped_seconds_total")`{reason}`;
  the counted forfeitures are exactly: `no_hw_class` (a spot node
  matching no configured class --- the cursor advances by design, no
  retro-bank), `absent_node` (a node deleted between flushes forfeits
  its final partial slice --- one flush period in the common case,
  growing with the gap across LIST-failure streaks), `shutdown`
  (the pending queue is process memory; shutdown forfeits the WHOLE
  backlog, one counted drop per slice --- there is no single-window
  bound), and `refused` (the scheduler refused the slice's content or
  shape --- re-sending the same bytes cannot succeed; counted in the
  pass that observes the refusal instead of recirculating forever).
  An identity (auth) refusal under the per-request service credential
  is presentation-judging (`sec.authz.refusal-adjudication`: it judges
  one presentation under one key observation, never the next fresh
  mint) and MUST exit counted only at the typed auth-strike budget ---
  with the observation count disclosed at the exit --- never on a
  single observation; below the budget the slice MUST be retained
  exactly like a transient failure (one strike per observation,
  monotone, untouched by interleaved non-auth transients). A LIST
  failure forfeits nothing (cursors untouched), and a
  non-advancing flush window likewise forfeits nothing (banking
  deferred, cursors untouched --- the next admitted window banks the
  full delta); a scheduler outage spanning any number of flush windows
  MUST NOT reduce total banked exposure; and a process (re)start MUST
  seed each
  cursor at `max(creationTimestamp, process boot)` so a restart cannot
  re-bank windows the previous incarnation already shipped (the
  pre-boot residual is forfeited toward the conservative direction ---
  λ reads marginally high).
]
The pre-fix shape advanced every cursor before delivery and handled append
failure with a warning alone: each failed flush permanently lost
`fleet x window` node-seconds of denominator, biasing λ high precisely
during scheduler rollouts --- while the numerator leg of the same RPC counted
its identical failure through the typed-drop chokepoint. The first fix
round (bug_150) re-credited failed slices but left the retry un-keyed
(quadratic double-banking under commit-but-timeout brownouts,
merged_bug_002) and claimed two bounded forfeitures while the
implementation had five unbounded-or-uncounted ones (merged_bug_070).
The second round (merged_bug_002) keyed the retry deterministically but
minted the key axis-free against the table-global index --- colliding
across clusters, missing the same-cluster co-run collision it existed
to absorb, and re-mintable under a backward clock step --- which
merged_bug_001 closed with the typed, cluster-scoped, grid-aligned,
monotonically-gated key this rule now requires (Q2-round5: the axis
rides the uid FORMAT; M_047 stays frozen). The cluster axis's VALUE
distinctness across deployments --- which the typed key cannot enforce
--- is `ctrl.informer.cluster-identity-boundary` below (bug_022).

#r("ctrl.informer.exposure-drain-budget+3")[
  The pending-queue exposure sweep MUST bound each drain pass by a
  wall-clock budget and MUST be preemptible by shutdown both between
  and during shipments (the in-flight append raced against
  cancellation), so the shutdown arm's counted whole-backlog
  forfeiture is reachable within the pod termination grace at ANY
  backlog depth; each pending slice MUST be attempted at most once
  per drain pass --- pass work is bounded by queue length, never by
  the budget/error-latency ratio, and the flush period itself is the
  retry pacing; a REQUEST-DISPROVING refused slice MUST exit through
  the counted-drop chokepoint in the pass that observes the refusal,
  while a presentation-judging (auth) refusal exits counted only at
  auth-strike-budget exhaustion (`sec.authz.refusal-adjudication`)
  and is otherwise retained; a budget-deferred or preemption-requeued
  slice remains PENDING --- never a drop.
]
Pre-fix, the flush arm shipped the ENTIRE unshipped backlog serially
inside one `select!` arm body (each shipment riding a 5s admin-RPC
timeout, the backlog uncapped by design, every failure re-queued).
`select!` cannot preempt a running arm body, so under a live-but-stalled
scheduler the sweep grew ~5s × backlog and the biased shutdown arm ---
the ONLY site of the counted `shutdown` forfeiture --- went unpolled:
SIGTERM mid-sweep ended in SIGKILL with ZERO disclosure of exactly the
permanent denominator loss the recredit rule exists to make visible
(merged_bug_033). Preempting an in-flight append is safe ONLY because
of the keyed identity above: the aborted append is the ambiguous
commit-or-not case, and the verbatim-requeued slice redelivers into the
absorb.

#r("ctrl.informer.cluster-identity-boundary+1")[
  The cluster identity axis MUST be value-distinct across deployments
  that share a PG: the chart MUST refuse to render an empty cluster id
  when the external-secrets PG path (the shared-capable topology
  declaration) is enabled, and the informer MUST disclose the
  single-cluster default loudly at activation, so a cross-deployment
  uid collision is constructible only past two explicit boundaries.
  The axis has exactly ONE normalization law (trim; post-trim-empty =
  the single-cluster default): the chart's identity emission MUST be
  normalized by the same law as the runtime constructor, the render
  gate MUST evaluate the normalized value --- the refusal set is
  "every value the runtime classifies single-cluster-default", never
  the raw-empty point --- and the chart-side and constructor-side
  predicates MUST be pinned to one committed cross-boundary fixture.
]
One-time re-key disclosure (merged_bug_067, non-normative): a
deployment that previously ran a whitespace-padded cluster value
re-keys its `sla_ema_state`/`interrupt_samples` scope on upgrade (the
padded scope's rows age out of the λ window; the EMA reseeds) --- this
is the fix taking effect, not a residual on a safety property: the
affected population is installs that hand-wrote padded values past
every documented path (R19 check: no proviso minted).

Presence-in-type (the `ClusterId` axis the recredit rule demands)
cannot close VALUE distinctness: two deployments both at the empty
default mint byte-identical `exposure::{hw}:{slot}` uids every window,
and the scheduler's `ON CONFLICT (event_uid) DO NOTHING` absorb counts
the loser as the designed at-most-once outcome --- silent, permanent,
cross-deployment λ-evidence loss that no in-process check can detect
(the colliding uids are byte-equal; the winning row's `cluster` stamp
equals the loser's). The close is therefore LAYERED at the two
boundaries that can act: render time --- `externalSecrets.enabled` is
the chart's ONLY render-visible declaration of a shared-capable PG
(release-local `postgresql.enabled` is provably unshared; the PG
endpoint itself is Secret-borne and invisible to templates), so the
`rio.clusterIdentity` helper `fail`s the render on the empty-id with
external-secrets conjunction (zero blast radius: every in-tree values
file and helm fragment leaves it disabled, while the `xtask k8s -p
eks` path that enables it also sets `karpenter.clusterName`) --- and
activation time, where the informer warns on the residual the chart
cannot see (manual-secret external PG, out-of-chart installs).
Derivation from an unforgeable per-deployment source (kube-system
namespace UID) was REJECTED this wave: it breaks the
one-values-expression mirror with the scheduler's `[sla].cluster` row
stamp (fragment 39's law), needs a k8s-API identity channel the
scheduler does not have, and re-mints dedup identity across the
upgrade window (the frozen-M_047 cutover hazard). A controller boot
refusal on the empty default was likewise REJECTED: empty is the
legitimate single-cluster default mirroring the scheduler's
`DEFAULT ''` column, and a refusal would brick every existing
single-PG deployment. `nix/tests/helm/39-cluster-axis-single-source.sh`
carries the render gate's planted-red leg (the gate MUST fail its
planted fixture).

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

#r("ctrl.health.ready-gates-connect+1")[
  The controller binds its HTTP health server *before* awaiting
  `connect_forever` for the scheduler, and serves it from the dedicated guard
  runtime (`src/guard.rs` --- its own OS thread, schedulable when the working
  runtime is not). `/healthz` (liveness) returns 200 unconditionally once the
  guard thread starts; `/readyz` (readiness) returns 503 until the scheduler
  admin channel connects, then 200 *iff* the working runtime schedules the
  guard's no-op probe within the ready-probe budget --- a browned-out main
  runtime sheds (Endpoints removal), it is never killed. Spawning the health
  server _after_ `connect_forever` would leave nothing listening during
  scheduler cold-start and the chart's livenessProbe (`periodSeconds:10`,
  `timeoutSeconds:10`, `failureThreshold:6`, plus the 2s×30 `startupProbe`)
  would SIGTERM the pod once the startup budget lapsed --- re-introducing the
  CrashLoopBackOff that `connect_forever` was added to fix.
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

#r("ctrl.drain.sigterm+3")[
  *Scale-down:* there is no finish-if-you-can drain: pod termination
  (including graceful node drain / scale-down) is an abort --- the pod
  template carries the 45 s AD5 grace (#rref("ctrl.pod.tgps-default")), the
  builder cgroup-kills the in-flight build and makes one bounded report
  attempt, and the derivation requeues charge-free.
]

*SIGTERM handling (no preStop hook needed):* the builder's main loop has a
`select!` arm on SIGTERM. For a pull-mode pod the signal is an abort, not a
drain: the builder cgroup-kills the in-flight build, makes one bounded
best-effort `ReportOutcome` attempt inside the AD5 grace
(#rref("builder.shutdown.sigint+3")), and exits --- there is no
scheduler-side drain step (`AdminService.DrainExecutor` was removed outright
by the executor-lifecycle proto sweep --- a call today gets tonic's generic
`UNIMPLEMENTED`, and the per-executor drain it used to set has no object), no
registration to give back,
and the requeue happens at the report fold or, failing that, the
establishment sweep. A preStop hook would be redundant: K8s sends SIGTERM
on pod termination regardless, and the signal handler implements the abort.
The Job pod template does NOT define a preStop.

#r("ctrl.drain.disruption-target+4")[
  *Eviction-triggered preemption:* the controller runs a Pod watcher filtered
  to `rio.build/pool`-labeled pods with
  `status.conditions[type=DisruptionTarget,status=True]`. When K8s marks an
  executor pod for eviction (node drain, spot interrupt), the watcher MUST
  preempt by report-then-delete: resolve the pod's OPEN attempt from
  `ListOpenAttempts` and synthesize the terminal
  `ReportAttemptOutcome(preempted)` keyed by THAT attempt's `exec_id` ---
  when no open attempt exists (never pulled, or already closed) the watcher
  MUST send no report at all --- then foreground-delete the owning Job, so
  the pod's SIGTERM-abort fires within
  the 45 s grace and the requeue happens at the report fold, never the
  establishment sweep. The pod's own SIGTERM abort is the fallback if the
  watcher misses the window. There is no per-executor drain RPC --- the
  stream-era `DrainExecutor{force:true}` hop retired with the stream
  protocol. The same closed-attempt evidence drives the cancel
  successor: the reconcile foreground-deletes an active pull-mode Job only on
  the closed→active edge of the open-attempt view (an attempt previously
  observed open for that Job no longer listed by a later successful read),
  never on bare absence, and the view read stays fail-closed.
]

== Pull-mode attempt lifecycle (additive)

Executor pods dispatch via `PullAssignment`/`ReportOutcome` --- the only
delivery mode --- and the controller is the classifier of pod/Job terminal
status and the consumer of the scheduler's ledger-backed open-attempt view.
The controller has no stream-mode call sites left: the 1d controller cleanup
removed the `ListExecutors` busy consultation, the `DrainExecutor` preemption
hop, and the executor-id prefix correlation; the legacy
`ReportExecutorTermination` and `DrainExecutor` RPCs (both removed) left the
proto with the 1d sweep (whole-message deletions; see admin_types.proto).

#r("ctrl.report.attempt-outcome")[
  The controller MUST fold a pull-mode attempt's pod/Job terminal status to
  the scheduler as one idempotent `AdminService.ReportAttemptOutcome` call
  keyed by the attempt identity (exec_id when known, otherwise intent id /
  Job name), and the scheduler MUST treat it as the second-installment
  column fill on the existing attempt row (first classifier wins, `WHERE
  termination_reason IS NULL`); re-reports and reports for already-terminal
  attempts MUST be acknowledged without writing.
]
This is the C4/C5 unification: one idempotent unary replaces the
terminated-pod report and the deadline-exceeded Job report for pull-mode
attempts, with no re-report dedup window and no Job-name prefix matching.

#r("ctrl.job.synthesize-on-delete+4")[
  Whenever the controller deletes a Job that still has an open pull-mode
  attempt (cancel, preemption, or any reap path), it MUST synthesize the
  terminal `ReportAttemptOutcome` (reason cancelled / preempted / reaped as
  appropriate) for that attempt before or with the foreground deletion;
  deleting a Job with no open pull-mode attempt MUST NOT synthesize or send
  anything. The synthesized verdict MUST bind only an attempt whose executor
  identity is one the scheduler mints for the Job's own intent — the attested
  intent for build pulls, `intent@instance` for materialization claims — and
  MUST NOT bind build-lifecycle verdicts to materialization attempts. The
  synthesized verdict and the delete it rides MUST consume attempt evidence
  within a typed freshness bound; the deciding view is captured immutably at
  the wave's selection, and EVERY job's veto is evaluated against it — a Job
  whose freshest evidence shows a covering open attempt the deciding view did
  not contain MUST NOT be reaped that tick, regardless of which iteration's
  staleness refreshed the observation. A FAILED attempt-view read (initial or
  refetch) MUST defer every attempt-affecting delete that tick: an error-born
  empty view is not evidence of absence and MUST NOT adjudicate deletes or
  step the verdict-free-death backoff.
]
The deletion destroys the only Job/pod terminal status the unified report
could otherwise fold; synthesizing keeps requeue at the next fold instead of
waiting for the establishment sweep.

#r("ctrl.job.busy-from-open-attempts+2")[
  The orphan-Running reap MUST treat a Job as busy exactly when an open
  pull-mode attempt from `AdminService.ListOpenAttempts` covers it (match
  key: the Job's intent annotation), and MUST keep the fail-closed posture:
  an error on the view read means no reap this tick.
]
The open-attempt ledger is the only busy carrier --- the stream-mode
`ListExecutors.running_build` arm and the executor-id prefix correlation it
needed retired with the stream protocol, and at no point may the reap infer
busyness (or its absence) from anything other than the durable view.

#r("ctrl.job.cancel-close-cause+2")[
  The AD5 cancel arm MUST select a Job for teardown only through the
  four-conjunct binding: the scheduler's
  `ListOpenAttempts.recently_closed` window lists the Job's intent with
  `CLOSE_CAUSE_CANCELLED`, the Job PREDATES the close (the Job is older
  than the close's wire-carried age plus the documented clock-skew
  slack --- a re-submitted derivation's respawned Job postdates its
  cancelled close and is structurally unselectable, whether or not its
  pod has pulled), and no open attempt covers the intent. The close
  cause travels WITH the close on the wire; the absence of an open
  attempt is never, by itself, cancellation evidence. A normal
  completion sitting in the Job-status propagation lag window carries
  `CLOSE_CAUSE_COMPLETED` and is untouchable by type; a pod that never
  pulled matches no entry at all and is covered only by the grace-gated
  orphan reap and `activeDeadlineSeconds`; a genuine cancel younger
  than the slack is missed by at most the slack and falls to the same
  backstop pair.
]
The window (120s server-side) bounds the arm's latency: cancel-to-teardown
is at most the controller tick plus propagation, and a close that ages out
during a controller outage falls back to the orphan reap ---
the same backstop pair as before, minus the wrongful teardown of normal
completions the closed-edge inference allowed.

#r("ctrl.job.idle-render-coupled+2")[
  The controller MUST render the builder idle-exit bound (`RIO_IDLE_SECS`)
  into the executor pod env from its own constants: the flat
  `POOL_IDLE_EXIT_SECS` for Ready intents, and for FORECAST intents
  (`ready == false`) an eta-priced bound of at least
  `eta + FORECAST_IDLE_ETA_SLACK_SECS` floored at the flat bound --- no
  forecast spawn may carry an idle bound shorter than the boot horizon it
  was spawned to cover (the same horizon the executor-token mint prices).
  The rendered bound MUST be stamped on the Job (the
  `rio.build/idle-exit-secs` template annotation), and the orphan-running
  grace MUST cover the pod's own patience: per Job,
  `max(ORPHAN_REAP_GRACE, rendered_bound + 60s)`, with the flat case
  additionally pinned by the compile-time assertion
  `ORPHAN_REAP_GRACE >= POOL_IDLE_EXIT_SECS + 60s`. Pod env wins over image
  env, so the value pods actually run with is the one the coupling checks
  --- the reap/idle coupling is enforced where it can fail the build, not
  stated in prose beside constants that can drift apart.
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

#r("ctrl.scaler.signal-substituting+5")[
  The predictive `builders` signal MUST include `substituting_derivations` at
  1:1 weight with `queued`/`running`, for ANY Deployment a ComponentScaler CR
  targets. A substitution cascade with zero queued/running and a CLAIMABLE
  backlog MUST NOT produce `builders=0` --- that scales the target toward
  `min` exactly when substitution load is the thing demanding capacity. The
  field's source is the scheduler's substituting bucket: derivations with
  CLAIMABLE materialization jobs --- unclaimed, not parked, not deferred
  (#rref("sched.admin.snapshot-substituting")); claimable backlog is thereby
  visible to the predictive signal before any store replica claims the work.
  Parked and deferred jobs are pacing, not demand: they leave the bucket
  (parked stay visible via
  #(refs.metric)("rio_scheduler_materialization_stalled"); deferred
  sit in neither gauge for their bounded <=300s window), so a park/defer-heavy
  cascade reads near-zero here BY DESIGN --- the store cannot make a parked
  job progress, and holding capacity for it would defeat the pacing.
]
The store itself is no longer a ComponentScaler target: the chart defines no
store CR, and the store Deployment's replica count is owned by the KEDA
ScaledObject (#rref("infra.store.autoscaling")), whose leading trigger reads
the same substituting bucket through its Prometheus form
(#rref("obs.metric.scheduler-substituting")). This rule governs the
reconciler's signal arithmetic for whatever target a future CR names.

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

#r("ctrl.scaler.evidence-funding+2")[
  A windowed-evidence counter MUST increment under exactly the
  predicate whose sustained truth it witnesses --- fund equals spend:
  the low-load streak counts only low-load-while-WORKING ticks
  (`builders > 0 && current > min`), and every non-evidence tick
  (idle, at-min) RESETS the counter --- never caps, parks, or banks
  it. Idle is non-evidence by the growth gate's own rationale;
  banking it as redeemable credit fires the spend at the regime
  boundary with zero of the evidence the window exists to require.
  The funding predicate evaluates at the OBSERVATION-ALPHABET
  boundary: any conjunct computable without the load reading
  (working vs idle/at-min) MUST also evaluate on observation-absent
  ticks --- preserve-on-absent is reserved for the genuinely
  evidence-ambiguous cell (working, load unknown --- sensor absent or
  partial-coverage low), and even there the banked streak is
  staleness-bounded (decayed one tick per ambiguous tick), so an
  unbounded sensor outage expires the bank instead of parking it.
]

The round-12 instance (bug_147): gate-failure low-load ticks were
CAPPED at the threshold instead of reset, so any idle period ≥5min
parked the streak AT threshold and ratio growth fired on the first
busy low tick (or the second tick after the predictive scale-up) ---
zero low-load-while-working evidence, recurring on every idle→busy
transition; the companion test pinned the cap and never drove the
transition. The wave-10 close gated the SPEND but left idle ticks
FUNDING the window --- the funding predicate is part of the law.

The round-13 instance (merged_bug_009): the bug_147 reset lived only
inside the low-reading arm, so it was evaluated only when the poll
clock ticked --- with scale-to-zero targets (`replicas.min: 0` ⇒ zero
pods ⇒ zero resolved addresses ⇒ no reading) sensor absence is
CORRELATED with the idle regime the gate classifies, the reset was
structurally unreachable across the whole idle window, and the
parked streak funded growth on the first busy low tick. The repair
hoists the sensor-free classification to the observation-alphabet
boundary (the R34(iii) funding-clock face: a predicate computable
without the sensor is evaluated on sensor-absent ticks).

#r("store.admin.get-load+3")[
  `StoreAdminService.GetLoad` returns `pg_pool_utilization = (pool.size −
  pool.num_idle) / max_connections` and `substitute_admission_utilization =
  (capacity − available_permits) / capacity` for the replica it's called on.
  When a ComponentScaler CR targets the store, the reconciler polls every
  store pod (DNS-resolving the headless service); per-pod load is
  `max(pg_pool_utilization, substitute_admission_utilization)` (substitution
  can saturate independently --- upstream HTTP bottleneck while PG sits
  idle), and `observedLoadFactor` is the max across pods. The handler also
  publishes #(refs.metric)("rio_store_pg_pool_utilization") and
  #(refs.metric)("rio_store_substitute_admission_utilization") on call, so
  Prometheus sees the same values any polling controller acted on; the
  gauges' steady publication is store-owned
  (#rref("obs.metric.store-pg-pool")), not a side-effect of being polled.
]
With no store CR in the chart (KEDA owns the store replica count,
#rref("infra.store.autoscaling")), nothing polls GetLoad periodically --- the
RPC and its return values stay (rio-cli, ad-hoc diagnosis, any future CR
target), and the store's 30 s in-process tick keeps the PG-pool gauge live.

#r("ctrl.scaler.load-coverage")[
  A partial aggregate over per-replica gauges MUST carry its
  denominator: the poll fold reports `answered`/`resolved` alongside
  `max`, and `decide()` MUST consume partial coverage asymmetrically
  --- a survivor reading above `loadThresholds.high` remains scale-up
  evidence under any coverage, while ratio-growth funding demands
  total coverage; a partial aggregate is never consumed as a total
  one, and zero answers degrade to the no-reading posture rather than
  a fabricated max.
]
Rationale: a per-replica gauge's `max()` drops the unanswered replica
exactly when its reading may BE the max --- GetLoad is sub-ms when
healthy, so the slow-to-answer pod is the saturated one (the
load-correlated timeout regime recurs every pass; the round-13
instance is bug_061, where idle survivors' low readings funded ratio
growth every tick while the hot replica's timeout suppressed the
reactive scale-up). The asymmetry preserves the protective action:
degrading the whole letter to `None` would suppress exactly the
scale-up partial coverage can still justify.
#(refs.metric)("rio_controller_component_scaler_load_poll_partial_total")
is the recurrence's operator trail.

For any Deployment a ComponentScaler CR targets, the helm chart MUST omit
that Deployment's `spec.replicas` from the rendered template --- otherwise
`helm upgrade` resets the replica count and fights the controller. (The
store Deployment's own replicas gating is keyed to `store.autoscaling.enabled`
--- the KEDA branch, #rref("infra.store.autoscaling") --- since the chart
defines no store CR.) The controller's `/scale` patches use field-manager
`rio-controller-componentscaler` (distinct from helm's apply manager).

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

#r("ctrl.nodeclaim.sim-window")[
  The per-tick FFD simulation MUST be work-bounded: intents beyond a
  fleet-capacity-derived admission window --- cores-denominated,
  `(live free + budget remaining) × slack`, round-robin-fair across
  hw-class buckets with priority order preserved within a bucket ---
  are a TYPED remainder (counted, gauge-surfaced, re-seen next tick),
  never a silent drop; the window MUST be at least the mint law's
  per-tick consumption (`ctrl.nodeclaim.mint-deficit-proportional`'s
  budget term) so supply is never window-starved; and the walk MUST
  yield to the runtime between bounded chunks so a pathological tick
  cannot starve the reconciler's executor.
]
Rationale: B4 --- the unwindowed clone+sort+walk was the one unbounded
O(demand) compute pass left on the reconciler runtime (measured: 385
intents did NOT explain the 17--18s whole-runtime freezes, so the walk
is bounded structurally while the D5 skew sentinel attributes the
residual primitive; the yield quantum is a violable envelope sized at
the first sentinel-attributed freeze). The window's fairness mirrors
`cells_round_robin`: a single class's pathological demand cannot
permanently starve a sibling class's window share, preserving the
budget brake's rotation property upstream of the mint.

#r("ctrl.nodeclaim.anchor-bulk+6")[
  Unplaced intents per `(h,cap)` cell whose pod footprint fits the cell's
  per-class `(max_cores, max_mem)` and global `max_disk` cap are covered by `n`
  uniform claims at `(max(⌈Σc*/n⌉, max_i c*), max(⌈Σm/n⌉, max_i m),
  max(⌈Σd_eph/n⌉, max_i d_eph))` (uniform `div_ceil` on all three axes — the
  over-provision direction, ≤1 unit per bin), where `n` iterates upward from
  the 3-axis lower bound
  `max(⌈Σc*/cell_cores⌉, ⌈Σm/cell_mem⌉, ⌈Σd_eph/maxDisk⌉)` until the production
  FFD's MostAllocated-cpu placement order packs every fitting intent; over-cap
  intents are dropped with `intent_dropped_total{reason=exceeds_cell_cap}` (`Σ/n`
  is a bin-packing lower bound, not a guarantee). §13c-3:
  `cell_cores`/`cell_mem` are the per-class effective ceiling
  `min(HwClassConfig::ceilings_for(h), HwClassConfig::global_ceilings())` (both
  shipped over `GetHwClassConfig`, not `controller.toml`), so each claim's `(c,
  m)` chunk is hostable by some instance in `h`'s `requirements` set;
  `cover_deficit` skips the tick when the global ceiling is not yet loaded
  (fail-closed, ≤300s self-heal). NodeClaim creation is bounded by the
  two-term law `min(n_pack, ⌊budget/chunk⌋)` — demand (the FFD bin count over
  real placeable-gated footprints) and the `sla.maxFleetCores` fleet-budget
  brake, and by nothing else (the flat `sla.maxNodeClaimsPerCellPerTick`
  per-tick cap is RETIRED, live_049 L1 — its helm row is parse-only;
  #rref("ctrl.nodeclaim.mint-deficit-proportional")); cells
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

#r("ctrl.nodeclaim.budget.per-class+3")[
  `cover_deficit` clamps each cell's per-tick mint at `min(global_remaining,
  hwClasses[cell.0].max_fleet_cores − class_live − class_created_this_tick)`
  where `class_live` and `class_created_this_tick` are summed across
  capacity-types (per-hwClass, NOT per-Cell --- a per-Cell cap would let
  spot+od each hit it independently → 2× \$/hr exposure). `max_fleet_cores=None`
  ⇒ global budget only. The per-tick created-core accounting (global and
  per-class) MUST count only successful creates --- a failed NodeClaim
  create consumes no budget. The budget brake and demand are the ONLY
  mint bounds: the per-cell sizing law is the two-term
  `min(n_pack, ⌊budget/chunk⌋)` (the
  `ctrl.nodeclaim.mint-deficit-proportional` law --- the former flat
  per-cell-per-tick cap is retired, live_049 L1).
]

#r("ctrl.nodeclaim.mint-deficit-proportional")[
  Minting MUST be bounded by demand and by the fleet budget --- the two
  quantities with safety meaning --- and by NOTHING else: each cell's
  per-tick claim count is `min(n_pack, ⌊budget/chunk⌋)`, where `n_pack`
  is the FFD bin count over the actual placeable-gated unplaced
  intents (right-sizing by construction) and `budget` is the per-class
  fleet-core sub-budget. A deficit of `D` placeable-gated chunks
  within budget MUST mint fully in ONE tick.
]
Rationale (live_049 L1, the parallel-ramp verdict): the 208-claim peak
ramped at 18 ticks x 8/cell x \~1.44 slots/claim with demand drained
BEFORE the budget crossing --- the flat cap stretched the ramp while
protecting nothing: every deferred claim was demanded
(placeable-gated, the `ctrl.nodeclaim.placeable-gate+5` premise,
witnessed by the gate-population red `gate_population_feeds_zero_mint`
and the ffd lead-time greens), budget-affordable (the brake premise,
witnessed by the budget-binds pin + the intra-tick `class_created`
accumulation law above), and right-sized (the `n_pack` premise,
witnessed by the sim_packs battery + the four-caller footprint
census). A gate regression after retirement is bounded by the
fleet-budget brake, with the per-tick blast radius grown from 8/cell
to `⌊budget/chunk⌋` --- which is why the gate premise carries a PROBING
population witness, never prose. The write-burst axis is PRICED, not
capped: worst-case creates/tick = `⌊remaining-budget/min-chunk⌋`
(\~157/class at the grounded anchor chunk), absorbed by the
kube-client QPS posture and Karpenter's CreateFleet batching +
cloud-provider backoff (sla-sizing.typ carries the same pricing). The
per-Cell round-robin start rotation, not the cap, is what prevents
early-cell budget capture --- re-verified unchanged.

#r("ctrl.nodeclaim.placement-outcome+1")[
  Every nodeclaim-plane intent disposition MUST be a letter of ONE
  total typed alphabet (`PlacementOutcome`: placed, lead-time-gated,
  unplaceable-all-masked, no-hosting-class, over-cap, unknown-universe,
  decode-refused) — minted at the cell-assignment chokepoint
  (`assign_to_cells`, the only point that knows whether `A_open` was
  non-empty before ICE-masking and whether the wire pair decoded
  cleanly), with the sizing partition's over-cap drop RE-CLASSIFIED
  onto the same alphabet (a post-chokepoint terminal drop is a
  compile-time variant, never a shadow disposition). The walk REALIZES
  placements against the controller's mintable universe ("placement
  supersets provisioning", enforced where the walk happens): the
  cheapest pick prefers controller-known candidates, and demand whose
  every unmasked candidate is unknown takes the typed
  `UnknownUniverse` disclosure (`unknown_hw_class` reason series;
  self-heals at the next `GetHwClassConfig` refresh) — never a silent
  `Placed` the cover loop strands. An intent whose every hosting cell is
  ICE-masked MUST surface as a counted, operator-visible outcome
  (`ready_all_cells_ice_masked` tally + WARN naming the intents and
  their hosting classes + the `intent_dropped_total` reason series) —
  never a silent drop; the lead-time-gated arm stays quiet (the next
  tick re-evaluates) and is reachable ONLY by forecast intents
  (decode losses are their own LOUD letter — `DecodeRefused`,
  `cells_decode_refused_total` — so a ready intent can never launder
  into the forecast-quiet arm). The wire-mapped set is EXACTLY
  {`NoHostingClass`, `OverCap`}: a `NoHostingClass` outcome MUST be
  answered to the scheduler as a typed per-intent verdict
  (`AckSpawnedIntentsRequest.rejected`: intent id + closed reason +
  operator-actionable detail naming the configured classes; the
  scheduler consumes it to its terminal poison budget); an `OverCap`
  outcome MUST be answered with the DISTINCT `OVER_CAP` reason whose
  consumer semantics are ADVISORY — the scheduler MUST NOT step any
  terminal budget on it (the lane is version-skew transient, ≤300s;
  conflating it onto `NO_HOSTING_CLASS` would poison self-healing
  drvs at exactly the skew threshold). The masked, unknown-universe, and
  decode-refused outcomes stay OFF the wire, since their masks are
  already the scheduler's own evidence and the universe/decode faults
  are controller-side.
]
Rationale: live_050(a) measured 208 ready intents starving silently —
the pre-fix fold asserted "non-empty `hw_class_names` + empty `A_open`
⇔ lead-time-gated", a false equivalence once masking empties a
non-empty `A_open`; live_051(c) measured the `no_hosting_class` arm as
drop-tally-only — the scheduler never learned, and the affected drvs
looped Ready forever until operators cancelled the builds. The verdict
budget and poison consumption are scheduler-side
(`scheduler.sla.ceiling.stale-solve-revalidation`).

#r("ctrl.nodeclaim.capacity-ladder")[
  A hw-class's capacity-degradation ladder MUST be declared as typed
  config — rung-sibling class references over the (capacity-type ×
  instance-generation) product, every rung a declared hw-class row —
  and the declared rungs MUST derive the class's hosting closure: an
  intent solved into a ladder'd class carries the rungs'
  hosting-valid `(class × capacity)` cells in `hw_class_names`,
  membership independent of the cost deadband. Unfulfillable evidence
  (ICE marks, the #rref("ctrl.nodeclaim.ice-mark-clear") plane) MUST
  advance the walk to a different unmasked rung, and hard starvation
  MUST be impossible while any rung has LAUNCHABLE capacity — every
  rung masked except the last places on the last; all rungs masked
  surfaces as the counted `UnplaceableAllMasked` outcome
  (#rref("ctrl.nodeclaim.placement-outcome+1")), never a silent hang.
  "Has capacity" means launchable at the class's derived ceiling
  (the `scheduler.sla.ceiling.catalog-derived` launchability law) and
  revalidated at emission (the
  `scheduler.sla.ceiling.stale-solve-revalidation` law), never bare
  API existence.
]
Rationale: the §5-S graceful-degradation directive ("the system
shouldn't hang just because there isn't spot capacity … degrade
gracefully to using gen7 and so on"), typed. live_050's hi band was a
single-rung supply universe on the generation axis — gen-8-only
classes, so the 512/704 hang had no rung to advance to. The ladder is
MEMBERSHIP authority only (the recorded option-(a) form): the realized
walk ORDER is derived from cost — capacity-major (the controller's
`cell_rank`: spot in `[0, 0.5)` before od in `[1, 1.5)`), then the
name-hash disambiguator within a band, because the scheduler's seed
prices are capacity-type-only (`seed_price`, sla/cost.rs) and
`cell_rank` is generation-blind — a declared-order quantifier would be
unwitnessed (no machinery executes it).
// values-bound[infra/helm/rio-build/values.yaml: scheduler.sla.hwClasses
// *.capacityTypes — this paragraph NARRATES committed chart state and
// MUST be re-derived on ANY capacityTypes change (the bughunt-9 R23
// supersession-qualifier form, bug_048: an earlier od-only narration
// survived TWO same-wave posture flips because nothing bound it).]
Under the SHIPPED posture — every class, metal included, declares
`[spot, on-demand]` (the spot+od doctrine; its last divergence, the
metal od-only carve-out, died with the owner-signed M1 verdict) — the
realized walk for a laddered pair is capacity-major:
gen8:spot → gen7:spot → gen8:od → gen7:od; capacity-major and
generation-major do NOT coincide (the od band is the cross-generation
failover tier, entered only when the whole spot plane carries ICE
evidence). The within-band order is tiebreak-determined and re-derives
per name. Per-rung price posture: od rungs are a PRICED availability
trade, signed by the directive; metal's spot rungs additionally carry
the M1 priced residual (interruption mid-VM-test wastes the build;
relaunch latency worst-in-fleet — recorded in the values doctrine
block, signed). Lead-time seeds exist for every class×capacity cell —
the helm/18 seed-coverage lint structurally refuses an unseeded cell,
so a capacity-axis extension cannot ship seedless (the M1 flip landed
WITH its `metal-*:spot` rows; helm/43 pins the doctrine).
Rung-advance latency ≤ one IceBackoff step + one tick (the backoff
ladder's own consts, sla/cost.rs cite-only).

#r("ctrl.nodeclaim.placeable-gate+5")[
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
  is pass-through). Producer-side guarantee: the published set MUST come from
  the producer's last successful FFD tick over `Registered=True` claims ---
  it is not republished on a failed-poll tick or in consolidate-only mode,
  and on lease loss the producer MUST unarm the gate (publish nothing) before
  the next consumer tick, so an ex-leader's stale set never drives spawn or
  reap against the new leader's Jobs. Consumer-side postures by
  configuration: the unarmed-gate fail-closed sentence above applies to
  Builder pools; when the NodeClaim CRD is absent the spawn side passes
  through unfiltered (fail-open) but the excess-pending reap stays
  fail-closed --- an ungated `queued` count is not authoritative against the
  post-completion Job-status lag; and a Fetcher pool's excess reap is keyed
  only on scheduler reachability when the CRD is present, since its `queued`
  is the raw scheduler count and needs no FFD gate to be authoritative.
]

#r("ctrl.nodeclaim.lease-edge-polarity+4")[
  Every cross-tick in-memory field of the NodeClaim-pool reconciler is
  classified by its stale-state polarity, and its clear-or-keep MUST sit on
  the matching lease edge. The classes and the per-field classification:
  *suppress* (a stale entry suppresses a later observation or signal) ---
  split by recoverability: `recorded_boot` is cleared in the reload `Ok`
  arm only (its clear re-arms edge DETECTION against the freshly reloaded
  sketches, and a wrongly-cleared entry re-creates itself on the next
  observation, bounded by the `observe_registered` recency gate), while
  the two latched evidence buffers --- `inflight_created` and
  `pending_evidence` (a stale buffered ICE-clear from a previous tenure
  could mask a genuinely ICE'd cell) --- are cleared on the ACQUISITION
  EDGE, exactly once per tenure and never in the reload `Ok` arm: their
  producers are consume-once, so an `Ok`-arm clear destroys current-tenure
  evidence accumulated during a reload-`Err` retry window (degraded ticks
  legitimately fill both buffers), which is unrecoverable; *amplify* (a
  stale entry amplifies a
  destructive action) --- `prev_idle`, cleared unconditionally on the
  lease-acquire edge BEFORE the PG reload attempt, so even a failed reload
  cannot leave a pre-acquire idle timestamp in place and the idle basis is
  never earlier than the current tenure's first idle observation;
  *cleanup-pending* (a stale entry owes exactly one trailing cleanup write)
  --- `prev_extra_cells` and `prev_unplaced_extras`, never cleared on
  acquire; *reload-latch* --- `sketches`, reloaded from PG on acquire with
  the latch cleared only on a successful load and `persist()` gated off
  while the reload is pending, so a stale in-memory copy cannot overwrite
  the previous leader's rows; *retain-safe* (a stale entry only biases
  toward the conservative or degraded action and is deliberately kept
  across BOTH edges) --- `consecutive_bot_ticks` (a re-acquiring ex-leader
  carrying a high streak enters consolidate-only on its first post-acquire
  ⊥ tick: strictly more observation, no growth) and the wedge tracker
  (expiry evidence is event-shaped and ages out of its window; a fresh
  process under-detects for at most one window, the same safe direction as
  the detector it succeeded). Pure cursors with no stale-state semantics
  (`tick_counter`) are exempt from classification. On the lease-loss edge
  the reconciler MUST unarm the placeable gate
  (#rref("ctrl.nodeclaim.placeable-gate")) and, while not leader, MUST take
  no create, delete, ack, or publish effect. Any new cross-tick field MUST
  be classified into one of these classes and its clear (or deliberate
  not-clear) placed on the matching edge.
]

The polarity classes are the distilled lesson of the lease-edge fix history:
a suppress-class field left stale costs one lost observation or one spurious
ICE mask; an amplify-class field left stale deletes a healthy node; a
cleanup-class field cleared too eagerly orphans a paging gauge series at its
last value; a stale sketch persisted over the previous leader's rows resets
fleet-wide learning.

#r("ctrl.nodeclaim.inflight-conservation+3")[
  `inflight_created` tracks every NodeClaim `cover_deficit` created until it
  is observed `Registered`, observed terminating, deleted by this
  controller, or detected vanished --- and each tracked claim MUST resolve
  to exactly one of those outcomes within its tenure. Its mutators are
  exactly: extending with
  the names created this tick; clearing on the ACQUISITION EDGE, exactly
  once per tenure (never in the reload `Ok` arm: an `Ok`-arm clear
  destroys tracking for claims created during the reload-`Err` retry
  window, silently cancelling their vanish detection; previous-tenure
  entries are dropped at the edge because an interim leader's deliberate
  reaps are indistinguishable from Karpenter GC standby-side --- a
  retained stale entry would emit a spurious ICE mark on a healthy
  cell); `detect_vanished`'s
  retain rules (drop registered/terminating/absent, KEEP still-in-flight),
  which MUST run on consolidate-only ticks as well as full ticks; and
  removal of the names this controller itself reaped, which MUST happen
  BEFORE `detect_vanished` scans in both modes so the controller's own
  deletes are never misread as Karpenter GC (a spurious ICE mark on a
  healthy cell). A code path that deletes or forgets a tracked claim without
  updating the map violates this rule.
]

#r("ctrl.nodeclaim.evidence-ack-latch+3")[
  EVERY scheduler-evidence plane carried by `AckSpawnedIntents` —
  registered-cell ICE-clears, observed instance types, AND ICE marks
  (`unfulfillable_cells`) — MUST be delivered commit-on-Ack: the
  request is built FROM the accumulated buffer (no evidence may reach
  the request except through it), shipped BY READ, and the buffer's
  planes are cleared ONLY when the RPC returns success (the epoch
  mint survives the clear). An Ack failure or a mid-tick abort MUST
  leave every plane intact for the next tick (no moved-out value may
  exist between the read and the commit), and a tick that cannot
  deliver at all (scheduler unreachable, consolidate-only) MUST
  buffer its produced evidence rather than drop it — the producers
  are consume-once, so a dropped plane is unrecoverable.
  Buffered-but-unacked ICE marks MUST mask their cells from the same
  controller's `cover_deficit` until acknowledged (a retained
  buffered clear never unmasks local cover). The scheduler MUST
  answer the Ack only after the leader-gated apply — ack means
  APPLIED UNDER LEADERSHIP, never enqueued — and an erring Ack MUST
  imply that NO plane landed (validate-then-commit,
  #rref("sched.sla.ack-validate-then-commit")): a deposed drain, a
  closed cost-table gate, or an undecodable plane entry MUST err the
  RPC with nothing applied so the buffer is retained and redelivered
  whole. The buffer MUST hold per-cell ORDERED evidence with
  producer-minted per-cell-monotonic epochs serialized onto the wire
  (`"h:cap@epoch"`): a newer clear supersedes a buffered mark (the
  cell provably delivered capacity after the failure), and a newer
  mark RETAINS an older buffered clear as a clear-then-mark pair —
  the request then carries that cell in BOTH planes with
  clear-epoch < mark-epoch, and the scheduler MUST apply clears
  before marks so the chronology lands as reset-then-step-0.
  Redelivery after a successful-but-unobserved Ack MUST be a no-op by
  construction: the scheduler applies a cell event iff its epoch is
  strictly greater than the highest epoch applied for that cell
  (redelivery and reorder are total no-ops answered Ok), and
  epoch-less entries keep the pre-epoch semantics as a
  decode-totality lane (refresh-not-step while masked; observed-type
  upserts remain idempotent).
]

The lease-handoff residual is an accepted posture, documented at the
mint: a clock-behind successor controller no-ops fresh events until
its clock passes the prior leader's last mint — symmetric in kind and
magnitude with the scheduler-side handoff posture (in-memory ladder +
gate state, lease-holder only).

#r("ctrl.nodeclaim.ice-mark-clear+5")[
  ICE mark and clear signals sent via `AckSpawnedIntents` MUST be sound:
  `unfulfillable_cells` (marks) are deduplicated to at most one entry per
  cell per tick (the scheduler's backoff ladder climbs once per DISTINCT
  post-expiry failure --- a redelivered mark no-ops via the epoch gate,
  and an epoch-less duplicate refreshes the masked window at the same
  rung, #rref("sched.sla.hw-class.ice-mask")), a request carries one cell
  in both planes ONLY as the ordered clear-then-mark pair with
  clear-epoch < mark-epoch (per-cell ordered evidence at the buffer,
  #rref("ctrl.nodeclaim.evidence-ack-latch") --- any other both-planes
  shape is forbidden), and a mark is emitted only for a
  cell whose claim launch-failed, timed out unregistered, or vanished to
  Karpenter GC --- never for a claim this controller itself reaped,
  whether the delete completed OR returned an ambiguous non-404 error
  that later proves committed: an errored delete's provenance MUST
  survive across ticks (the delete-attempted tombstone, whose grace is
  denominated in CONSUMER FOLD EXECUTIONS and whose expiry is a typed,
  disclosed disposition --- #rref("ctrl.pool.fold-clock"),
  #rref("ctrl.pool.delete-outcome")) so the claim's subsequent
  terminating/absent observation classifies as this controller's own
  reap and applies the ORIGINAL classification's consequence --- mask
  iff that classification was ICE, counter under the original reason
  --- never vanish-attributed evidence
  (#rref("ctrl.nodeclaim.inflight-conservation")). A never-Registered
  NodeClaim observed terminating or absent MUST produce the same
  unfulfillable evidence as a timed-out launch EXACTLY WHEN its launch
  had not provably succeeded and the teardown is not this controller's
  own (the closed `VanishClass` exit alphabet over presence,
  registration, termination, the `Launched` condition, and delete
  provenance: only a REGISTERED claim's teardown is deliberate ---
  live_050(b): the conflated arm starved the scheduler's IceBackoff
  failover, vanished=101/ice=0, zero od claims); a `Launched=True`
  never-Registered teardown is a BOOT failure (capacity provably
  materialized --- e.g. Karpenter's registration TTL firing before the
  controller's `ice_timeout` on slow cells) and MUST NOT mask --- it
  counts `boot-timeout`, the vanish-path mirror of the `BootTimeout`
  reap's non-mask posture; an absent-without-terminating-observation
  exit stays capacity-side by construction (the ~1s GC that evades the
  terminating window is the launch-failure path; a launched claim's
  teardown rides a multi-tick finalizer). The failover TIME axis
  is the existing IceBackoff ladder (60s -> 120s -> ... <= max-lead-time,
  scheduler-side) and vanish-detection latency is one tick by
  construction (tick-over-tick absence). `registered_cells`
  (clears) are emitted only for `Registered=True` edges that pass the
  recency gate in `observe_registered`: a registration older than the gate
  is recorded without emitting a clear, so a restart or lease acquire ---
  both of which empty the edge-detector state --- MUST NOT mass-clear the
  scheduler's accumulated backoff from old registrations. Cells ICE-marked
  this tick MUST be masked from the same tick's `cover_deficit` (mark before
  cover). The clear-side ladder, the heartbeat clear, and TTL expiry are
  scheduler-side (#rref("sched.sla.hw-class.ice-mask")) and are not restated
  here.
]

#r("ctrl.pool.delete-outcome")[
  Every NodeClaim reap lane (the unhealthy reap AND the idle
  consolidation reap, and any lane added later) MUST translate its
  `Api::delete` result through ONE shared typed total ---
  `DeleteOutcome` with exactly the arms `OkDeleted`, `Committed404`,
  and `AmbiguousErr` --- and MUST discharge every arm: `OkDeleted` and
  `Committed404` apply the reason's IDENTICAL full consequence (the
  reap counter under the lane's own reason, the ICE mask iff the
  reason is ICE, the backing-node wedge-eviction feed, and the lane's
  local samples --- a 404 means Karpenter GC raced the delete, but the
  claim WAS reaped); `AmbiguousErr` MUST stamp a delete-provenance
  tombstone carrying the reason's full consequence packet (reason,
  cell, backing node, lane-local sample data). Every tombstone MUST be
  consumed by an arm applying its reason's full consequence when a
  later observation confirms the commit, or expire as a TYPED,
  DISCLOSED disposition --- never a silent prune; a lane that matches
  the raw kube `Result` instead of the shared total is forbidden (the
  lane census over the reconciler's delete-call sites is the
  enforcement).
]
The pre-type history is two strikes of one shape: `record_reap`'s own
doc records the 404-vs-`Ok` divergence as "the original bug", and
bug_112 re-shipped it at `reap_idle` (404 arm `=> {}`, `Err` arm
warn-only) because the law existed only as per-site match-arm
discipline --- N lanes, N hand-copies. bug_042 is the tombstone half:
the registered-claim tombstones (Dead reaps) had no consumer at all,
so a committed-but-errored Dead reap's wedge eviction never fired and
`reaped_total{reason=dead}` permanently undercounted. The typed total
makes a one-armed lane unwritable (rustc exhaustiveness), and the
consumer census makes an unconsumed tombstone a structural
impossibility rather than a per-site promise. The round-12 instance
(merged_bug_050) closed the per-EXIT face the consumer census did not
quantify: the vanish fold's uniform exit epilogue consumed tombstones
on disconfirmed-only evidence at the RegisteredHandoff exit (the same
tick's fold consults a pre-delete LIST), silently losing a committed
delete's consequence packet; tombstone disposition is now a TYPED
per-exit property --- only the packet-firing exit consumes, every
other exit hands the obligation to the registered-population sweep,
and consumption is freshness-gated on the fold clock (a stamp is
consumable only by a fold whose LIST post-dates it). The wave-11
vanish_class census froze the classification alphabet; the typed
disposition closes per-exit discharge --- the axis that seal never
enumerated.

#r("ctrl.pool.fold-clock")[
  The delete-tombstone grace MUST be denominated in CONSUMER FOLD
  EXECUTIONS --- the executions of the one tombstone/vanish consumer
  chokepoint (the in-flight vanish fold, then the registered-tombstone
  sweep, then the clock advance and the prune, in that order, owned by
  ONE function) --- never in wall ticks; the prune MUST run only after
  the consult, at every call site, by construction; and a tombstone
  MUST never be pruned unconsulted. Wall ticks that skip the fold
  (pre-threshold scheduler-unreachable ticks, failed-LIST ticks)
  therefore lengthen real grace instead of consuming it.
]
The wall-clock law this replaces (bug_043, R29) aged
`TOMBSTONE_TTL_TICKS` on the unconditionally-incremented tick counter
while the consuming fold was skipped on pre-threshold ⊥ ticks and
kube-LIST-failure ticks, and `prune_expired` ran BEFORE
`detect_vanished` at both call sites: a tombstone stamped just before
a ≥3-tick foldless window was dropped before the first fold ever
consulted it, and `classify_vanish(None, None)` minted `GcVanish`
(false ICE mask + `reaped_total{reason=vanished}`) for the
controller's own BootTimeout self-reap --- the correlated
apiserver-disruption path (ambiguous delete error + failed LISTs in
one outage) makes exactly that sequence realistic. Every quantitative
envelope names its clock; this one's is the consuming fold's own
execution count.

#r("ctrl.nodeclaim.consolidate-only-degraded+3")[
  After `BOT_TICKS_BEFORE_CONSOLIDATE_ONLY` (5) consecutive failed
  `GetSpawnIntents` polls the NodeClaim-pool reconciler MUST run in
  consolidate-only mode until a poll succeeds. A consolidate-only tick MAY
  list NodeClaims, record kube-only observations, reap idle and unhealthy
  claims, prune `inflight_created`, and persist sketches (subject to the
  reload latch); it MUST NOT create NodeClaims, MUST NOT republish the
  placeable set (the consumer's own failed poll keeps it fail-closed,
  #rref("ctrl.nodeclaim.placeable-gate")), and MUST NOT send ICE marks
  (locally detected ICE cells are dropped, not queued). Idle reaping
  in this mode treats the placeable set as empty --- no FFD reservation is
  honored during the outage. During a `GetSpawnIntents` outage EVERY
  leader tick --- failed-poll ticks before the threshold and
  consolidate-only ticks after it --- MUST run the kube-only observation
  block (idle→busy pruning and Registered-edge recording); a
  pre-threshold failed-poll tick MUST take no effect beyond those
  observations (no create, reap, ack, or publish). Scheduler-bound
  observation outputs produced during the outage (`registered_cells`
  ICE-clears, observed instance types) are BUFFERED, not discarded
  (#rref("ctrl.nodeclaim.evidence-buffered")).
]

#r("ctrl.nodeclaim.evidence-buffered")[
  Scheduler-bound evidence produced by the kube-only observation block on a
  tick that cannot ship it (a pre-threshold failed-poll tick or a
  consolidate-only tick) MUST be buffered and drained into the next
  successful tick's `AckSpawnedIntents` --- never discarded. The producing
  edges are consume-once (`recorded_boot` marks the node so the Registered
  edge never re-fires), so a discard loses the cell's ICE-clear and the
  observed instance type permanently. The buffer is cleared on the
  lease-acquire edge (suppress polarity:
  #rref("ctrl.nodeclaim.lease-edge-polarity")).
]
The buffer also closes the pre-existing ≥5-tick loss: observed instance
types recorded across a long consolidate-only stretch now reach the
scheduler's cost table when the outage ends.

#r("ctrl.nodeclaim.acquire-edge-token+1")[
  Lease-acquire edge state MUST be tracked as a monotone acquisition epoch
  (incremented by `on_acquire`), with the reconciler holding two cursors:
  `edge_seen_epoch` (edge actions --- the amplify-class `prev_idle` clear
  and the `pending_evidence` and `inflight_created` resets --- fire
  exactly once per acquisition,
  never once per retry tick) and `reloaded_epoch` (advanced only on a
  successful PG sketch reload; `persist()` is gated while it lags ---
  latch-on-Ok-only, unchanged). A re-acquisition during a reload-error loop
  is a NEW epoch and re-fires the edge exactly once. A boolean latch is
  not an acceptable encoding: re-reading it every tick re-executes the
  edge actions for the whole outage (bug_346 --- every idle clock
  restarted every tick, so idle consolidation never fired).
]

#r("ctrl.nodeclaim.wedge-cluster+3")[
  On every full reconcile tick the NodeClaim-pool reconciler MUST compute a
  per-node clustering of pull-mode attempt-deadline expiries from the
  open-attempt ledger view (`AdminService.ListOpenAttempts`): an open
  build-class attempt whose age exceeds its known intent deadline by the
  observation grace contributes its derivation as evidence against the
  ledger's `source_node` ONLY --- an attempt with no ledger node attribution,
  no known deadline, or a non-build work class contributes nothing (a
  materialization attempt is a store-side fetch whose stamped binding is the
  stale builder pod). Evidence admission MUST treat eviction as an admission
  source, not a state wipe: a node the controller reaped (any reap path,
  idle included) or that is absent from the registered NodeClaim fleet is
  tombstoned for one window --- its still-open attempts are inadmissible (a
  reaped node's attempts re-present for several ticks before the
  establishment sweep closes them, and re-anchoring them re-feeds the `Dead`
  arm with a node that no longer exists). Each (node, derivation) evidence
  entry MUST anchor its window at the derivation's FIRST observation --- a
  stuck-open attempt re-observed every tick does not slide the window. A
  node accumulating evidence for at least 2 distinct derivations inside the
  30-minute window MUST be treated as Dead-equivalent --- SUBJECT to the
  trajectory gates of `ctrl.nodeclaim.wedge-two-axis` (breadth and
  post-episode dwell MAY suppress the per-node verdict) --- and fed to the
  unhealthy reap's `Dead` arm
  --- the sole `Dead` input since the 1d sweep removed `GetSpawnIntents.dead_nodes`
  --- under the same per-tick dead-reap cap. One
  derivation expiring repeatedly MUST NOT mark a node by itself; an
  open-attempt RPC failure MUST only skip that tick's observation (previously
  accumulated evidence is retained, and no node is marked from data the
  controller did not observe).
]

#r("ctrl.nodeclaim.wedge-two-axis+6")[
  The wedge clustering's verdict MUST be trajectory-gated over
  COMMENSURABLE, FLEET-DERIVED populations: the denominator is the
  registered NodeClaim fleet united with the evidence-bearing nodes ---
  never the per-tick attributed view, whose traffic-lull collapse minted
  false systemic verdicts --- so `affected <= of` holds by construction
  and an idle fleet cannot shrink the denominator to the expiring nodes
  themselves. Three suppression axes gate every per-node verdict, in
  precedence order: (1) RATIO --- when more than half of the population
  is past the cluster threshold and at least two nodes are affected, the
  verdict is SYSTEMIC: the reconciler MUST mark no node, MUST
  drain the WHOLE episode's window evidence (every node in the windowed
  population, not only the wedged subset --- a sub-threshold
  participant's episode anchor must not survive as half of a future
  pair), MUST latch the suppression watermark, and MUST re-derive the
  marked set; (2) BREADTH --- when more than half of the population
  bears at least one in-window expiry (at least two nodes), per-node
  verdicts MUST be suppressed --- WITHOUT draining or latching while
  the episode stays ENGAGED (staggered shared-cause onset: the
  evidence keeps accumulating toward the ratio law --- serial per-node
  Dead-reaps before the ratio trips are the failure mode this axis
  removes), the marked transition-memory retained with it (a
  suppressed tick that retains evidence MUST NOT re-derive the marked
  set --- draining it re-counts one continuous wedge as a fresh
  transition); a breadth episode that ends WITHOUT the ratio law
  firing MUST close through the same drain + merge-latch + dwell
  chokepoint at its release edge --- the merge-latch never lowers the
  watermark, and evidence observed during an engaged episode MUST be
  unable to mint a per-node verdict after release (the late-onset
  node re-detects only from fresh post-watermark expiries after the
  dwell, the same re-entry law as a ratio close);
  (3) DWELL --- for `WEDGE_VERDICT_DWELL_SECS` after a suppression
  watermark latches, per-node verdicts MUST remain suppressed (an
  episode's trailing edge is not a sequence of fresh per-node wedges);
  a dwell release is a no-op (draining at dwell expiry would destroy
  legitimate fresh post-watermark evidence). An axis CHANGE on an
  engaged episode is itself an edge: the outgoing axis's release
  obligations MUST run at the transition, before the incoming axis's
  engaged-tick effects --- in particular a breadth episode that
  downgrades to the dwell arm MUST close (drain + merge-latch + dwell
  measured from the transition) at the downgrade; no edge out of an
  engaged breadth phase may skip its close, and the transition law
  MUST be total over the axis product. Every verdict runs its
  full epilogue through one sealed exit whose token is constructible
  only inside that exit, and every suppressed tick MUST increment the
  suppression counter
  (#(refs.metric)("rio_controller_wedge_systemic_suppressed_total"))
  labeled by the engaging axis (ratio | breadth | dwell, highest
  precedence labels the tick). Evidence admission MUST
  beat the watermark in a SINGLE clock frame: an expired attempt
  contributes only when its ledger-frame expiry instant
  (`assigned_at + deadline + grace`, from `assigned_at_epoch_secs`) is
  strictly newer than the drained episode's newest ledger-frame expiry
  --- never a controller-frame reconstruction, whose per-tick jitter
  flipped admission at the boundary --- and the watermark itself expires
  after one window (a protective latch cannot outlive the episode it
  suppresses: an episode attempt that never closes is exactly the
  signature of a still-wedged node, which MUST re-detect after the
  window). Only a per-node verdict may feed the `Dead` arm. A tick whose
  open-attempt view RPC failed MUST produce the distinct UNOBSERVED
  verdict: retained evidence neither marks nor suppresses, and the
  marked set is not re-derived (an observation blip must not
  double-count later transitions). Backing nodes the controller reaped
  MUST be evicted from the window before the next verdict (reap
  feedback is a required input, not optional). The wedge observation
  grace plus two reconcile ticks MUST fit inside the scheduler's
  establishment report slack, enforced from one shared constant on both
  sides (controller compile-time, scheduler config-load).
]

// SIGNED 2026-06-08 (owner, bughunt-4 fix-wave, §5-S Q2 --- recorded at
// both anchors per §1.5-2; the twin block sits at the systemic-guard
// doc in nodeclaim_pool/wedge.rs): merged_bug_034's systemic guard is
// trajectory-aware over the fleet-derived denominator. Denominator =
// registered NodeClaim fleet (never the per-tick attributed survivor
// set); breadth and dwell axes gate every Dead-reap; the model twin
// demonstrates BOTH retired failure modes (serial reap under staggered
// onset; false-systemic under a traffic lull, made sticky by the
// episode drain). No retroactive repair for nodes Dead-reaped under
// the retired instantaneous law --- disclosed at signing. This
// signature supersedes the +3 body's attributed-fleet denominator and
// its instantaneous-snapshot guard; the +3->+4 bump and the
// wedge-cluster+2->+3 bump (admission-source eviction, dwell-gated
// marking) ride the same commit with all markers re-stamped.
//
// 2026-06-09 (bughunt-5 fix-wave, riding the same Q2 signature): the
// +4->+5 bump extends the trajectory law with the release-edge close
// (a breadth episode that ends without the ratio law firing closes
// through the same drain + merge-latch + dwell chokepoint --
// merged_bug_023's silent third exit removed), the suppressed-tick
// marked-retention (merged_bug_016), and the per-axis suppression
// counter law (every suppressed tick increments, labeled ratio |
// breadth | dwell -- SIGNED S1-OQ2: per-tick + axis). Same
// fleet-derived denominator, same dwell direction; no behavioral
// change to the ratio axis.


This is the OA2 successor to the retired heartbeat-fed scheduler-side
hung-node detector: pull-mode pods
never register or heartbeat, so a wedged-but-`Ready` node (EBS stall, kernel
softlockup, D-state runtime) is visible only as its builds running out their
attempt deadlines without any report. The clustering reads only ledger facts
plus the spawn-ack node binding, so it survived the session-machinery
deletion and is now the only node-wedge signal: the scheduler-side detector
is gone and `GetSpawnIntents.dead_nodes` is removed outright (field 3 is
reserved in the proto; the union arm went with it in the 1d sweep). The
#(refs.metric)("rio_controller_node_wedge_marked_total") counter records each
not-wedged→wedged transition and
#(refs.metric)("rio_controller_wedge_systemic_suppressed_total") each tick the
systemic guard refused to mark; the
#(refs.alert)("RioSchedulerAttemptEstablishmentCluster")
alert and the manual-reap runbook remain the independent operator-facing
tripwire and confirmation procedure --- the automation now applies the
runbook's systemic-vs-per-node discrimination itself before feeding the Dead
arm.

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

The FUSE cache emptyDir `sizeLimit` is single-sourced from a per-*kind*
boot-time value --- `[nodeclaim_pool].fuse_cache_bytes` for Builder pools (helm
`poolDefaults.fuseCacheBytes`, 50Gi in prod) and
`[nodeclaim_pool].fetcher_fuse_cache_bytes` for Fetcher pools (helm
`poolDefaults.fetcherFuseCacheBytes`, 4Gi) --- so FFD/cover/stamp agree.
`PoolSpec.fuseCacheBytes` is rejected for both Builder and Fetcher kind.
(Pre-§13e, Fetcher pools could set a per-pool value because they didn't route
through `nodeclaim_pool`; §13e routed them through, and r35 closed the
resulting accounting drift by collapsing both kinds onto the builder value.
The per-kind split keeps the single-sourcing while stopping fetcher pods from
reserving the builder's input-closure budget: a FOD's input closure is a fetch
script's runtime deps, not an arbitrary build closure, and the builder budget
dominated the fetcher pod's ephemeral-storage request ~30#sym.times over what
the pod could ever touch.) The same value is added to the container's
`ephemeral-storage` request/limit so the two cannot drift. Pods are one-shot so
the cache never outlives one build's input closure. Unlike the overlay
(`disk_bytes`), the FUSE-cache dimension has no eviction-driven escalation
path, so the fetcher budget must statically cover the heaviest fetch toolchain
in use; raise `fetcherFuseCacheBytes` if one outgrows it.

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
per-derivation #glspl("prefetch-hint") --- the builder prefetches the
assignment payload's input closure into its FUSE cache before the build
starts, so the cache warms during setup rather than on first `read()`. (The
scheduler-side cold-start warm-gate retired with the stream placement
layer; prefetch is builder-local for pull-mode pods, where every build
cold-starts --- #rref("ctrl.pool.ephemeral").)
