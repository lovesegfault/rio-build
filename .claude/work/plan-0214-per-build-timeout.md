# Plan 0214: scheduler — per-build overall timeout in `handle_tick`

Wave 2. Implements the **documented intent** of `BuildOptions.build_timeout`: proto doc-comment at [`types.proto:315`](../../rio-proto/proto/types.proto) says "Max **total** build time in seconds." Today that field is only used as a per-derivation daemon floor at [`actor/build.rs:567`](../../rio-scheduler/src/actor/build.rs) (`min_nonzero` forwarded to worker). This plan adds the primary semantics: wall-clock limit on the entire build from submission.

**R-TOUT resolved (A3 accepted):** this is NOT a bug. The per-derivation use at `:567` is incidental defense-in-depth — the proto field's intent was ALWAYS per-build-total. Both uses coexist: scheduler enforces total wall-clock (this plan), worker enforces per-derivation daemon floor (stays). Document both in the code comment.

Distinct from `r[sched.backstop.timeout]` at `:464` (per-derivation heuristic: est×3) — that's already implemented and this doesn't touch it.

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync-markers.md) merged (marker `r[sched.timeout.per-build]` exists in `scheduler.md`)

## Tasks

### T1 — `feat(scheduler):` per-build timeout check in `handle_tick`

At [`rio-scheduler/src/actor/worker.rs:440`](../../rio-scheduler/src/actor/worker.rs), AFTER the existing per-derivation backstop loop (`:464`, `r[sched.backstop.timeout]`):

```rust
// r[impl sched.timeout.per-build]
//
// Wall-clock limit on the ENTIRE build from submission. Distinct from:
//   - r[sched.backstop.timeout] above (per-derivation heuristic: est×3)
//   - worker-side daemon floor at actor/build.rs:567 (also receives
//     build_timeout as min_nonzero per-derivation — defense-in-depth)
//
// Zero = no overall timeout.
let mut timed_out_builds = Vec::new();
for (build_id, build) in &self.builds {
    if build.options.build_timeout > 0
        && build.submitted_at.elapsed().as_secs() > build.options.build_timeout as u64
    {
        timed_out_builds.push(*build_id);
    }
}
for build_id in timed_out_builds {
    // Cancel non-terminal derivations (reuse path from CancelBuild handler)
    self.cancel_build_derivations(build_id);
    self.transition_build(build_id, BuildOutcome::Failed {
        status: BuildResultStatus::TimedOut,
        reason: format!(
            "build_timeout {}s exceeded (wall-clock since submission)",
            self.builds[&build_id].options.build_timeout
        ),
    });
}
```

`BUILD_RESULT_STATUS_TIMED_OUT = 12` at [`types.proto:278`](../../rio-proto/proto/types.proto) — "Scheduler treats as permanent-no-reassign" (A4 confirmed).

### T2 — `test(scheduler):` paused-time timeout unit test

With `tokio::time::pause`: submit build with `build_timeout=60`, advance time by 61s, call `handle_tick` → assert build transitioned to `Failed { status: TimedOut }` with reason containing "build_timeout 60s exceeded".

Also verify: advance to 59s, tick → NOT failed (boundary check).

Marker: `// r[verify sched.timeout.per-build]`

### T3 — `test(vm):` scheduling.nix — real timeout

Extend [`nix/tests/scenarios/scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) (TAIL append): submit with `--option timeout 10`, derivation is `sleep 60` → assert `Failed` with `TimedOut` status within ~15s wall-clock (well before 60s).

### T4 — `docs(errors):` flip row 97 to "Implemented"

At [`docs/src/errors.md:97`](../../docs/src/errors.md) (the per-build-timeout row): "Not implemented (Phase 4)" → "Implemented."

## Exit criteria

- `/nbr .#ci` green
- Paused-time test: `Failed { TimedOut }` at timeout+1s, NOT failed at timeout-1s
- Existing backstop test at `:464` (`r[sched.backstop.timeout]`) still passes unchanged — proves independence

## Tracey

References existing markers:
- `r[sched.timeout.per-build]` — T1 implements (handle_tick check); T2 verifies (paused-time test)

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "T1: per-build timeout loop after :464 backstop; r[impl sched.timeout.per-build]; T2: paused-time test; r[verify]"},
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "T3: timeout subtest (TAIL append): --option timeout 10 + sleep 60 → TimedOut ~15s"},
  {"path": "docs/src/errors.md", "action": "MODIFY", "note": "T4: row 97 → Implemented"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "note dual-use semantics under r[sched.timeout.per-build] if P0204 text insufficient"}
]
```

```
rio-scheduler/src/actor/worker.rs  # T1+T2: check + test
nix/tests/scenarios/scheduling.nix # T3: VM test
docs/src/errors.md                 # T4: row 97
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [], "note": "No deps beyond P0204. Merge BEFORE P0211 (both touch actor/worker.rs — P0214 has no deps so goes first)."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync-markers.md) — marker `r[sched.timeout.per-build]` must exist.

**Conflicts with:**
- [`actor/worker.rs`](../../rio-scheduler/src/actor/worker.rs) — **P0211** touches `handle_heartbeat` at `:290`; T1 touches `handle_tick` at `:440`. 150 lines apart, different functions. **Merge P0214 first** (this has no deps, P0211 needs P0205) → P0211 rebases trivially.
- [`scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) — **P0215** also TAIL-appends a subtest. Both are TAIL-append, low risk — serialize merge if same-day.
- [`errors.md`](../../docs/src/errors.md) — P0215 touches row 96, this touches row 97. Adjacent but distinct lines — auto-merge.
