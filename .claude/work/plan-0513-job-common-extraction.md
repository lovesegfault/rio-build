# Plan 513: extract builderpool/job_common.rs — manifest ↔ ephemeral shared segments

Consolidator finding (mc=60, **ungated from mc=55** now that P0505+P0507 are both DONE): `reconcile_manifest` ([`manifest.rs`](../../rio-controller/src/reconcilers/builderpool/manifest.rs), 945L) and `reconcile_ephemeral` ([`ephemeral.rs`](../../rio-controller/src/reconcilers/builderpool/ephemeral.rs), 832L) share ~50L across 4 byte-identical-or-near segments. P0505 added manifest-only scale-down (~70L with no ephemeral analog), so cross-file duplication did NOT grow this window — but cross-ref density DID: `manifest.rs` now reaches into `super::ephemeral::{scheduler_unreachable_condition, random_suffix}`. Two hard deps on a sibling module for helpers that belong in neither.

Timing is ideal: no active worktrees on either file, both gates (P0505, P0507) merged, `manifest.rs` at 945L (388 comment / 508 code / 12 fn / 15-item test-import surface) approaching the split threshold. Extract now and buy ~25L headroom before the 1000L re-evaluation at mc=70.

## Segments

| # | What | manifest.rs | ephemeral.rs | Lines | Identity |
|---|---|---|---|---|---|
| 1 | prologue (ns/name/jobs_api) | `:138-142` | `:126-130` | 5 | byte-identical |
| 2 | `is_active_job` predicate | `:202-206` | `:149-152` | 4 | byte-identical closure body |
| 3 | `spawn_prerequisites` (oref + SchedulerAddrs) | `:243-250` | `:200-207` | 8 | byte-identical |
| 4 | `patch_job_pool_status` | `:367-388` | `:267-287` | 22 | `json!` shape identical; var name differs |

Plus two cross-refs to consolidate: `super::ephemeral::scheduler_unreachable_condition` and `super::ephemeral::random_suffix` move to `job_common`.

**Bonus type-linkage fix:** `Bucket = (u64, u32)` at [`manifest.rs:118`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) is `pub(super)`. The parent module [`reconcilers/mod.rs:88`](../../rio-controller/src/reconcilers/mod.rs) `ManifestIdleState` re-inlines the tuple literally because it can't see `pub(super)` from a child's perspective — no compiler check links the two tuples. Move `Bucket` to `job_common` with `pub(crate)` visibility.

## Tasks

### T1 — `refactor(controller):` create job_common.rs with shared helpers

NEW [`rio-controller/src/reconcilers/builderpool/job_common.rs`](../../rio-controller/src/reconcilers/builderpool/job_common.rs). Sibling of `builders.rs` (which owns pod-spec building — clear division: `builders` = K8s object shapes, `job_common` = reconcile-loop plumbing).

Contents:

```rust
//! Shared plumbing for the two Job-mode reconcilers (`manifest.rs`,
//! `ephemeral.rs`). Both follow the same skeleton: list Jobs by label,
//! filter active, diff against demand, spawn deficit, patch status.
//! The diff is different (per-bucket vs flat-ceiling); the plumbing
//! around it is the same.

/// `(est_memory_bytes, est_cpu_millicores)` — manifest-mode bucket key.
/// Shared between the manifest reconciler's internal maps and
/// `Ctx::manifest_idle`'s per-pool idle state. `pub(crate)` so the
/// parent `reconcilers::mod` sees the same type alias instead of
/// re-inlining the tuple.
pub(crate) type Bucket = (u64, u32);

/// Neither Succeeded nor Failed → still active (running or pending).
/// `None` status treated as active — fresh Job before Job controller
/// populates; don't double-spawn.
pub(super) fn is_active_job(j: &Job) -> bool { /* segment 2 */ }

/// ns/name/jobs_api prelude. Both reconcilers start here.
pub(super) fn job_reconcile_prologue(wp: &BuilderPool, ctx: &Ctx)
    -> Result<(String, String, Api<Job>)> { /* segment 1 */ }

/// OwnerReference + SchedulerAddrs — both spawn paths need both.
pub(super) fn spawn_prerequisites(wp: &BuilderPool, ctx: &Ctx)
    -> Result<(OwnerReference, SchedulerAddrs)> { /* segment 3 */ }

/// SSA-patch `.status.{replicas,readyReplicas,desiredReplicas}`.
/// "Replicas" means "active Jobs" in Job-mode — `kubectl get wp`
/// shows the same columns either way.
pub(super) async fn patch_job_pool_status(
    wp_api: &Api<BuilderPool>, name: &str,
    active: i32, ready: i32, desired: i32,
) -> Result<()> { /* segment 4 — unify var names */ }

pub(super) fn random_suffix() -> String { /* moved from ephemeral */ }
pub(super) fn scheduler_unreachable_condition(/* … */) { /* moved from ephemeral */ }
```

