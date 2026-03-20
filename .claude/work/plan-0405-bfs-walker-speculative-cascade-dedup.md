# Plan 0405: BFS walker dedup — speculative_cascade_reachable

Three near-identical BFS frontier walks in rio-scheduler — consolidator-mc205. Two added by [P0252](plan-0252-ca-cutoff-propagate-skipped.md) in the same window; one predates:

| Walker | Location | Shape | Purpose |
|---|---|---|---|
| `cascade_cutoff` | [`dag/mod.rs:567-606`](../../rio-scheduler/src/dag/mod.rs) | `Vec<DrvHash>` frontier, `while pop`, `for eligible in find_cutoff_eligible(...)`, `push+transition` | Queued→Skipped propagation |
| `verify_cutoff_candidates` speculative prewalk | [`completion.rs:82-100`](../../rio-scheduler/src/actor/completion.rs) | Same frontier loop, local `provisional` set, `find_cutoff_eligible_speculative(...)` | Over-approximate candidate collection for batched store RPC |
| `cascade_dependency_failure` | [`completion.rs:1026-1072`](../../rio-scheduler/src/actor/completion.rs) | Same frontier loop, `get_parents(...)` expand, `await persist` per step | Queued→DependencyFailed propagation |

All three are ~25L of `Vec<DrvHash> frontier` → `while let Some(current) = frontier.pop()` → `for expanded in <expand_fn>(current)` → `if <gate>` → `push + record`. The first two share `MAX_CASCADE_DEPTH` (mis-named per [P0304](plan-0304-trivial-batch-p0222-harness.md)-T169). The third has no depth cap (walks to exhaustion, bounded by DAG size).

**Why now:** [P0254](plan-0254-ca-metrics-vm-demo.md) hooks [`completion.rs:443-485`](../../rio-scheduler/src/actor/completion.rs); [P0397](plan-0397-ca-contentlookup-self-match-exclude.md) adds `exclude_store_path` at the same hook. A 4th walker is probable. Extracting a shared `speculative_cascade_reachable` before 4th-walk-added avoids a fourth copy.

**Why deferred until P0254+P0397 merge:** both have in-flight worktrees touching [`completion.rs`](../../rio-scheduler/src/actor/completion.rs). Extracting the walker mid-flight creates a 3-way conflict.

## Entry criteria

- [P0254](plan-0254-ca-metrics-vm-demo.md) merged ([`completion.rs`](../../rio-scheduler/src/actor/completion.rs) stable at CA-compare hook)
- [P0397](plan-0397-ca-contentlookup-self-match-exclude.md) merged (`exclude_store_path` wiring stable)

## Tasks

### T1 — `refactor(scheduler):` extract speculative_cascade_reachable

NEW `DerivationDag::speculative_cascade_reachable` in [`dag/mod.rs`](../../rio-scheduler/src/dag/mod.rs) near `cascade_cutoff` (`:567`):

```rust
/// Generic BFS walk collecting all nodes reachable via `expand(current)`
/// from `trigger`, capped at `max_nodes` pops. Pure, non-mut — the caller
/// decides what to do with the result (transition, store-verify, persist).
///
/// Used by:
/// - [`Self::cascade_cutoff`] — Queued→Skipped propagation
/// - [`crate::actor::Actor::verify_cutoff_candidates`] — over-approximate
///   candidate collection for batched FindMissingPaths RPC
///
/// The `expand` closure receives `(current, &visited_so_far)` so it can
/// implement speculative-skipped semantics (see
/// [`Self::find_cutoff_eligible_speculative`] — treats provisional-visited
/// as already-Skipped).
///
/// Returns `(reachable, cap_hit)`. Deduplication via `visited` HashSet —
/// diamond DAGs are safe.
pub fn speculative_cascade_reachable<F>(
    &self,
    trigger: &DrvHash,
    max_nodes: usize,
    mut expand: F,
) -> (Vec<DrvHash>, bool)
where
    F: FnMut(&DrvHash, &HashSet<DrvHash>) -> Vec<DrvHash>,
{
    let mut reachable = Vec::new();
    let mut visited: HashSet<DrvHash> = HashSet::new();
    let mut frontier = vec![trigger.clone()];
    let mut pops = 0usize;
    let mut cap_hit = false;
    while let Some(current) = frontier.pop() {
        if pops >= max_nodes {
            cap_hit = true;
            break;
        }
        for next in expand(&current, &visited) {
            if visited.insert(next.clone()) {
                reachable.push(next.clone());
                frontier.push(next);
            }
        }
        pops += 1;
    }
    (reachable, cap_hit)
}
```

