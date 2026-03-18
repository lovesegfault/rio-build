# Plan 0265: Worker ResourceUsage mid-build emission (proactive ema — USER Q1 reframe)

**USER Q1 reframe: NO kill, NO preemption marker, NO ADR.** `r[sched.preempt.never-running]` stands unbumped. This plan + [P0266](plan-0266-scheduler-ema-update-midbuild.md) do **proactive ema only** — scheduler updates `ema_peak_memory_bytes` BEFORE completion so the NEXT submit of that drv is right-sized immediately, instead of after a full OOM→retry cycle.

Per GT7: proto already done. [`types.proto:401`](../../rio-proto/proto/types.proto) has `ResourceUsage`; `:338` has `ProgressUpdate` with `resources` field at `:340`+`:352`. `ResourceUsage` already plumbed through `Heartbeat` via [`cgroup.rs:573 to_proto()`](../../rio-worker/src/cgroup.rs). **Zero proto touch** — this is worker-side emission cadence only.

## Entry criteria

- [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) merged (frontier root)

## Tasks

### T1 — `feat(worker):` emit ProgressUpdate.resources during build

MODIFY [`rio-worker/src/runtime.rs`](../../rio-worker/src/runtime.rs) — in the build-execution loop, add a 10s tick that samples cgroup stats and sends `ProgressUpdate { resources: Some(cgroup_sample) }`:

```rust
// USER Q1 reframe: proactive ema. NO kill. This just gives the scheduler
// mid-build peak-memory samples so the NEXT submit is right-sized
// before the current build even finishes (or OOMs).
let mut resource_tick = tokio::time::interval(Duration::from_secs(10));
loop {
    tokio::select! {
        _ = resource_tick.tick() => {
            let usage = cgroup_tracker.sample();  // cgroup.rs:573 to_proto()
            progress_tx.send(ProgressUpdate {
                resources: Some(usage),
                ..Default::default()
            }).await?;
        }
        result = &mut build_future => break result,
    }
}
```

### T2 — `test(worker):` emission cadence

```rust
#[tokio::test(start_paused = true)]
async fn resource_usage_emitted_every_10s() {
    // Mock build that runs 35s. Advance clock in 10s steps.
    // Assert 3 ProgressUpdate messages with resources populated.
}
```

## Exit criteria

- `/nbr .#ci` green
- No proto changes (`git diff rio-proto/` → empty)

## Tracey

none — this is proactive-ema plumbing. Per USER Q1: no `sched.preempt.oom-migrate` marker exists or is seeded. `r[sched.preempt.never-running]` stands.

## Files

```json files
[
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "T1: 10s ProgressUpdate.resources tick (GT7: proto already has field)"}
]
```

```
rio-worker/src/
└── runtime.rs                    # T1: 10s emission tick
```

## Dependencies

```json deps
{"deps": [245], "soft_deps": [], "note": "USER Q1 REFRAME: proactive ema ONLY. No kill. No marker. No ADR (P0246 dropped). Zero proto touch per GT7. runtime.rs low collision."}
```

**Depends on:** [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) — frontier root.
**Conflicts with:** `runtime.rs` low collision — verify at dispatch.