MODIFY [`rio-controller/src/reconcilers/builderpool/mod.rs`](../../rio-controller/src/reconcilers/builderpool/mod.rs) — add `mod job_common;`.

### T2 — `refactor(controller):` rewrite manifest.rs + ephemeral.rs over job_common

MODIFY both files. Replace the four segments with calls/imports. For segment 2 specifically, the closure `|j| { ... }` at both sites becomes `|j| is_active_job(j)` or directly `.filter(|j| is_active_job(j))`.

`manifest.rs` imports change: drop `super::ephemeral::{scheduler_unreachable_condition, random_suffix}`, add `super::job_common::{…}`. `Bucket` type alias at `:118` → `pub(super) use super::job_common::Bucket;` re-export (keeps `manifest::Bucket` working for `manifest_tests.rs` imports at `:23`).

`ephemeral.rs` drops the original `random_suffix` + `scheduler_unreachable_condition` defs (moved out).

### T3 — `refactor(controller):` ManifestIdleState uses the Bucket type alias

MODIFY [`rio-controller/src/reconcilers/mod.rs`](../../rio-controller/src/reconcilers/mod.rs). `ManifestIdleState`'s inner map key (currently `(u64, u32)` literal) → `builderpool::job_common::Bucket`. One line; closes the type-linkage gap.

### T4 — `test(controller):` nextest green

No new tests — pure extraction, behavior-preserving. `nix develop -c cargo nextest run -p rio-controller` must stay green. Existing `manifest_tests.rs` + `ephemeral_tests.rs` (if it exists) cover the call sites.

`manifest_tests.rs:23` import surface may need adjustment if any extracted items change visibility — verify at dispatch.

## Exit criteria

- `nix develop -c cargo nextest run -p rio-controller` green
- `wc -l rio-controller/src/reconcilers/builderpool/manifest.rs` < 925 (was 945; ~25L drop expected)
- `grep 'super::ephemeral::' rio-controller/src/reconcilers/builderpool/manifest.rs` → 0 hits (cross-refs consolidated)
- `grep '(u64, u32)' rio-controller/src/reconcilers/mod.rs` → 0 hits near `ManifestIdleState` (type alias in use)
- `nix develop -c cargo clippy --all-targets -- --deny warnings` green
- `grep -c 'is_active_job\|job_reconcile_prologue\|spawn_prerequisites\|patch_job_pool_status' rio-controller/src/reconcilers/builderpool/job_common.rs` ≥ 4

## Tracey

References existing markers (no new markers — pure refactor):
- `r[ctrl.pool.ephemeral]` — segment-1/2/3/4 at `ephemeral.rs` are under this umbrella; extraction doesn't change behavior, `r[impl]` annotations move with the code OR stay at the call site (whichever is more specific — segment 4 `patch_job_pool_status` is generic enough that the annotation stays at each caller).
- `r[ctrl.pool.manifest-reconcile]` — same for the `manifest.rs` side.

