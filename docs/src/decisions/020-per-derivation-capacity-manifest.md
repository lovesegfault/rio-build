# ADR-020: Per-Derivation Capacity Manifest

## Status
Accepted (sizing mechanism); amended 2026-04 (pod lifetime)

## Status update (2026-04)

Manifest-spawned pods are now **one-shot** like every other worker pod — they run one build and exit. Design point 4 below ("Pods are long-lived, not one-shot") and the rejected-alternative analysis of "True one-pod-per-derivation" are superseded. The concerns there (cold-start dominating short builds, FUSE cache never warming, Karpenter churn cost) are addressed by composefs (<10ms mount, zero warm upcalls), node-level FSx Lustre cache surviving pod churn, and Karpenter consolidation keeping nodes alive across pods. The per-derivation sizing mechanism (points 1–3, 5) is unchanged. `r[ctrl.pool.manifest-long-lived]` is retired.

## Context

[ADR-015](015-size-class-routing.md) introduced size-class routing: operators pre-define discrete pools (small/medium/large), the scheduler classifies derivations by EMA duration with memory/CPU bump, and routes to the matching pool. It explicitly rejected per-derivation resource requests as "much more complex (requires per-derivation resource prediction, dynamic pod resizing, and tighter scheduler/K8s integration)."

Three things have changed since that decision:

**Per-derivation prediction already exists.** `build_history` tracks `ema_peak_memory_bytes` and `ema_peak_cpu_cores` per `(pname, system)` ([rio-scheduler/src/db/history.rs:65](../../rio-scheduler/src/db/history.rs)). The Estimator reads them every ~60s tick. `classify()` already consumes them for mem-bump/cpu-bump ([rio-scheduler/src/assignment.rs:126](../../rio-scheduler/src/assignment.rs)). The prediction layer ADR-015 called complex is in place --- it just feeds a coarser output (a class name) than it could (a resource request).

**Karpenter makes bins an artificial constraint.** On static node groups, bins match the three instance types you bought. With Karpenter's continuous sizing ([deployment.md:66](../deployment.md)), node provisioning is already per-pod-request --- a pod asking for 47Gi gets a node that fits 47Gi. The bin abstraction adds a quantization step between the scheduler's continuous estimate and Karpenter's continuous provisioning, wasting the difference.

**The scheduler→controller channel is thin by choice, not necessity.** The current signal is `ClusterStatus.queued_derivations` --- a scalar count. A speculative `ControllerService.CreateEphemeralBuilder` push RPC was removed in 2026-03 ([admin.proto:56](../../rio-proto/proto/admin.proto)) because push required the controller to grow a gRPC server. But pull can carry richer data without that cost: the controller already polls `ClusterStatus`; a neighboring RPC returning queue shape instead of queue count needs no new infrastructure.

