# Plan 435: WPS per-class resource requests derived from build_history EMA

The `TODO(P0435)` at [`infra/helm/rio-build/values.yaml:552`](../../infra/helm/rio-build/values.yaml) flags the phase-4b placeholder: per-class worker pod `resources.requests` (cpu/memory) are hand-picked constants (`cpu: "2"`, `memory: "4Gi"`). This works for a single unclassified pool, but once size-classes are live ([P0232](plan-0232-wps-crd-struct-crdgen.md)–[P0235](plan-0235-wps-main-wire-rbac-crd-regen.md)), each class's pod should request what its builds actually use — not a one-size-fits-all guess.

The data already exists. `build_history` carries `ema_peak_memory_bytes` and `ema_peak_cpu_cores` per `(pname, system)` pair (see [`rio-scheduler/src/db/history.rs:28`](../../rio-scheduler/src/db/history.rs)). The scheduler's size-class router ([P0229](plan-0229-cutoff-rebalancer-gauge-convergence.md), `r[sched.classify.smallest-covering]`) already aggregates these EMAs to route builds into classes. What's missing is the reverse flow: per-class EMA percentile → `ResourceRequirements` → WorkerPool spec.

The controller's WPS reconciler ([`rio-controller/src/reconcilers/workerpoolset/builders.rs:117`](../../rio-controller/src/reconcilers/workerpoolset/builders.rs)) currently copies `class.resources` verbatim from the CRD spec. This plan adds an optional `resourcesFrom: buildHistory` mode on `SizeClassSpec` that, when set, makes the reconciler query the scheduler for per-class EMA aggregates and synthesize `requests.cpu`/`requests.memory` from them (with configurable headroom multiplier and floor/ceiling clamps).

This is NOT the same as autoscaling ([P0234](plan-0234-wps-status-perclass-autoscaler-yjoin.md), `r[ctrl.wps.autoscale]`) — that adjusts replica count based on queue depth. This adjusts per-replica resource requests based on historical build resource footprint. The two compose: autoscaling decides HOW MANY workers; this decides HOW BIG each worker's pod request is, so Karpenter provisions appropriately-sized nodes.

## Entry criteria

- [P0235](plan-0235-wps-main-wire-rbac-crd-regen.md) merged (WPS reconciler wired, RBAC in place)
- [P0231](plan-0231-get-sizeclass-status-rpc-hub.md) merged (`GetSizeClassStatus` RPC exists — extend it for EMA aggregates)

## Tasks

### T1 — `feat(proto):` extend GetSizeClassStatus with per-class EMA aggregates

MODIFY [`rio-proto/proto/scheduler.proto`](../../rio-proto/proto/scheduler.proto). Add fields to `SizeClassStatus`:

```proto
message SizeClassStatus {
  // ... existing fields (queued, active, cutoff) ...

  // P50/P90 of build_history.ema_peak_memory_bytes for builds
  // routed into this class. Bytes. 0 if no history.
  uint64 ema_memory_p50_bytes = N;
  uint64 ema_memory_p90_bytes = N+1;

  // P50/P90 of build_history.ema_peak_cpu_cores. 0.0 if no history.
  double ema_cpu_p50_cores = N+2;
  double ema_cpu_p90_cores = N+3;
}
```

MODIFY [`rio-scheduler/src/grpc/admin.rs`](../../rio-scheduler/src/grpc/admin.rs) — `get_size_class_status` handler populates the new fields. The per-class aggregation query JOINs `build_history` against the class-routing cutoffs (re-use the `classify()` predicate from [`rio-scheduler/src/estimator.rs`](../../rio-scheduler/src/estimator.rs)). Percentile via `percentile_cont(0.5/0.9) WITHIN GROUP (ORDER BY ema_peak_memory_bytes)`.

### T2 — `feat(crds):` SizeClassSpec.resourcesFrom mode + headroom config

MODIFY [`rio-crds/src/workerpoolset.rs`](../../rio-crds/src/workerpoolset.rs). Add alongside the existing `resources: ResourceRequirements` field:

```rust
/// How to determine pod resource requests for this class's WorkerPool.
///
/// - `Static` (default): use `resources` verbatim.
/// - `BuildHistory`: derive from scheduler's per-class EMA aggregates.
///   `resources` becomes the FLOOR (requests never go below it).
#[serde(default)]
pub resources_from: ResourcesFrom,

/// Headroom multiplier applied to EMA P90 when
/// `resources_from = BuildHistory`. Default 1.2 (20% over P90).
/// Clamped to [1.0, 3.0].
#[serde(default = "default_headroom")]
pub resources_headroom: f64,

#[derive(..., Default)]
pub enum ResourcesFrom {
    #[default]
    Static,
    BuildHistory,
}
```

