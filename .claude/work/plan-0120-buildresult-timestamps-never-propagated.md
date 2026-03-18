# Plan 0120: CRITICAL — BuildResult timestamps never propagated → EMA never fired

## Design

**The single most consequential bug fix of phase 3a.** Worker read `start_time`/`stop_time` off the wire from `nix-daemon`'s `BuildResult`, then set `start_time: None, stop_time: None` in the proto `CompletionReport`. Scheduler's `completion.rs` guards `update_build_history` on **both** being `Some` — it cannot compute `duration_secs` without them. When either is `None`, the **entire** `build_history` row write is silently skipped. Not just duration — **also** `peak_memory_bytes`, `peak_cpu_cores`, `output_size_bytes`. The EMA blend never ran. Every resource prediction the scheduler ever made was based on the hardcoded default or stale pre-seeded data.

The bug was in phase 2c code. vm-phase2c never caught it because it pre-seeded `build_history` via `psql INSERT` — it tested the read side (estimator consumes EMA) but never the write side (completion produces EMA). vm-phase3a's `psql SELECT ema_peak_memory_bytes FROM build_history WHERE ...` was the **first** end-to-end check of the real completion → DB chain. It returned NULL for a derivation that had just completed with a known memory profile.

The wire values were already being read correctly by `rio-nix` (`build.rs:118-120` — u64 Unix epoch seconds). They were simply dropped when constructing `ProtoBuildResult`. Two-line fix: propagate the fields. `0` → `None` — nix-daemon sends 0 on some error paths; a build at 1970-01-01 doesn't exist; safe sentinel.

`7c636fc` (folded here) updated `phase3a.md` with the bugs catalog including this one, which is where the full explanation lives.

This is the kind of bug that would have taken months to notice in production — the scheduler would silently make terrible scheduling decisions based on wrong data, builds would succeed, and nobody would connect the dots until someone asked "why does the scheduler always pick the smallest worker for GCC?" P0109 had fixed the phase2c VmHWM bug (measuring daemon RSS not builder memory); this bug meant that fix never mattered — the corrected measurement was never written anyway.

## Files

```json files
[
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "propagate BuildResult.start_time/stop_time to ProtoBuildResult; 0 → None sentinel"},
  {"path": "docs/src/phases/phase3a.md", "action": "MODIFY", "note": "Key Bugs Found catalog; this bug's entry is the longest — silent-data-loss class"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `r[worker.completion.propagate-timestamps]` — annotation on the fixed lines. The scheduler-side guard (`r[sched.history.duration-gate]`) was already correct; the bug was worker-side.

## Entry

- Depends on P0109: `peak_cpu_cores` was one of the fields silently dropped by this bug. P0109's work was invisible until this fix.
- Depends on P0096: the bug was **in** phase 2c code — `start_time: None` was written then and never tested.

## Exit

Merged as `211b010..7c636fc` (2 commits). `.#ci` green at merge. vm-phase3a's `psql SELECT ema_peak_memory_bytes` now returns a real value after a build completes. No unit test added — the failure mode was "silently works but writes nothing," which a unit test wouldn't have caught either; the VM test end-to-end check is the correct coverage.
