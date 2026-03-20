# Plan 0391: Warm-gate deterministic prefetch selection

[P0299](plan-0299-staggered-scheduling-cold-start.md) shipped `on_worker_registered` at [`worker.rs:99`](../../rio-scheduler/src/actor/worker.rs): collect input closures from up to 32 Ready derivations, dedup, cap at 100 paths, send as the initial PrefetchHint. Review found the path selection is **doubly non-deterministic**:

1. `dag.iter_nodes()` iterates `HashMap<DrvHash, DerivationState>` ([`dag/mod.rs:45`](../../rio-scheduler/src/dag/mod.rs)) — random order, so `.take(32)` picks arbitrary 32 Ready nodes
2. `HashSet<_>.into_iter().take(100)` also iterates in random order — the final 100-path cap discards different paths per process

The comment at `:97-98` says the intent is "broad common-set (glibc, stdenv)". Dedup helps (common paths survive the `HashSet` union), but when `unique_paths > 100` the final cap is random. A fresh scheduler restart with the same queue state will send a different hint set to the same worker. Prefetch hit-rate becomes unpredictable when `ready_queue > 32` or total unique closure paths exceed 100.

Not a correctness bug — [`r[sched.assign.warm-gate]`](../../docs/src/components/scheduler.md) at `:434` says the gate is "optimization-not-correctness" and the fallback path handles the all-cold case. But the optimization's effectiveness depends on how well the initial hint predicts subsequent assignments. Deterministic selection also makes the VM scenario at [`scheduling.nix:1234`](../../nix/tests/scenarios/scheduling.nix) reproducible (currently passive metrics-only per [P0295](plan-0295-doc-rot-batch-sweep.md)-T89).

## Entry criteria

- [P0299](plan-0299-staggered-scheduling-cold-start.md) merged (`on_worker_registered` + `WorkerState.warm` exist)

## Tasks

### T1 — `perf(scheduler):` sort Ready nodes by fan-in before cap

MODIFY [`rio-scheduler/src/actor/worker.rs`](../../rio-scheduler/src/actor/worker.rs) at `:107-113`. Replace `iter_nodes().filter().take(32)` with collect-sort-take:

```rust
// Sort Ready nodes by fan-in (interested_builds.len()) descending.
// Highest fan-in = most likely to be dispatched first (tie-breaker
// in best_worker). Their closures are the paths a warm cache wants
// preloaded. Deterministic: sort key is stable across restarts for
// the same queue state (same Ready set, same interested_builds).
//
// Tie-break on drv_hash ascending so identical-fanin nodes get a
// reproducible order — drv_hash is the DAG key, always unique.
let mut ready: Vec<(&str, &DerivationState)> = self
    .dag
    .iter_nodes()
    .filter(|(_, s)| s.status() == DerivationStatus::Ready)
    .collect();
ready.sort_by(|(ha, a), (hb, b)| {
    b.interested_builds
        .len()
        .cmp(&a.interested_builds.len())
        .then_with(|| ha.cmp(hb))
});
let ready_hashes: Vec<DrvHash> = ready
    .into_iter()
    .take(MAX_READY_TO_SCAN)
    .map(|(h, _)| DrvHash::from(h))
    .collect();
```

`interested_builds` is a `HashSet<Uuid>` at [`derivation.rs:206`](../../rio-scheduler/src/state/derivation.rs) — `.len()` is O(1). Sort is O(n log n) over Ready nodes. Acceptable: registration is once per worker per reconnect, not hot-path.

**Why fan-in not `submitted_at`:** `submitted_at` lives on `BuildState` at [`build.rs:127`](../../rio-scheduler/src/state/build.rs), not on `DerivationState`. A derivation with 5 interested builds was needed by 5 submissions — that's a stronger dispatch-priority signal than which single build submitted first. The existing `best_worker` scoring at [`assignment.rs`](../../rio-scheduler/src/assignment.rs) already favors high-fan-in (via `interested_builds.len()` in the priority calc); prefetching those derivations' closures aligns the warm cache with actual dispatch order.

### T2 — `perf(scheduler):` deterministic 100-path cap via frequency sort

MODIFY same function at `:114-120`. Replace `HashSet.into_iter().take(100)` with frequency-counted selection:

```rust
// Count how many of the scanned derivations want each path.
// High-count paths are the "broad common-set" the comment at :97
// describes — glibc/stdenv appearing in most closures. Sort by
// count descending (tie-break on path string for reproducibility),
// take the top 100.
use std::collections::HashMap;
let mut path_counts: HashMap<String, usize> = HashMap::new();
for h in &ready_hashes {
    for p in crate::assignment::approx_input_closure(&self.dag, h) {
        *path_counts.entry(p).or_default() += 1;
    }
}
let mut paths: Vec<(String, usize)> = path_counts.into_iter().collect();
paths.sort_by(|(pa, ca), (pb, cb)| cb.cmp(ca).then_with(|| pa.cmp(pb)));
let paths: Vec<String> = paths
    .into_iter()
    .take(MAX_PREFETCH_PATHS)
    .map(|(p, _)| p)
    .collect();
```

Alternative considered: `BTreeSet` instead of `HashSet` for deterministic `.into_iter()`. Rejected — it gives lexicographic order (`/nix/store/0...` first), which has no correlation with prefetch-value. Frequency-sort is the same O(n log n) and serves the "broad common-set" intent directly.

### T3 — `test(scheduler):` determinism regression test

MODIFY [`rio-scheduler/src/actor/tests/worker.rs`](../../rio-scheduler/src/actor/tests/worker.rs) — add near the existing warm-gate tests (grep for `warm_gate` around `:449`). Construct a DAG with >32 Ready nodes and >100 unique closure paths. Call `on_worker_registered` twice (two `Actor` instances with the same DAG state), assert the sent hint path-lists are identical:

```rust
// r[verify sched.assign.warm-gate]
// Determinism: same DAG state → same PrefetchHint contents.
// HashMap iteration is random; T1+T2's sort makes the hint
// reproducible. Pre-T1+T2 this test is flaky (passes ~1/N! of
// the time).
#[tokio::test]
async fn warm_gate_initial_hint_is_deterministic() {
    // ... build 40 Ready nodes with varied interested_builds.len()
    //     and distinct closures totalling >100 unique paths ...
    // ... spin up two actors with identical DAG, register a worker
    //     on each, capture PrefetchHint from stream_rx ...
    assert_eq!(hint_a.store_paths, hint_b.store_paths,
               "same DAG state must yield identical initial hint");
    // Also assert the first path has the highest expected count
    // (glibc-like high-fanin path seeded at idx 0).
    assert_eq!(hint_a.store_paths[0], "/nix/store/glibc-...",
               "highest-frequency path must be first (proves sort fired)");
}
```

The second assert is the proves-nothing guard: a test that only checks `hint_a == hint_b` would pass if both were empty or both selected the same arbitrary set by accident (unlikely but the explicit ordering assert closes it).

### T4 — `docs(scheduler):` update `r[sched.assign.warm-gate]` spec text for selection order

MODIFY [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) at `:434-440` (the `r[sched.assign.warm-gate]` paragraph added by P0299). Append one sentence after "Empty scheduler queue at registration time → warm flips true immediately":

```
Hint contents select up to 32 Ready derivations sorted by fan-in
(interested-builds count) descending, union their input closures,
sort by occurrence frequency descending, cap at 100 paths. The
selection is deterministic for a given queue state.
```

**No tracey bump:** the existing spec text didn't commit to any particular selection order, so this is additive clarification not behavior change. The `r[impl sched.assign.warm-gate]` annotation at `worker.rs:87` stays at the same site; T1+T2 rewrite the body under that annotation.

## Exit criteria

- `cargo nextest run -p rio-scheduler warm_gate_initial_hint_is_deterministic` → pass
- T3 mutation check: revert T1's `.sort_by()` to no-sort → determinism test fails (or flaky-fails) on `hint_a != hint_b`
- `grep 'interested_builds.len()' rio-scheduler/src/actor/worker.rs` → ≥1 hit in `on_worker_registered` body (fan-in sort present)
- `grep 'HashSet<_>.into_iter().take\|HashSet::<.*>::into_iter' rio-scheduler/src/actor/worker.rs` → 0 hits in `on_worker_registered` body (nondeterministic cap replaced)
- `grep 'sorted by fan-in\|deterministic for a given queue' docs/src/components/scheduler.md` → ≥1 hit in `r[sched.assign.warm-gate]` paragraph
- `/nbr .#ci` green

## Tracey

References existing markers:
- `r[sched.assign.warm-gate]` — T1+T2 refine the implementation (existing `r[impl]` at [`worker.rs:87`](../../rio-scheduler/src/actor/worker.rs)); T3 adds a fourth `r[verify]` site (joins the three at [`assignment.rs:571-638`](../../rio-scheduler/src/assignment.rs)); T4 extends the spec paragraph

No new markers. The selection-order detail is a refinement of existing warm-gate behavior, not a new contract.

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "T1+T2: on_worker_registered :107-120 — fan-in sort + frequency-count cap (replaces HashMap/HashSet random iteration)"},
  {"path": "rio-scheduler/src/actor/tests/worker.rs", "action": "MODIFY", "note": "T3: warm_gate_initial_hint_is_deterministic near :449"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T4: r[sched.assign.warm-gate] :434 +selection-order sentence"}
]
```

```
rio-scheduler/src/
├── actor/
│   ├── worker.rs            # T1+T2: :107-120 deterministic selection
│   └── tests/worker.rs      # T3: determinism test
docs/src/components/
└── scheduler.md             # T4: spec clarification
```

## Dependencies

```json deps
{"deps": [299], "soft_deps": [311, 295], "note": "discovered_from=299 (rev-p299 perf). P0299 (DONE) shipped on_worker_registered + warm flag + the HashMap-random selection this plan fixes. Soft-dep P0311-T58 (test-gap for on_worker_registered itself — three cases: merge-then-connect, connect-empty-queue, send-fails) — T3 here is orthogonal (determinism), P0311-T58 is coverage; both add tests to actor/tests/worker.rs, additive, non-overlapping test-fn names. Soft-dep P0295-T90 (plan-0299 EC :161 not-met doc note — VM scenario is passive-only). T1+T2 make the passive scenario's output more predictable but don't add the ordering-proof; that's still P0311-T58's scope. worker.rs is warm (count=27 per P0304 collisions table) — T1+T2 are a localized rewrite inside one fn body, no signature change."}
```

**Depends on:** [P0299](plan-0299-staggered-scheduling-cold-start.md) — `on_worker_registered` + `WorkerState.warm` + `PrefetchHint` proto message exist.

**Conflicts with:** [`worker.rs`](../../rio-scheduler/src/actor/worker.rs) count≈27 — [P0304](plan-0304-trivial-batch-p0222-harness.md)-T34 adds `emit_progress` calls at disconnect sites (different fn); [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T54 adds unit tests for `on_worker_registered` (tests/worker.rs, additive). T1+T2 touch `:107-120` only; low semantic-conflict risk. [`scheduler.md`](../../docs/src/components/scheduler.md) count=20 — T4 appends to `:434` warm-gate paragraph; [P0304](plan-0304-trivial-batch-p0222-harness.md)-T25 edits `:449-451` (timeout spec drift); non-overlapping.
