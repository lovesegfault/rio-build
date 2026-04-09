# `trait JobPool` driver — considered, not adopted

## Status

Rejected (wave-3 cleanup, 2026-04). Revisit if a fourth Job-mode
reconciler appears that is structurally identical to one of the
existing three.

## Context

`common/job.rs` already extracts the byte-identical plumbing shared by
the three Job-mode reconcilers (`builderpool::{static_sizing,manifest}`,
`fetcherpool::jobs`): prologue, Job predicates, `spawn_count`,
`try_spawn_job`/`SpawnOutcome`, both reap helpers, `patch_job_pool_status`,
constants. The question was whether the remaining per-reconciler
`reconcile()` bodies (~120–170 LoC each, comment-heavy) should be
collapsed under a `trait JobPool` with one generic driver.

## Decision

Keep the current shape: free-function helpers in `common/job.rs`,
hand-written `reconcile()` per role.

## Why

**The three flows differ structurally, not parametrically.** A trait
driver wants `poll → diff → spawn → reap → patch`. What we have:

| step       | static_sizing            | fetcherpool                  | manifest                                  |
|------------|--------------------------|------------------------------|-------------------------------------------|
| poll       | one u32 (size-class RPC) | `QueueSignals{flat,by_class}`| `(Vec<Estimate>, u32)` from two RPCs      |
| iteration  | flat (1 unit)            | per-`classes[]` loop         | per-bucket plan + global truncate         |
| pre-spawn  | —                        | —                            | `sweep_failed` **must** precede spawn (ResourceQuota deadlock) |
| spawn      | `for 0..n {try_spawn}`   | same, inside class loop      | `spawn_manifest_jobs` with consecutive-fail threshold |
| reap       | `reap_excess_pending` + `reap_orphan_running` | same, per-class | neither — `reap_surplus_manifest_jobs` (bucket idle-grace + `Ctx::manifest_idle` mutex) |
| status     | `replicas: Some(active)` | `replicas: None`             | `replicas: Some(active)`                  |

`manifest` shares prologue/patch and the Job predicates; everything
between is its own algorithm. A driver that covers it needs ~8 hook
methods, most of which manifest overrides entirely — that is relocation,
not deduplication.

**Two-of-three coverage doesn't clear the bar either.** Unifying only
`static_sizing` + `fetcherpool` (treat static as "fetcher with one
class") was costed:

- before: ~292 LoC across two `reconcile()` bodies
- after: trait def (~50) + driver (~90) + two impls (~100) ≈ 240 LoC
- net: ~17% reduction, below the 20% threshold, and:
  - associated `type Class` is `()` for static — unit-type filler
  - `poll_queued` must return a per-class lookup closure carrying
    `idx` for the smallest-class flat-fallback — awkward signature
  - the I-183 list-after-poll ordering comment, I-165/I-183 reap
    rationale, and per-role fail-open/closed notes currently sit at
    the line they govern; a generic driver hosts one merged comment
    that is true for neither role precisely
  - a `trait JobPool` that silently excludes the most complex Job
    reconciler (`manifest`) is a misleading name

**What is already shared is the right cut.** The wave-1 extraction
landed every segment that was byte-identical or near. What remains in
each `reconcile()` is the role-specific control flow plus the
commentary explaining *why* that flow has its shape — exactly the part
a reader needs co-located.

## Revisit when

- a fourth reconciler arrives that matches `static_sizing` or
  `fetcherpool` step-for-step, or
- `manifest` drops its bucket/idle machinery and converges on the
  pending/orphan reap path.