CEL validation: `resources_headroom` in `[1.0, 3.0]`. Regen CRD YAML via `just crds`.

### T3 — `feat(controller):` WPS builder derives requests when resourcesFrom=BuildHistory

MODIFY [`rio-controller/src/reconcilers/workerpoolset/builders.rs`](../../rio-controller/src/reconcilers/workerpoolset/builders.rs). The `build_worker_pool` path branches on `class.resources_from`:

- `Static` → current behavior (`Some(class.resources.clone())`)
- `BuildHistory` → call `scheduler_client.get_size_class_status()`, compute:
  ```rust
  let mem_bytes = (status.ema_memory_p90_bytes as f64 * class.resources_headroom)
      .max(floor_from(&class.resources, "memory"));
  let cpu_cores = (status.ema_cpu_p90_cores * class.resources_headroom)
      .max(floor_from(&class.resources, "cpu"));
  ```
  Round memory up to nearest 256Mi, cpu to nearest 0.25 core (avoid churn on EMA noise). Synthesize `ResourceRequirements { requests: {cpu, memory}, limits: None }`.

If the scheduler call fails (unavailable, timeout), fall back to `Static` behavior and emit a `ResourcesFromFallback` k8s Event — reconcile will retry on next tick.

### T4 — `feat(helm):` values.yaml gains resourcesFrom knob, TODO closed

MODIFY [`infra/helm/rio-build/values.yaml`](../../infra/helm/rio-build/values.yaml). Replace the `TODO(P0435)` block with:

```yaml
# resourcesFrom: Static | BuildHistory
#   Static (default): requests below are used verbatim.
#   BuildHistory: controller derives requests from scheduler's
#     per-class EMA P90 × resourcesHeadroom, floored at the
#     values below. Requires scheduler size_classes configured.
resourcesFrom: Static
resourcesHeadroom: 1.2
resources:
  requests:
    cpu: "2"          # floor when resourcesFrom=BuildHistory
    memory: "4Gi"
    ephemeral-storage: "25Gi"
```

MODIFY the WPS helm template to pass `resourcesFrom`/`resourcesHeadroom` through to the CRD.

### T5 — `test(controller):` builders derive requests from mocked GetSizeClassStatus

NEW test in [`rio-controller/src/reconcilers/workerpoolset/builders.rs`](../../rio-controller/src/reconcilers/workerpoolset/builders.rs) `mod tests`:

```rust
#[tokio::test]
async fn resources_from_build_history_applies_headroom_and_floor() {
    let mock_sched = MockScheduler::with_size_class_status(SizeClassStatus {
        ema_memory_p90_bytes: 3 * GI,  // 3Gi
        ema_cpu_p90_cores: 1.5,
        ..Default::default()
    });
    let class = SizeClassSpec {
        resources_from: ResourcesFrom::BuildHistory,
        resources_headroom: 1.2,
        resources: floor_2cpu_4gi(),  // floor
        ..test_class()
    };
    let pool = build_worker_pool(&wps, &class, &mock_sched).await.unwrap();
    let req = pool.spec.resources.unwrap().requests.unwrap();
    // 3Gi × 1.2 = 3.6Gi → rounds to 3.75Gi; floor is 4Gi → 4Gi wins
    assert_eq!(req["memory"], Quantity("4Gi".into()));
    // 1.5 × 1.2 = 1.8 → rounds to 2.0; floor is 2 → 2 wins
    assert_eq!(req["cpu"], Quantity("2".into()));
}

#[tokio::test]
async fn resources_from_build_history_scheduler_down_falls_back_static() {
    let mock_sched = MockScheduler::unavailable();
    // ... asserts pool.spec.resources == class.resources verbatim
    // ... asserts ResourcesFromFallback event recorded
}
```

## Exit criteria

- `/nixbuild .#ci` green
- `grep 'TODO(P0435)\|TODO(P0429)' infra/helm/rio-build/values.yaml` → 0 hits (TODO closed)
- `grep 'resourcesFrom\|resources_from' rio-crds/src/workerpoolset.rs` → ≥2 hits (field + enum)
- `grep 'ema_memory_p90\|ema_cpu_p90' rio-proto/proto/scheduler.proto` → ≥2 hits
- `cargo nextest run -p rio-controller resources_from_build_history` → ≥2 passed
- `nix develop -c tracey query rule ctrl.wps.resources-derived` → non-empty (new marker parsed)
- `just crds && git diff --exit-code infra/helm/rio-build/crds/` → idempotent regen

