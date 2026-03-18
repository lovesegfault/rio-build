# Plan 0266: Scheduler mid-build ema update (proactive ema — USER Q1 reframe)

**USER Q1 reframe — second half.** Consume [P0265](plan-0265-worker-resource-usage-emit.md)'s `ProgressUpdate.resources` and update `build_samples.ema_peak_memory_bytes` **while the build is still running**. No kill, no redispatch, no threshold check. The existing `r[sched.classify.penalty-overwrite]` at [`completion.rs:336`](../../rio-scheduler/src/actor/completion.rs) already handles OOM→bump on the NEXT attempt; this plan makes that bump happen EARLIER (mid-build instead of post-completion).

**Architectural shift from partition default:** the original plan touched `actor/mod.rs` (count=31, #2 HOTTEST) for OOM-detect+kill. Per USER Q1, no kill → the work moves to a MUCH safer location: consuming `ProgressUpdate` in the existing stream handler and updating ema via a db query. Lower collision, smaller scope.

## Entry criteria

- [P0265](plan-0265-worker-resource-usage-emit.md) merged (worker emits `ProgressUpdate.resources`)

## Tasks

### T1 — `feat(scheduler):` consume ProgressUpdate.resources

MODIFY the scheduler's `ProgressUpdate` stream handler (find at dispatch: `rg 'ProgressUpdate' rio-scheduler/src/` — likely in `grpc/` or `actor/`):

```rust
// USER Q1 reframe: proactive ema. NO kill. If a running build is
// already over what ema predicted, update ema NOW — the next submit
// of this drv gets right-sized before this build even finishes.
if let Some(resources) = progress_update.resources {
    if resources.peak_memory_bytes > current_ema {
        // Same update logic as completion.rs:336 penalty-overwrite,
        // just triggered earlier.
        db.update_ema_peak_memory(drv_hash, resources.peak_memory_bytes).await?;
        metrics::counter!("rio_scheduler_ema_proactive_updates_total").increment(1);
    }
}
```

### T2 — `test(scheduler):` mid-build sample updates ema

```rust
#[tokio::test]
async fn mid_build_resource_sample_updates_ema() {
    // Start build with ema_peak_memory=1GB.
    // Send ProgressUpdate{resources.peak_memory_bytes=2GB}.
    // Assert db.ema_peak_memory updated to ~2GB BEFORE build completes.
}
```

## Exit criteria

- `/nbr .#ci` green
- `rg 'CancelBuild|kill' $(git diff --name-only main -- rio-scheduler/)` → 0 (USER Q1: NO kill code added)

## Tracey

none — proactive ema is a refinement of existing `r[sched.classify.penalty-overwrite]` behavior, not a new normative requirement. Per USER Q1: no new marker.

## Files

```json files
[
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "T1: consume ProgressUpdate.resources → ema update (find exact handler at dispatch)"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "T1: update_ema_peak_memory query (may already exist from completion.rs path)"}
]
```

```
rio-scheduler/src/
├── grpc/mod.rs                   # T1: ProgressUpdate consumer (NOT actor/mod.rs per Q1)
└── db.rs                         # T1: ema update query
```

## Dependencies

```json deps
{"deps": [265], "soft_deps": [], "note": "USER Q1 REFRAME: proactive ema ONLY. NO actor/mod.rs touch (was count=31 HOTTEST in original plan). NO CancelBuild. NO kill. db.rs touch is small — may already have the ema-update fn from completion.rs."}
```

**Depends on:** [P0265](plan-0265-worker-resource-usage-emit.md) — worker emits.
**Conflicts with:** Per USER Q1, AVOIDS `actor/mod.rs` (was the collision concern). `grpc/mod.rs` moderate — check at dispatch. `db.rs` small addition, may reuse existing fn.
