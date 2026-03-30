# Plan 504: ADR-020 phase 4 — scheduler resource-fit placement filter

[ADR-020](../../docs/src/decisions/020-per-derivation-capacity-manifest.md) § Decision ¶5: `assign_to_worker` replaces its `size_class` string match with `worker.memory_total_bytes >= drv.est_memory_bytes`. Overflow routing is natural — a 16Gi derivation can run on a 64Gi pod if that's what's idle.

The filter site is [`rio-scheduler/src/assignment.rs:205`](../../rio-scheduler/src/assignment.rs) — currently a `match (target_class, w.size_class.as_deref())` string equality. It sits alongside `has_capacity()` (:189) as a hard filter preceding transfer-cost/locality scoring (:296). Resource-fit slots in the same position.

The data already flows: `Worker` carries `memory_total_bytes` from the heartbeat's `ResourceUsage` (cgroup.rs:667 sends it). The derivation's `est_memory_bytes` comes from the same `Estimator` read that P0501 uses for the manifest — same `HistoryEntry`, same bucketing.

## Entry criteria

- [P0501](plan-0501-capacity-manifest-rpc.md) merged (`bucketed_estimate` helper exists on Estimator; this plan reuses it)

## Tasks

### T1 — `feat(scheduler):` add est_memory_bytes to DAG node or dispatch context

The placement filter needs `drv.est_memory_bytes` at dispatch time. Two options:
- **Compute at dispatch**: call `estimator.bucketed_estimate()` inside `assign_to_worker`. Cheap (hashmap lookup) but couples assignment to estimator.
- **Cache on DAG node**: populate at DAG-merge time (same place `classify()` runs today), carry on the node struct. One extra field, no dispatch-path estimator dep.

Prefer cache-on-node: `classify()` already runs the estimator lookup for the size-class string. This plan adds the numeric estimate alongside it. MODIFY the DAG node struct and the classify callsite.

### T2 — `feat(scheduler):` resource-fit filter in assign_to_worker

MODIFY [`rio-scheduler/src/assignment.rs`](../../rio-scheduler/src/assignment.rs). At ~:205, add resource-fit alongside the size-class match:

```rust
// r[impl sched.assign.resource-fit]
// Resource fit: worker's memory ceiling must cover the derivation's
// bucketed estimate. memory_total_bytes==0 means "unknown ceiling"
// (cgroup memory.max=max → no limit → heartbeat sends 0) — treated
// as always-fits. Overflow routing falls out: a 16Gi drv fits a 64Gi
// worker. ADR-020 § Decision ¶5.
&& match drv.est_memory_bytes {
    None => true, // cold start — no estimate, any worker fits
    Some(est) => w.memory_total_bytes == 0 || w.memory_total_bytes >= est,
}
```

The size-class string match STAYS — it's gated on `target_class.is_some()`, which only happens under size-class config (Static mode). When size-classes aren't configured, `target_class` is None and the filter is a no-op. Manifest mode pods don't carry `size_class`, so the existing filter passes them through; the new resource-fit filter does the work.

### T3 — `test(scheduler):` resource-fit filter cases

Extend `rio-scheduler/src/assignment.rs` tests (~:673 is the existing `size_class_filter` test). New cases:

- Worker at 8Gi, drv est 6Gi → fits (6 ≤ 8)
- Worker at 8Gi, drv est 12Gi → doesn't fit
- Worker at 64Gi, drv est 6Gi → fits (overflow routing)
- Worker at 0 (unknown ceiling), drv est 128Gi → fits (0 = unlimited)
- Drv with `est_memory_bytes: None` (cold start) → any worker fits

## Exit criteria

- DAG node carries `est_memory_bytes: Option<u64>` populated at classify-time
- Worker at 8Gi rejects 12Gi derivation (test)
- Worker at 0 (unknown) accepts any derivation (test)
- `None` estimate accepts any worker (test)
- Size-class string match still works in Static mode (existing test still passes)
- Transfer-cost/locality scoring unchanged — filter precedes, doesn't replace

## Tracey

- `r[sched.assign.resource-fit]` — NEW marker. T2 is `r[impl]`; T3 is `r[verify]`.

## Spec additions

Add to `docs/src/components/scheduler.md` (new section, or near the existing dispatch/assignment markers):

```
r[sched.assign.resource-fit]

Under manifest mode (ADR-020), `assign_to_worker` filters workers by `memory_total_bytes >= est_memory_bytes` as a hard filter preceding transfer-cost scoring. A worker reporting `memory_total_bytes == 0` (cgroup `memory.max=max`, no k8s limit set) is treated as unlimited-fit. A derivation with no history estimate (cold start) fits any worker. Overflow routing is natural: a 16Gi derivation may be placed on a 64Gi worker if that worker is the best transfer-cost fit among those that pass the hard filter.
```

## Files

```json files
["rio-scheduler/src/assignment.rs", "rio-scheduler/src/dag.rs", "docs/src/components/scheduler.md"]
```

`dag.rs` (or wherever the DAG node struct lives — grep for where `classify()` result is stored).

## Dependencies

```json deps
[501]
```

- P0501: `bucketed_estimate` helper on Estimator. T1 reuses it for DAG-node population. (The RPC itself isn't needed here — just the helper.)

Does NOT depend on P0503 — this filter works with ANY worker that reports `memory_total_bytes`, whether spawned by manifest reconciler, ephemeral, or STS. P0503 makes the worker population heterogeneous; this filter routes correctly across heterogeneity. But homogeneous workers + this filter is still correct (all pass or all fail).

## Risks

- `memory_total_bytes == 0` semantics: per cgroup.rs:642 comment, 0 = "unknown ceiling". Verify there's no OTHER code path that sets it to 0 meaning "zero memory." Grep for `memory_total_bytes` writes.
- The filter is AND-ed with `has_capacity()`. If ALL workers fail the filter, the derivation stays queued forever. P0503's reconciler should eventually spawn a fitting pod, but under misconfiguration (manifest disabled, no big pods) → starvation. Maybe: a `rio_scheduler_unplaceable_derivations` gauge? Follow-up, not this plan.
