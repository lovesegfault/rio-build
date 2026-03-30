# Plan 501: ADR-020 phase 1 — GetCapacityManifest RPC + scheduler queue-shape aggregation

[ADR-020](../../docs/src/decisions/020-per-derivation-capacity-manifest.md) replaces the scalar `ClusterStatus.queued_derivations` autoscale signal with a per-derivation resource manifest. This is the foundational RPC: the controller polls `AdminService.GetCapacityManifest` alongside `ClusterStatus`, gets `repeated DerivationResourceEstimate` for queued-ready derivations, groups by (memory, cpu) buckets, and spawns right-sized pods.

The pull model is load-bearing — no new gRPC server, no push. `admin.proto:56-63` documents why the speculative `ControllerService` push RPC was removed. This plan lands a *sibling* to `GetSizeClassStatus` (:53), not a replacement.

The scheduler already has the data: `Estimator` ([rio-scheduler/src/estimator.rs:34](../../rio-scheduler/src/estimator.rs)) holds `HistoryEntry{ema_duration_secs, ema_peak_memory_bytes, ema_peak_cpu_cores}` per `(pname, system)`, refreshed every ~60s tick from `build_history`. P0435's per-class aggregation query is the same read path — this plan removes the GROUP BY.

## Tasks

### T1 — `feat(proto):` add GetCapacityManifest to AdminService

MODIFY [`rio-proto/proto/admin.proto`](../../rio-proto/proto/admin.proto). Add after `GetSizeClassStatus` (~:54):

```proto
// r[sched.admin.capacity-manifest]
// Per-derivation resource estimates for queued-ready derivations. Polled
// by the controller's manifest reconciler (ADR-020 sizing=Manifest mode).
// Sibling to ClusterStatus.queued_derivations — this is the detailed
// shape behind that scalar count. Pull model, same as ClusterStatus.
rpc GetCapacityManifest(GetCapacityManifestRequest) returns (GetCapacityManifestResponse);
```

MODIFY [`rio-proto/proto/types.proto`](../../rio-proto/proto/types.proto). Add message types:

```proto
message GetCapacityManifestRequest {
  // Empty — manifest is always for the full ready queue.
}

message DerivationResourceEstimate {
  // EMA × headroom multiplier, rounded UP to nearest 4Gi bucket.
  // 0 = no history sample yet (cold start — controller uses the floor).
  uint64 est_memory_bytes = 1;
  // EMA × headroom, rounded UP to nearest 2-core bucket. 0 = cold start.
  uint32 est_cpu_millicores = 2;
  // EMA duration. Informational — controller MAY use for grace-period
  // hints (P0505); scheduler already uses for critical-path priority.
  uint32 est_duration_secs = 3;
}

message GetCapacityManifestResponse {
  repeated DerivationResourceEstimate estimates = 1;
}
```

Millicores (not cores) because k8s `ResourceRequirements` speaks millicores — saves the controller a ×1000.

### T2 — `feat(scheduler):` bucketed_estimate helper on Estimator

MODIFY [`rio-scheduler/src/estimator.rs`](../../rio-scheduler/src/estimator.rs). Add a method to `Estimator`:

```rust
/// Bucket a HistoryEntry for the capacity manifest.
///
/// Applies `headroom_mult` then rounds UP: memory to 4GiB, CPU to 2000mcores.
/// Returns None if the entry has no memory sample (cold start — caller
/// uses the operator floor, not a guess).
///
/// Rounding-at-source is load-bearing: two derivations at 6.2Gi and 7.8Gi
/// both land in the 8Gi bucket → share a pod sequentially. Without bucketing
/// every derivation is a unique (mem, cpu) pair and the controller would
/// spawn N single-use pods. ADR-020 § Decision ¶2.
// r[impl sched.admin.capacity-manifest.bucket]
pub fn bucketed_estimate(&self, entry: &HistoryEntry, headroom_mult: f64)
    -> Option<rio_proto::types::DerivationResourceEstimate>
```