## Tracey

References existing markers:
- `r[ctrl.wps.reconcile]` — T3 extends the reconcile path (builders branch on `resources_from`)
- `r[sched.admin.sizeclass-status]` — T1 extends the RPC response
- `r[sched.classify.smallest-covering]` — T1's aggregation query re-uses the classify predicate

Adds new markers to component specs:
- `r[ctrl.wps.resources-derived]` → `docs/src/components/controller.md` (see ## Spec additions)

## Spec additions

Add to [`docs/src/components/controller.md`](../../docs/src/components/controller.md) after the `r[ctrl.wps.autoscale]` paragraph:

```
r[ctrl.wps.resources-derived]
When `SizeClassSpec.resourcesFrom = BuildHistory`, the WPS reconciler
derives per-class pod `resources.requests` from the scheduler's
`GetSizeClassStatus` EMA aggregates: `requests.memory = ceil_256Mi(
ema_memory_p90_bytes × resourcesHeadroom)` floored at the static
`resources.requests.memory`; same for `cpu` at 0.25-core granularity.
On scheduler unavailability, falls back to `Static` behavior and emits
a `ResourcesFromFallback` Event. This is orthogonal to replica
autoscaling (`r[ctrl.wps.autoscale]`) — derives per-pod SIZE, not
pod COUNT.
```

## Files

```json files
[
  {"path": "rio-proto/proto/scheduler.proto", "action": "MODIFY", "note": "T1: add ema_{memory,cpu}_p{50,90} to SizeClassStatus"},
  {"path": "rio-scheduler/src/grpc/admin.rs", "action": "MODIFY", "note": "T1: populate EMA percentiles in get_size_class_status"},
  {"path": "rio-scheduler/src/db/history.rs", "action": "MODIFY", "note": "T1: per-class percentile_cont query helper"},
  {"path": "rio-crds/src/workerpoolset.rs", "action": "MODIFY", "note": "T2: ResourcesFrom enum + resources_headroom field + CEL"},
  {"path": "rio-controller/src/reconcilers/workerpoolset/builders.rs", "action": "MODIFY", "note": "T3: branch on resources_from; T5: tests"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T4: close TODO, add resourcesFrom/Headroom knobs"},
  {"path": "infra/helm/rio-build/templates/workerpoolset.yaml", "action": "MODIFY", "note": "T4: plumb resourcesFrom through template"},
  {"path": "infra/helm/rio-build/crds/workerpoolset.yaml", "action": "MODIFY", "note": "T2: regen via just crds"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "Spec: add r[ctrl.wps.resources-derived] marker"}
]
```

```
rio-proto/proto/
└── scheduler.proto          # T1: SizeClassStatus +4 EMA fields
rio-scheduler/src/
├── grpc/admin.rs            # T1: populate percentiles
└── db/history.rs            # T1: percentile query
rio-crds/src/
└── workerpoolset.rs         # T2: ResourcesFrom enum
rio-controller/src/reconcilers/workerpoolset/
└── builders.rs              # T3+T5: derive logic + tests
infra/helm/rio-build/
├── values.yaml              # T4: close TODO
├── templates/workerpoolset.yaml
└── crds/workerpoolset.yaml  # T2: regen
docs/src/components/
└── controller.md            # spec marker
```

## Dependencies

```json deps
{"deps": [235, 231], "soft_deps": [234, 229], "note": "Hard on P0235 (WPS reconciler wired) + P0231 (GetSizeClassStatus RPC). Soft on P0234 (autoscaler — composes but independent) + P0229 (classify predicate reused for aggregation query)."}
```

**Depends on:** [P0235](plan-0235-wps-main-wire-rbac-crd-regen.md) — WPS reconciler main-wired with RBAC. [P0231](plan-0231-get-sizeclass-status-rpc-hub.md) — `GetSizeClassStatus` RPC hub (T1 extends it).

**Conflicts with:** `rio-crds/src/workerpoolset.rs` is HOT (P0232-P0235 cluster + trivial-batch T-items touch it). `rio-controller/src/reconcilers/workerpoolset/builders.rs` shared with [P0233](plan-0233-wps-child-builder-reconciler.md) — serialize after.
