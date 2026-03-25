# Plan 0443: Poison-expiry + keep_going=true hang — two DAG-removal paths miss derivation_hashes update

Bughunter correctness finding (two independent peer agents converged) at [`rio-scheduler/src/actor/worker.rs:864-873`](../../rio-scheduler/src/actor/worker.rs) (`tick_process_expired_poisons`) and [`rio-scheduler/src/actor/completion.rs:1243`](../../rio-scheduler/src/actor/completion.rs) (`handle_clear_poison`). Both paths call `dag.remove_node(drv_hash)` without updating `BuildInfo.derivation_hashes`. For `keep_going=true` builds this desyncs `build_summary`'s completion accounting: `total = derivation_hashes.len()` stays at the original count, but `completed + failed` can never reach it because the removed node is no longer counted in either bucket. `all_resolved` stays `false` forever; the build hangs `Active` until manual intervention.

**Trigger path A (TTL expiry):** Build A has `keep_going=true`, derivations D1+D2. D1 fails early, gets poisoned. D2 keeps running (keep_going). After `POISON_TTL` (24h), `tick_process_expired_poisons` removes D1 from the DAG. `build_summary` now counts D2's status but `derivation_hashes` still has {D1, D2} — `total=2`, `completed≤1`, hang.

**Trigger path B (admin clear):** Same setup, but admin issues `ClearPoison(D1)` while D2 is running. `handle_clear_poison` at [`completion.rs:1243`](../../rio-scheduler/src/actor/completion.rs) removes D1 from DAG. Same accounting desync.

The comment at [`completion.rs:1258`](../../rio-scheduler/src/actor/completion.rs) claims "Poisoned nodes have no interested builds (build already terminated)" — this is **false for `keep_going=true`**. [`recovery.rs:137-146`](../../rio-scheduler/src/actor/recovery.rs) documents a similar gap but only for the recovery path. Adjacent to [P0442](plan-0442-scheduler-lifecycle-six-bug-sweep.md) scope (which fixed the `keep_going=false` cancellation path) but a distinct code path.

## Entry criteria

- [P0442](plan-0442-scheduler-lifecycle-six-bug-sweep.md) merged (6-bug lifecycle sweep — shares `completion.rs` hunks; this plan builds on its `build_summary` accounting)

## Tasks

### T1 — `fix(scheduler):` tick_process_expired_poisons — remove from derivation_hashes on DAG node removal

At [`worker.rs:864-873`](../../rio-scheduler/src/actor/worker.rs): before `dag.remove_node(drv_hash)`, iterate `interested_builds` for the node. For each build with `keep_going=true`, remove `drv_hash` from `BuildInfo.derivation_hashes`. The `keep_going=false` case is already handled (build terminated when poison tripped). Update the `:1258` comment to read "Poisoned nodes have no interested `keep_going=false` builds; `keep_going=true` builds are pruned from `derivation_hashes` here."

### T2 — `fix(scheduler):` handle_clear_poison — same derivation_hashes prune

At [`completion.rs:1243`](../../rio-scheduler/src/actor/completion.rs): mirror T1's fix. Before `dag.remove_node`, prune `derivation_hashes` for every interested `keep_going=true` build. Consider extracting a `prune_interested_keep_going(&mut self, drv_hash)` helper shared by T1+T2 — both sites do the identical iterate-interested-builds-and-remove-from-hashes dance.

### T3 — `test(scheduler):` regression tests for both trigger paths

Add to [`rio-scheduler/src/actor/tests/lifecycle_sweep.rs`](../../rio-scheduler/src/actor/tests/lifecycle_sweep.rs):

- `test_poison_ttl_expiry_keep_going_completes` — build with `keep_going=true`, 2 derivations, poison D1, advance mock clock past `POISON_TTL`, tick, complete D2, assert build reaches terminal state (not hung `Active`).
- `test_admin_clear_poison_keep_going_completes` — same setup but admin-triggered `ClearPoison` instead of TTL expiry. Assert build completes.

Both tests MUST fail on pre-fix code (hang at `Active` with `total=2, completed=1`).

## Exit criteria

- `/nbr .#ci` green
- `cargo nextest run -p rio-scheduler test_poison_ttl_expiry_keep_going_completes test_admin_clear_poison_keep_going_completes` → 2 passed
- `grep 'keep_going=false builds\|keep_going=true builds are pruned' rio-scheduler/src/actor/completion.rs` → ≥1 hit (comment corrected)
- Both trigger paths (`tick_process_expired_poisons` + `handle_clear_poison`) prune `derivation_hashes` before `remove_node`

## Tracey

References existing markers:
- `r[sched.build.keep-going]` — T1+T2 fix the `keep_going=true` aggregation accounting; T3 verifies
- `r[sched.poison.ttl-persist]` — T1 fixes the TTL-expiry interaction with keep_going builds
- `r[sched.admin.clear-poison]` — T2 fixes the admin-clear interaction with keep_going builds

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "T1: :864-873 prune derivation_hashes for interested keep_going builds before remove_node"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T2: :1243 same prune; :1258 comment correction. HOT count=33"},
  {"path": "rio-scheduler/src/actor/tests/lifecycle_sweep.rs", "action": "MODIFY", "note": "T3: +2 regression tests (TTL expiry + admin clear, both keep_going=true)"}
]
```

```
rio-scheduler/src/actor/
├── worker.rs              # T1: tick_process_expired_poisons prune
├── completion.rs          # T2: handle_clear_poison prune + comment fix
└── tests/
    └── lifecycle_sweep.rs # T3: 2 regression tests
```

## Dependencies

```json deps
{"deps": [442], "soft_deps": [], "note": "P0442 is the 6-bug lifecycle sweep that touched the same build_summary accounting. This plan extends that fix to the poison-removal paths. Both files are HOT (completion.rs count=33, worker.rs count=31) — serialize after any concurrent completion.rs plan."}
```

**Depends on:** [P0442](plan-0442-scheduler-lifecycle-six-bug-sweep.md) — shares `build_summary` accounting logic and `lifecycle_sweep.rs` test module.
**Conflicts with:** [`completion.rs`](../../rio-scheduler/src/actor/completion.rs) count=33 (hottest scheduler file), [`worker.rs`](../../rio-scheduler/src/actor/worker.rs) count=31. T1 edits `:864-873`, T2 edits `:1243-1258` — both post-P0442 hunks. Serialize with any concurrent P0311 test-gap appends targeting `lifecycle_sweep.rs`.