No `r[impl]` annotations should be on the extracted helpers themselves (they're plumbing, not spec-behavior). If any move accidentally, `tracey query validate` stays green either way — the helper is under a file that `config.styx` scans.

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/builderpool/job_common.rs", "action": "NEW", "note": "T1: Bucket type alias pub(crate) + is_active_job + job_reconcile_prologue + spawn_prerequisites + patch_job_pool_status + random_suffix + scheduler_unreachable_condition"},
  {"path": "rio-controller/src/reconcilers/builderpool/manifest.rs", "action": "MODIFY", "note": "T2: replace 4 segments with job_common calls; drop super::ephemeral:: cross-refs; Bucket → re-export. Net ~-25L"},
  {"path": "rio-controller/src/reconcilers/builderpool/ephemeral.rs", "action": "MODIFY", "note": "T2: replace 4 segments with job_common calls; drop moved random_suffix + scheduler_unreachable_condition defs"},
  {"path": "rio-controller/src/reconcilers/builderpool/mod.rs", "action": "MODIFY", "note": "T1: add mod job_common;"},
  {"path": "rio-controller/src/reconcilers/mod.rs", "action": "MODIFY", "note": "T3: ManifestIdleState inner key (u64,u32) literal → job_common::Bucket alias"}
]
```

```
rio-controller/src/reconcilers/
├── mod.rs                      # T3: Bucket alias at ManifestIdleState
└── builderpool/
    ├── mod.rs                  # T1: mod job_common;
    ├── job_common.rs           # T1: NEW — shared plumbing
    ├── manifest.rs             # T2: 4 segments → calls; ~-25L
    └── ephemeral.rs            # T2: 4 segments → calls; defs moved out
```

## Dependencies

```json deps
{"deps": [505, 507], "soft_deps": [511], "note": "P0505 (scale-down) + P0507 (truncation-starvation) were the gate — both DONE, UNGATED. Soft-dep P0511: the Failed-Job sweep adds a failed_jobs filter immediately after segment-2's active_jobs filter. If P0511 lands FIRST, its is_failed_job predicate goes into job_common too (inverse of is_active_job). If THIS lands first, P0511 rebases to put is_failed_job alongside the extracted is_active_job."}
```

**Depends on:** [P0505](plan-0505-manifest-scaledown-grace.md) + [P0507](plan-0507-manifest-truncation-starvation.md) — both DONE. mc=55 consolidator was gated on these; mc=60 confirmed both merged, no active worktrees.

**Conflicts with:** [P0511](plan-0511-manifest-failed-job-sweep.md) touches `manifest.rs:199-207` (segment 2's vicinity). Serialization preferred: P0511 FIRST (smaller, ~15-line insert), this SECOND (~50-line churn). `manifest.rs` not currently top-20 collisions but touched 6× this window.

[P0304](plan-0304-trivial-batch-p0222-harness.md) T505/T506/T507 touch `ephemeral.rs` + `manifest.rs` (rpc_name param, sizing filter, floor job name) — all are small localized edits; this plan's segment boundaries are at `:126-130, :149-152, :200-207, :267-287` in ephemeral.rs. T505 is at `scheduler_unreachable_condition` which MOVES to `job_common` here — **coordinate at dispatch**: if T505 hasn't landed, apply the rpc_name param change IN job_common post-extraction.

## Risks

- **mc=55 entry SUPERSEDED:** this plan supersedes the mc=55 consolidator row from the same followups batch. That row named 6 segments at ~80L; mc=60 recounted 4 segments at ~50L after P0505's manifest-only scale-down added non-shared code. mc=60 is ground truth.
- **Segment-4 var name unification:** `manifest.rs:367-388` and `ephemeral.rs:267-287` have the same `json!` shape but different variable names for the `active`/`ready`/`desired` ints. `patch_job_pool_status` takes all three as params — this is a naming unification, not a behavior change. Verify the status fields map identically (both use `.status.replicas`, `.status.readyReplicas`, `.status.desiredReplicas`).
- **Test-import surface:** `manifest_tests.rs:23` imports 15 items directly from `manifest`. If `Bucket` is re-exported (not moved), the import stays. If other items move without re-export, the test imports break — clippy catches this. Prefer re-exports for anything the tests import directly.
