# Plan 503: ADR-020 phase 3 — controller manifest-diff reconciler

[ADR-020](../../docs/src/decisions/020-per-derivation-capacity-manifest.md) § Decision ¶3: heterogeneous pod creation via per-size Job batches. The controller groups the manifest by `(est_memory, est_cpu)`, compares to live pod inventory, spawns Jobs for the deficit.

This is a variant of [`rio-controller/src/reconcilers/builderpool/ephemeral.rs`](../../rio-controller/src/reconcilers/builderpool/ephemeral.rs). `reconcile_ephemeral` (:125) polls `ClusterStatus.queued_derivations` (scalar count) and spawns N identical Jobs. `reconcile_manifest` polls `GetCapacityManifest` (detailed shape) and spawns Jobs with varying `ResourceRequirements`. Same Job-spawn machinery, different pod-spec source.

The key change: `builders::build_pod_spec` (called at ephemeral.rs:423) currently reads resources from `BuilderPool.spec`. Manifest mode needs it to accept resources as an ARGUMENT. Refactor the function signature or add an override variant.

## Entry criteria

- [P0501](plan-0501-capacity-manifest-rpc.md) merged (`GetCapacityManifest` RPC + `DerivationResourceEstimate` type exist)
- [P0502](plan-0502-builderpool-sizing-enum.md) merged (`spec.sizing` enum exists to gate on)

## Tasks

### T1 — `refactor(controller):` build_pod_spec accepts ResourceRequirements override

MODIFY [`rio-controller/src/reconcilers/builderpool/builders.rs`](../../rio-controller/src/reconcilers/builderpool/builders.rs). Change `build_pod_spec` signature: add `resources_override: Option<ResourceRequirements>`. When `Some`, use that instead of reading from `wp.spec.resources`. `None` preserves today's behavior (STS path, ephemeral path).

Update the two existing callers (STS reconciler, `ephemeral.rs:423`) to pass `None`. Zero behavior change for them.

### T2 — `feat(controller):` reconcile_manifest — poll + diff + spawn

NEW file `rio-controller/src/reconcilers/builderpool/manifest.rs` (or extend ephemeral.rs with a mode switch — judgment call, lean toward new file for clarity). Structure:

1. **Poll** `ctx.admin.get_capacity_manifest()` alongside `ClusterStatus`
2. **Group** estimates by `(est_memory_bytes, est_cpu_millicores)` → `BTreeMap<(u64, u32), usize>` (count per bucket). BTreeMap for deterministic iteration order (stable Job naming across ticks).
3. **Inventory** live Jobs: list Jobs with label selector `rio.build/pool={name},rio.build/sizing=manifest`, extract each Job's `(memory-class, cpu-class)` from labels → same-shaped map
4. **Diff**: for each demand bucket, `deficit = demand.saturating_sub(supply)`. For cold-start derivations (omitted from manifest), treat as one bucket at `spec.resources` (the floor).
5. **Spawn**: for each deficit, call `build_pod_spec` with `resources_override: Some({memory, cpu})`. Label the Job `rio.build/memory-class={n}Gi`, `rio.build/cpu-class={n}m`. Job name: `{pool}-mf-{mem_gi}g-{cpu_m}m-{random6}` (readable + collision-safe).

Requeue interval: same ~10s as ephemeral (not 5min like STS). Manifest is demand-driven.

### T3 — `feat(controller):` gate reconciler dispatch on spec.sizing

MODIFY the top-level `BuilderPool` reconcile dispatch (wherever `reconcile_ephemeral` vs STS is chosen). Add `match wp.spec.sizing`: `Static` → existing path; `Manifest` → `reconcile_manifest`. Manifest mode does NOT require `ephemeral: true` (manifest pods are long-lived, not one-shot — ADR-020 § Decision ¶4).

### T4 — `test(controller):` manifest diff unit tests

Unit tests for the diff logic (no k8s): given a manifest with buckets `{(8Gi,2000m): 3, (32Gi,4000m): 1}` and inventory `{(8Gi,2000m): 1}`, expect spawn calls for `(8Gi,2000m)×2 + (32Gi,4000m)×1`. Over-provisioned bucket (supply > demand) → zero spawns for that bucket (scale-down is P0505's job).

Cold-start test: manifest omits 2 derivations (returned by `ClusterStatus.queued_derivations - manifest.len()`), expect 2 Jobs at the floor spec.

## Exit criteria

- `build_pod_spec(..., resources_override: None)` compiles at existing call sites (zero behavior change)
- `reconcile_manifest` groups by bucket, computes deficit, spawns with correct labels
- `spec.sizing == Static` takes existing code path (regression: existing BuilderPools still work)
- Diff test: 3 demand / 1 supply → 2 spawns
- Cold-start test: omitted derivations get floor-sized Jobs
- Job labels `rio.build/memory-class`, `rio.build/cpu-class` present on spawned Jobs

## Tracey

- `r[ctrl.pool.manifest-reconcile]` — NEW marker. T2 is `r[impl]`; T4 is `r[verify]`.
- `r[ctrl.pool.manifest-labels]` — NEW marker. T2 label code is `r[impl]`; T4 label assertion is `r[verify]`.

## Spec additions

Add to `docs/src/components/controller.md` after `r[ctrl.pool.ephemeral]` (~:62):

```
r[ctrl.pool.manifest-reconcile]

Under `spec.sizing=Manifest`, the reconciler polls `GetCapacityManifest`, groups estimates by `(est_memory_bytes, est_cpu_millicores)` buckets, diffs against live Job inventory (by label), and spawns Jobs for the deficit. Each spawned Job's pod gets `ResourceRequirements` from the manifest bucket, not from `spec.resources`. `spec.resources` becomes the cold-start floor (used for derivations the manifest omits — no `build_history` sample yet).

r[ctrl.pool.manifest-labels]

Manifest-spawned Jobs carry `rio.build/memory-class={n}Gi` and `rio.build/cpu-class={n}m` labels. Operators can `kubectl get job -l rio.build/memory-class=48Gi` to see the 48Gi fleet; the reconciler uses the same labels for its inventory count.
```

## Files

```json files
["rio-controller/src/reconcilers/builderpool/builders.rs", "rio-controller/src/reconcilers/builderpool/manifest.rs", "rio-controller/src/reconcilers/builderpool/mod.rs", "docs/src/components/controller.md"]
```

Possibly `ephemeral.rs` if the dispatch gate lives there — T3 decides.

## Dependencies

```json deps
[501, 502]
```

- P0501: `GetCapacityManifest` RPC + `DerivationResourceEstimate` type — T2's poll depends on this
- P0502: `spec.sizing` enum — T3's gate depends on this

## Risks

- `build_pod_spec` signature change touches existing callers. Low risk (additive `Option`) but compile-check all callers including any tests.
- Inventory label selector must exactly match what T2 sets. Typo = perpetual over-spawn (reconciler never sees its own Jobs). T4 should assert round-trip: spawn with labels, list with selector, count matches.
- `ClusterStatus.queued_derivations - manifest.len()` cold-start math: manifest omits both cold-start AND already-dispatched. Verify the ready-queue definition matches between the two RPCs.