Bucket math: `(ema × mult).ceil_to(4 * GiB)` for memory, `ceil_to(2000)` for millicores. Use `(x + b - 1) / b * b` integer ceiling after f64→u64.

### T3 — `feat(scheduler):` GetCapacityManifest handler in admin.rs

MODIFY [`rio-scheduler/src/admin.rs`](../../rio-scheduler/src/admin.rs). Add handler alongside `get_size_class_status`. Walk the ready queue (DAG nodes where all deps built, blocked only on worker availability — same set `queued_derivations` counts), look up each `(pname, system)` in the Estimator, call `bucketed_estimate`, collect. Cold-start entries (no history) are OMITTED — the controller's deficit calculation treats missing as "use floor."

Handler config: `headroom_multiplier` from scheduler config (new field, default 1.25). Per-ADR: start scheduler-global, per-pool later if needed.

### T4 — `test(scheduler):` bucketing + handler unit tests

NEW cases in `rio-scheduler/src/estimator.rs` `#[cfg(test)]`:
- 6.2Gi × 1.25 = 7.75Gi → buckets to 8Gi (not 4Gi, not 12Gi)
- 0.3 cores × 1.25 = 0.375 → 375 mcores → buckets to 2000 (minimum bucket)
- `ema_peak_memory_bytes: None` → returns None (cold start)

NEW cases in `rio-scheduler/src/admin.rs` `#[cfg(test)]`:
- Ready queue with 3 derivations: 2 with history, 1 cold → response has 2 estimates
- Empty queue → empty response (not error)

## Exit criteria

- `GetCapacityManifest` RPC compiles and appears in generated Rust
- `bucketed_estimate` rounds 6.2Gi×1.25 → 8Gi (test)
- Cold-start derivations omitted from manifest (test)
- Handler walks ready queue, not full DAG (already-building nodes excluded)
- `headroom_multiplier` config field with default 1.25, validated >0 at startup

## Tracey

- `r[sched.admin.capacity-manifest]` — NEW marker. T1 proto comment is the spec site; T3 is `r[impl]`; T4 is `r[verify]`.
- `r[sched.admin.capacity-manifest.bucket]` — NEW marker. T2 is `r[impl]`; T4 bucketing test is `r[verify]`.

## Spec additions

Add to `docs/src/components/scheduler.md` after `r[sched.admin.sizeclass-status]` (~:161):

```
r[sched.admin.capacity-manifest]

`GetCapacityManifest` returns per-derivation resource estimates for queued-ready derivations (DAG nodes with all deps built, waiting only on worker availability). Polled by the controller's manifest reconciler under `BuilderPool.spec.sizing=Manifest` mode (ADR-020). Pull model — sibling to `ClusterStatus`, not a push RPC.

r[sched.admin.capacity-manifest.bucket]

Estimates are `EMA × headroom_multiplier`, rounded UP to 4GiB memory buckets and 2000-millicore CPU buckets. Bucketing at the scheduler (not the controller) means all consumers see identical buckets — two derivations that should share a pod don't diverge from floating-point rounding applied in different places. Cold-start derivations (no `build_history` sample) are omitted; the controller uses its operator-configured floor.
```

## Files

```json files
["rio-proto/proto/admin.proto", "rio-proto/proto/types.proto", "rio-scheduler/src/estimator.rs", "rio-scheduler/src/admin.rs", "rio-scheduler/src/config.rs", "docs/src/components/scheduler.md"]
```

## Dependencies

```json deps
[]
```

No deps. Foundational — P0503 (controller reconciler) and P0504 (placement filter) depend on this.

## Risks

- `types.proto` is a high-traffic file — rebase conflicts likely if other proto plans are in flight. Check `onibus collisions check 501` before impl.
- `headroom_multiplier` validation: `> 0.0 && is_finite()` (same pattern as P0424's cpu_limit_cores fix — NaN would silently yield 0-byte buckets).