### T2 — `refactor(scheduler):` migrate cascade_cutoff + verify_cutoff_candidates

MODIFY [`dag/mod.rs`](../../rio-scheduler/src/dag/mod.rs) `cascade_cutoff` at `:567-606`. Reimplement as:

```rust
pub fn cascade_cutoff(
    &mut self,
    trigger: &str,
    mut verify: impl FnMut(&DrvHash) -> bool,
) -> (Vec<DrvHash>, bool) {
    let Some(start) = self.canonical(trigger) else {
        return (Vec::new(), false);
    };
    // Walk collects all potentially-skippable nodes; transition happens
    // in a second pass so `self` stays `&` during expand and `&mut` only
    // for the bulk transition.
    let (candidates, cap_hit) = self.speculative_cascade_reachable(
        &start,
        MAX_CASCADE_NODES,  // post-P0304-T169 rename
        |current, visited| {
            self.find_cutoff_eligible_speculative(current, visited)
                .into_iter()
                .filter(|h| verify(h))
                .collect()
        },
    );
    let mut skipped = Vec::with_capacity(candidates.len());
    for hash in candidates {
        if let Some(state) = self.nodes.get_mut(&hash) {
            if state.transition(DerivationStatus::Skipped).is_ok() {
                skipped.push(hash);
            }
        }
    }
    (skipped, cap_hit)
}
```

**Borrow-checker note:** the closure captures `self` (`&Self`) via `find_cutoff_eligible_speculative`, and the outer `speculative_cascade_reachable` is also a `&self` method. The second-pass `&mut self.nodes` happens AFTER the walk returns — no borrow overlap. If the closure needs `&self` while the method is also `&self`, split `speculative_cascade_reachable` into a free fn taking `expand` only, not `&self` — check at dispatch.

MODIFY [`completion.rs`](../../rio-scheduler/src/actor/completion.rs) `verify_cutoff_candidates` at `:70-100`. The speculative-BFS body at `:82-100` collapses to:

```rust
let (candidates, _cap) = self.dag.speculative_cascade_reachable(
    trigger,
    crate::dag::MAX_CASCADE_NODES,
    |current, provisional| {
        self.dag.find_cutoff_eligible_speculative(current, provisional)
    },
);
if candidates.is_empty() {
    return HashSet::new();
}
```

~20L → ~8L in `verify_cutoff_candidates`.

### T3 — `refactor(scheduler):` cascade_dependency_failure — visitor closure (conditional)

[`completion.rs:1026-1072`](../../rio-scheduler/src/actor/completion.rs) `cascade_dependency_failure` is **async** (per-step `persist_status().await`) and walks parents via `get_parents()` not `find_cutoff_eligible`. The persist-per-step means it can't use a pure-collect-then-process shape without batching the persists.

Two routes:
- **(a)** Leave as-is. The async + different-expand-direction makes it the odd one out; T1's extraction serves the two sync cutoff walkers.
- **(b)** Split into two phases: sync-collect via `speculative_cascade_reachable` (with `expand = |h, _| self.dag.get_parents(h)`), then async bulk-persist in a second pass. Changes persist semantics from per-step to batch-at-end — **verify this is safe** (if a mid-cascade crash leaves some Queued after recovery, is the cascade re-triggerable? `handle_derivation_failure` at `:1074` suggests yes — recovery re-cascades from the original poisoned leaf).

**Prefer (a) unless P0254/P0397 add a 4th async walker** that would benefit from a shared async visitor. Document the non-migration in a comment at `:1026`: "// Same BFS-frontier shape as speculative_cascade_reachable but async (persist per step); not migrated — see P0405-T3."