The current bin architecture conflates two problems ADR-015's Harchol-Balter citation cares about: duration isolation (short jobs shouldn't queue behind long jobs) and resource fit (don't put a 128Gi build on an 8Gi pod). Bins solve both by proxy: classes are ordered by BOTH duration cutoff and memory limit, so routing by class isolates by both. But the two problems are separable. Duration isolation is about the ReadyQueue's dispatch order --- already handled by critical-path priority. Resource fit is about pod sizing --- which this ADR addresses directly.

## Decision

Replace the scalar `queued_derivations` autoscale signal with a per-derivation capacity manifest. The controller polls the scheduler for the resource shape of the ready queue, creates pods sized to that shape, and the scheduler places derivations by resource fit.

Key design choices:

1. **Pull, not push.** Add `AdminService.GetCapacityManifest` returning `repeated DerivationResourceEstimate {est_memory_bytes, est_cpu_cores, est_duration_secs}` for queued-ready derivations. The controller polls this from the existing ephemeral reconciler loop. No new gRPC server, no push semantics, no departure from the poll-driven architecture that motivated removing `ControllerService`.

2. **Scheduler owns rounding.** The manifest carries raw EMA × headroom, already rounded to stable buckets (nearest 4Gi memory, nearest 2 cores, minimum clamped to an operator floor). Rounding at the source means the controller and any future consumers see the same buckets; two derivations that should share a pod don't diverge from floating-point noise applied at different places.

3. **Heterogeneous pod creation via per-size Job batches.** The controller groups the manifest by `(est_memory, est_cpu)`, compares to the live pod inventory, and spawns Jobs for the deficit. This is ephemeral mode ([rio-controller/src/reconcilers/builderpool/ephemeral.rs](../../rio-controller/src/reconcilers/builderpool/ephemeral.rs)) with one change: `build_pod_spec` takes a `ResourceRequirements` argument instead of reading it from `BuilderPool.spec`. A new `BuilderPool.spec.sizing: {Static, Manifest}` enum gates the behavior --- `Static` preserves today's fixed-spec path.

4. **Pods are long-lived, not one-shot.** A manifest-spawned pod does NOT exit after one build. It heartbeats its `memory_total_bytes` (already reported, [rio-builder/src/cgroup.rs:667](../../rio-builder/src/cgroup.rs)), takes any derivation that fits, and idles until either work arrives or a grace period expires. The grace period is the existing `SCALE_DOWN_WINDOW` applied per-pod: the controller's manifest diff sees "48Gi pod idle, no 48Gi demand for 10min" → delete the Job. This preserves FUSE cache warming within a pod's lifetime and amortizes the ~50-80s node provision cost across multiple builds of similar size.

5. **Placement by resource fit, not class match.** `assign_to_worker` replaces its `size_class` string match with `worker.memory_total_bytes >= drv.est_memory_bytes`. Overflow routing is natural: a 16Gi derivation can run on a 64Gi pod if that's what's idle. The transfer-cost and locality scoring ([rio-scheduler/src/assignment.rs:296](../../rio-scheduler/src/assignment.rs)) already prefer the best-fit idle worker; resource fit is a hard filter preceding that scoring, same position as `has_capacity` today.

## Alternatives Considered

- **Keep ADR-015 bins, tune more finely (P0435):** P0435 sizes each class's pod requests from per-class EMA P90. This improves bin placement but keeps the quantization. With Karpenter, the bins are doing work that Karpenter does better at the node level. P0435 is not wasted --- the per-class EMA aggregation query becomes the per-derivation manifest query with the GROUP BY removed.

- **Scheduler creates pods directly:** The scheduler could call the k8s API itself, cutting out the controller middleman. Rejected: the scheduler is leader-elected and holds the in-memory DAG; adding k8s client + RBAC + pod lifecycle to its failure surface complicates failover. The controller is already the k8s-facing component with reconciler machinery and the right ServiceAccount.

- **True one-pod-per-derivation (no reuse):** Every ready derivation gets a fresh Job, exits after one build. Maximally simple, no grace-period bookkeeping. Rejected: ~50-80s cold start dominates short builds (most of nixpkgs), FUSE cache never warms, and Karpenter node churn per-build costs real money on the consolidation cycle.

- **Vertical Pod Autoscaler:** K8s VPA resizes pods based on observed usage. Rejected: VPA is reactive (pod must run undersized first), and k8s pod resize is a disruptive restart anyway. Rio's EMA is predictive --- size right on creation, no resize.

## Consequences

- **Positive:** No manual class tuning. Operators set a memory/CPU floor and headroom multiplier; the system does the rest. No "which class does chromium go in" decisions.
- **Positive:** Karpenter does the bin-packing at node level where it has the most leverage (spot instance selection, consolidation, multi-AZ spread).
- **Positive:** Cold-start problem has a clear answer: first build of a pname uses the floor (32c/64Gi or whatever the operator chose), reports peak on completion, second build gets a fitting pod. Same EMA learning curve as ADR-015, but the output is a pod size instead of a class name.
- **Positive:** ADR-015's SITA-E duration isolation is preserved by construction. Short builds get small pods (provision in ~50s). Long builds get big pods. They're on different pods --- no queue contention. The ReadyQueue's critical-path priority still orders dispatch within a size.
- **Negative:** Pod inventory is more dynamic. `kubectl get pods -n rio-builders` is noisier than a fixed-class set. Mitigation: label Jobs with the rounded size so `kubectl get job -l rio.build/memory-class=48Gi` groups them.

## Relationship to ADR-015

Supersedes ADR-015's sizing mechanism. The SITA-E insight (heavy-tailed distributions benefit from size-based routing) is preserved --- manifest pods ARE size-based routing, just with continuous sizes instead of discrete classes. The `BuilderPoolSet` CRD, `classify()` function, and `size_class` string matching become legacy under `sizing: Manifest`; they remain functional under `sizing: Static` for operators who prefer explicit bins.

## Implementation Status

Proposed. No code yet. Breaking into phases:

- `GetCapacityManifest` RPC + scheduler-side queue-shape aggregation
- `BuilderPool.spec.sizing` enum (Manifest mode is one build per pod by construction — P0537 made that universal)
- Controller manifest-diff reconciler (ephemeral.rs variant with per-size spawn)
- Scheduler resource-fit placement filter
- Per-pod idle grace period + scale-down
