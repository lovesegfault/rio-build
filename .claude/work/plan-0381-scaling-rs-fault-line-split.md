# Plan 0381: Split scaling.rs — cluster-wide vs per-class autoscaler fault line

Consolidator (mc170) flagged [`rio-controller/src/scaling.rs`](../../rio-controller/src/scaling.rs) at 1779L (post-P0374, up from 1466L pre-P0374, up from ~950L pre-P0234). [P0234](plan-0234-wps-status-perclass-autoscaler-yjoin.md) bolted a second autoscaler (per-class WPS, `scale_wps_class` + `GetSizeClassStatus`) onto the same file as the original cluster-wide loop (`scale_one` + `ClusterStatus`). The discriminant is [`is_wps_owned()`](../../rio-controller/src/scaling.rs) at `:958` — standalone pools go through the cluster-wide loop; WPS children go through the per-class loop.

16 plan docs reference `scaling.rs`. The current collision count is high and growing: [P0374](plan-0374-wps-asymmetric-key-scaling-flap.md) +330L, [P0304](plan-0304-trivial-batch-p0222-harness.md) T112/T114/T121 all touch it, [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T40/T41 add tests, [P0380](plan-0380-controller-workerpoolspec-test-fixture-dedup.md) rewrites test literals.

Split into `scaling/{mod.rs, standalone.rs, per_class.rs}` along the same fault line [P0356](plan-0356-split-scheduler-grpc-service-impls.md) used for `grpc/mod.rs` (1087L → 351L + scheduler_service.rs + worker_service.rs). Collision surface drops to the SHARED-types mod (~300L) for most future plans.

## Entry criteria

- [P0374](plan-0374-wps-asymmetric-key-scaling-flap.md) merged (`is_wps_owned_by`, `find_wps_child`, `ChildLookup`, prune-related tests all land there — without it the split would need re-application after P0374 rebases)
- [P0380](plan-0380-controller-workerpoolspec-test-fixture-dedup.md) merged (collapses 2 `WorkerPoolSpec` literals in the tests mod — makes the tests-move carry 2-line delegations not 30-line literals)

## Tasks

### T1 — `refactor(controller):` scaling/mod.rs — shared types + ownership predicates

Create `rio-controller/src/scaling/mod.rs` with the shared surface (current `:1-994` minus the two autoscaler `impl` blocks):

| Moved item | Current line | Notes |
|---|---|---|
| `ScalingTiming` struct + `Default` | `:59-98` | Both loops use same timing knobs |
| `ScaleState` struct + `::new` | `:100-125` | Per-pool state; keyed by `pool_key()` |
| `Decision` / `Direction` / `WaitReason` enums + `as_str` | `:840-893` | Shared outcome types |
| `check_stabilization()` | `:895-940` | Pure fn; both loops call it |
| `compute_desired()` | `:818-838` | Pure fn; both loops call it |
| `sts_replicas_patch()` | `:793-816` | SSA body builder; both patch STS |
| `wp_status_patch()` + `scaling_condition()` | `:750-791` | Status SSA body; per-class uses `conditions` subset |
| `pool_key()` | `:941-948` | Cache key for `ScaleState` map |
| `is_wps_owned()` + `is_wps_owned_by()` | `:958-994` | Ownership predicates |
| `find_wps_child()` + `ChildLookup` | `:998-1034` | Per-class lookup; exposed `pub(crate)` |
| `pub mod standalone;` + `pub mod per_class;` | new | Module declarations |
| `pub use standalone::Autoscaler;` | new | Back-compat re-export (main.rs uses `scaling::Autoscaler`) |

Tests that exercise the shared pieces stay in `mod.rs::tests`: `wp_status_patch_has_gvk_and_partial_status`, `scaling_condition_has_standard_fields`, `sts_replicas_patch_has_gvk`, `compute_desired_*` (5 tests), `mk_state`, `stabilization_*` (5 tests), `is_wps_owned_detects_controller_ownerref`, `is_wps_owned_by_matches_uid_not_just_kind`, `find_wps_child_*` (3 tests). Roughly `:1037-1650` of the current test mod.

### T2 — `refactor(controller):` scaling/standalone.rs — cluster-wide Autoscaler loop

Extract the `Autoscaler` struct, `::new`, `::run` loop, `tick()`, `scale_one()`, and the cluster-wide-specific helpers. Current location is `:127-750` (before the shared SSA builders). The `tick()` loop at `:273` calls `is_wps_owned(pool)` to SKIP WPS children — that call becomes `crate::scaling::is_wps_owned`. The `wps_manager_distinct_from_standalone` test at `:1758` belongs here (verifies the standalone loop's field-manager is distinct from per-class).

### T3 — `refactor(controller):` scaling/per_class.rs — WPS per-class loop

Extract `scale_wps_class()` and its call chain. This is the P0234-added surface; lives after `tick()` in the current file (~`:450-750` region interleaved with standalone). The per-class-specific tests move here: `scale_wps_class_skips_name_collision_without_ownerref` (`:1552`), and the pending [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T41 happy-path test lands in this file once P0311 dispatches.

### T4 — `refactor(controller):` main.rs + reconcilers import paths — back-compat re-exports

`rio-controller/src/main.rs` currently `use crate::scaling::Autoscaler;` — the `pub use standalone::Autoscaler` re-export in mod.rs keeps this working unchanged. [`reconcilers/workerpoolset/mod.rs`](../../rio-controller/src/reconcilers/workerpoolset/mod.rs) imports `is_wps_owned_by` for `prune_stale_children` — stays `crate::scaling::is_wps_owned_by` (it's in mod.rs).

Check at dispatch: `grep 'use crate::scaling::\|use super::scaling::' rio-controller/src/` — each import either resolves to the mod.rs re-export or needs a path update.

## Exit criteria

- `/nixbuild .#ci` green (or nextest-standalone clause-4c if VM-flake)
- `wc -l rio-controller/src/scaling/mod.rs` → ≤650L (shared types + predicates + shared tests; down from 1779L monolith)
- `test -f rio-controller/src/scaling/standalone.rs && test -f rio-controller/src/scaling/per_class.rs` — both exist
- `grep 'pub use standalone::Autoscaler' rio-controller/src/scaling/mod.rs` → 1 hit (back-compat re-export)
- `cargo nextest run -p rio-controller scaling::` → same test count as pre-split (currently ~20 test fns in the mod; verify via `cargo nextest list -p rio-controller | grep scaling | wc -l` pre/post)
- `grep 'use crate::scaling::' rio-controller/src/main.rs rio-controller/src/reconcilers/` → all paths resolve (no compile break)
- `.claude/bin/onibus collisions check rio-controller/src/scaling.rs` → 0 (the path no longer exists; plan-doc Files fences reference the monolith; future plans cite `scaling/mod.rs` or `scaling/per_class.rs`)
- `.claude/bin/onibus collisions check rio-controller/src/scaling/mod.rs` → ≤6 (reduced collision surface for SHARED types; per-class plans hit per_class.rs instead)

## Tracey

References existing markers:
- `r[ctrl.wps.autoscale]` — per_class.rs `scale_wps_class` implements this (the `r[impl ctrl.wps.autoscale]` annotation at current `:1029` moves with `find_wps_child`)
- `r[ctrl.autoscale.direct-patch]` / `r[ctrl.autoscale.separate-field-manager]` — verify annotations at current `:1035-1036` stay in mod.rs (shared SSA body tests)

No new markers. The split is a file reorganization; it does not change what behaviors are spec'd.

**Post-split tracey sanity:** `nix develop -c tracey query rule ctrl.wps.autoscale` must still show ≥1 impl + ≥1 verify site (annotations moved with their code, not dropped).

## Files

```json files
[
  {"path": "rio-controller/src/scaling/mod.rs", "action": "NEW", "note": "T1: shared types (ScalingTiming/ScaleState/Decision/Direction/WaitReason) + pure fns (check_stabilization/compute_desired/sts_replicas_patch/wp_status_patch/pool_key) + ownership predicates (is_wps_owned/is_wps_owned_by/find_wps_child/ChildLookup) + pub use re-exports + ~15 shared tests"},
  {"path": "rio-controller/src/scaling/standalone.rs", "action": "NEW", "note": "T2: Autoscaler struct + ::new + ::run + tick + scale_one + wps_manager_distinct_from_standalone test"},
  {"path": "rio-controller/src/scaling/per_class.rs", "action": "NEW", "note": "T3: scale_wps_class + per-class call chain + scale_wps_class_skips_name_collision test"},
  {"path": "rio-controller/src/scaling.rs", "action": "DELETE", "note": "T1-T3: replaced by scaling/ directory"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "T4: verify use crate::scaling::Autoscaler resolves via re-export (no change needed if re-export correct)"}
]
```

```
rio-controller/src/scaling/
├── mod.rs          # T1: shared ~600L (types+predicates+tests)
├── standalone.rs   # T2: cluster-wide Autoscaler ~550L
└── per_class.rs    # T3: WPS per-class scale_wps_class ~350L
```

## Dependencies

```json deps
{"deps": [374, 380], "soft_deps": [234, 311, 304], "note": "GATE on P0374 (adds is_wps_owned_by + find_wps_child + ChildLookup + 4 tests — split without it needs re-application). P0380 (fixture dedup) should land FIRST — makes the tests-move carry 2-line delegations not 30-line literals. Soft-dep P0234 (already merged — added per-class loop that created the fault line). Soft-dep P0311 T40/T41 (adds tests to scaling.rs — if lands first, split carries them; if split lands first, T40/T41 target scaling/per_class.rs and scaling/mod.rs). Soft-dep P0304 T112/T114/T121 (all touch scaling.rs — T112 comment-audit, T114 child_name format!→fn, T121 ssa_envelope migration; if split lands first, re-grep their line-refs)."}
```

**Depends on:** [P0374](plan-0374-wps-asymmetric-key-scaling-flap.md) — `is_wps_owned_by`/`find_wps_child`/`ChildLookup` arrive with it. [P0380](plan-0380-controller-workerpoolspec-test-fixture-dedup.md) — fixture collapse makes the test-move smaller.

**Conflicts with:** `scaling.rs` collision count ≈16 plan-doc mentions. [P0304](plan-0304-trivial-batch-p0222-harness.md)-T128 (`is_wps_owned` → `is_wps_owned_by` at `:1030`) — one-line edit to `find_wps_child`; if T128 lands first it moves with T1; if split lands first T128 targets `scaling/mod.rs`. [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T40/T41 add tests — land in `scaling/mod.rs` or `per_class.rs` post-split. Preferred order: fixture-dedup (0380) → this split → P0304 batch → P0311 batch. The split is a big-diff merge; sequence late in a quiet window.