## Exit criteria

- `/nbr .#ci` green
- `grep -c 'while let Some.*frontier.pop()' rio-scheduler/src/dag/mod.rs rio-scheduler/src/actor/completion.rs` → ≤2 (T1's extracted helper + T3-if-route-(a) `cascade_dependency_failure`; pre-fix was 3)
- `grep 'speculative_cascade_reachable' rio-scheduler/src/dag/mod.rs rio-scheduler/src/actor/completion.rs` → ≥3 hits (def + 2 callers)
- `cargo nextest run -p rio-scheduler dag:: actor::tests::completion` — all pass (cascade behavior preserved)
- **Conditional — if [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T67 landed:** `cargo nextest run -p rio-scheduler cascade_only_skips_verified_candidates` — T67's mutation-kill test still passes post-refactor (proves `|h| verified.contains(h)` wiring survives extraction). Check at dispatch: `grep 'cascade_only_skips_verified_candidates' rio-scheduler/src/actor/tests/completion.rs` → if 0 hits, P0311-T67 not yet landed; skip this criterion (the existing `dag::` + `actor::tests::completion` suite covers behavior preservation)

## Tracey

References existing markers:
- `r[sched.ca.cutoff-propagate]` — T2 refines the implementation (no behavior change)
- `r[sched.preempt.never-running]` — T3's `cascade_dependency_failure` carries this `r[impl]` annotation at `:1044`; if T3 route-(b) taken, annotation stays on the transition-gate line

No new markers. Pure refactor — BFS loop extraction, no spec-visible behavior change.

## Files

```json files
[
  {"path": "rio-scheduler/src/dag/mod.rs", "action": "MODIFY", "note": "T1: +speculative_cascade_reachable near :567. T2: cascade_cutoff :567-606 body → collect-via-helper + bulk-transition second pass"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T2: verify_cutoff_candidates :82-100 speculative-BFS → helper call (~20L→~8L). T3: :1026 comment documenting non-migration (route-a) OR split collect+batch-persist (route-b)"}
]
```

```
rio-scheduler/src/
├── dag/mod.rs                 # T1+T2: speculative_cascade_reachable + cascade_cutoff migrate
└── actor/completion.rs        # T2: verify_cutoff_candidates migrate; T3: dep-failure (conditional)
```

## Dependencies

```json deps
{"deps": [254, 397], "soft_deps": [252, 399, 304, 311], "note": "HARD-GATE on P0254+P0397 merge — both in-flight touching completion.rs at time of writing (consolidator-mc205). Extracting mid-flight creates 3-way conflict. Soft-dep P0252 (DONE — cascade_cutoff exists). Soft-dep P0399 (all_deps_completed Skipped fix — touches dag/mod.rs:411, non-overlapping with T1's :567 extraction). Soft-dep P0304-T169 (MAX_CASCADE_DEPTH→NODES rename — T1 uses the renamed const; if T169 not landed, T1 uses old name). Soft-dep P0311-T67/T68 (cascade tests — T2's behavior-preserving claim is proven by these tests still passing). completion.rs HOT count=30; dag/mod.rs count~15. T2's changes are localized to two fn bodies, no signature changes."}
```

**Depends on:** [P0254](plan-0254-ca-metrics-vm-demo.md) + [P0397](plan-0397-ca-contentlookup-self-match-exclude.md) — both land completion.rs changes first. Extract after their conflicts resolve.
**Conflicts with:** [`completion.rs`](../../rio-scheduler/src/actor/completion.rs) count=30 (HOT) — P0304-T148/T157 edit `:295-334` (ca_hash_compares labels), [P0399](plan-0399-all-deps-completed-skipped-hang.md)-T2 edits `:459` (find_newly_ready). T2 here edits `:82-100`; all non-overlapping hunks. [`dag/mod.rs`](../../rio-scheduler/src/dag/mod.rs) — P0304-T169 edits `:14-21` (const rename), P0399-T1 edits `:411-421`. T1 here adds a new fn near `:567`; T2 rewrites `:567-606` body. Non-overlapping.
