# Plan 995943541: all_deps_completed accepts Skipped — HANG FIX (URGENT)

**URGENT — P0252 is live on sprint-1.** rev-p252 correctness finding: [`dag/mod.rs:411`](../../rio-scheduler/src/dag/mod.rs) checks `== Completed` only. `Skipped` is terminal and output-equivalent (the output exists in the store — that's the CA-cutoff precondition), but a node whose only incomplete dependency is `Skipped` will never be promoted out of `Queued`.

Two reachable hang scenarios:

- **H1 (cascade verify-rejected):** `verify_cutoff_candidates` at [`completion.rs:70-99`](../../rio-scheduler/src/actor/completion.rs) walks BFS with a `verified` set — nodes whose outputs are NOT present in the store are rejected from the cascade (first-build guard). Those rejected candidates stay `Queued`. Their deps are `Completed|Skipped` (terminal). `find_newly_ready(trigger)` at [`completion.rs:732`](../../rio-scheduler/src/actor/completion.rs) only walks parents of the ORIGINAL trigger, not parents of each newly-`Skipped` node — so the rejected candidates are never promoted to `Ready`.
- **H2 (merge onto pre-existing Skipped):** build-2 merges a new node X depending on pre-existing `Skipped` Y. [`compute_initial_states`](../../rio-scheduler/src/dag/mod.rs) at `:704` calls `all_deps_completed(X)` → Y is `Skipped ≠ Completed` → X goes `Queued`. Y is terminal; no event will ever call `find_newly_ready(Y)` for X.

P0252-T1 said "WATCH FOR `_ =>` wildcards" — `== Completed` sites are the SAME hazard class (they're an implicit wildcard that maps every non-`Completed` status to false).

## Entry criteria

- [P0252](plan-0252-ca-cutoff-propagate-skipped.md) merged (Skipped variant exists) — **DONE**

## Tasks

### T1 — `fix(scheduler):` all_deps_completed — accept Completed|Skipped

MODIFY [`rio-scheduler/src/dag/mod.rs`](../../rio-scheduler/src/dag/mod.rs) at `:411-421`:

```rust
/// Check whether all dependencies of a derivation are satisfied
/// (output available). `Completed` and `Skipped` are both acceptable:
/// Skipped means CA cutoff verified the output exists in the store.
pub fn all_deps_completed(&self, drv_hash: &str) -> bool {
    let Some(children) = self.children.get(drv_hash) else {
        return true;
    };
    children.iter().all(|child_hash| {
        self.nodes.get(child_hash).is_some_and(|n| {
            matches!(n.status(), DerivationStatus::Completed | DerivationStatus::Skipped)
        })
    })
}
```

This fixes H2 directly (`compute_initial_states` now sees Skipped deps as satisfied) AND fixes `find_newly_ready` at `:466-478` (it delegates to `all_deps_completed`).

**Why not `is_terminal()`:** `Poisoned`/`DependencyFailed`/`Cancelled` are terminal but their outputs do NOT exist in the store. A node depending on those must NOT go `Ready` (that path is covered by `any_dep_terminally_failed` → `DependencyFailed`). Only `Completed|Skipped` mean "output is available".

### T2 — `fix(scheduler):` cascade — Ready-promote verify-rejected parents of Skipped

MODIFY [`rio-scheduler/src/dag/mod.rs`](../../rio-scheduler/src/dag/mod.rs) inside `cascade_cutoff` (the iterative cascade at `:550+`). After each `Skipped` transition, the newly-Skipped node's parents may now be `Ready` (if all their other deps were already terminal). The current cascade ONLY walks cutoff-ELIGIBLE parents (Queued + all-terminal → Skipped); it does not Ready-promote non-eligible Queued parents.

Two options:
- **(a) Return skipped-set, caller calls find_newly_ready per-Skipped:** `cascade_cutoff` returns the set of nodes it transitioned to `Skipped`. Caller at [`completion.rs:459-467`](../../rio-scheduler/src/actor/completion.rs) loops `for s in skipped { for r in dag.find_newly_ready(&s) { ... Ready } }`. Same pattern as the existing `find_newly_ready(drv_hash)` at `:732`.
- **(b) Cascade also Ready-promotes:** inside the cascade loop, after each `Skipped` transition, also call `find_newly_ready` and transition those to `Ready` (they don't re-enter the cascade frontier — Ready nodes aren't cutoff-eligible).

Prefer **(a)** — keeps `cascade_cutoff` single-responsibility (Skipped transitions only), and the caller already has the `find_newly_ready + transition-to-Ready` pattern at `:732`. Option (a) also naturally handles the H1 rejected-candidate case: a rejected candidate is a `Queued` parent-of-Skipped that wasn't itself Skipped; `find_newly_ready(skipped_node)` will return it (T1 makes `all_deps_completed` accept `Skipped`).

```rust
// in cascade_cutoff: collect into a return vec
let mut skipped = Vec::new();
// ... existing loop ...
    self.transition(&eligible, DerivationStatus::Skipped)?;
    skipped.push(eligible.clone());
    // ... existing frontier.push ...
// ...
Ok(skipped)
```

Caller at `completion.rs`:

```rust
let skipped = self.dag.cascade_cutoff(drv_hash, |h| verified.contains(h))?;
// H1 fix: each newly-Skipped node may have Queued parents whose
// all-deps are now Completed|Skipped. Those parents need Ready
// promotion. (The :732 find_newly_ready only walks from drv_hash,
// not from each Skipped — without this loop, they'd hang Queued.)
for s in &skipped {
    for r in self.dag.find_newly_ready(s) {
        self.dag.transition(&r, DerivationStatus::Ready)?;
    }
}
```

**Grep `cascade_cutoff` signature at dispatch** — it may already return the skipped set (e.g., for the `ca_cutoff_saves_total` metric). If so, T2 is caller-side only.

### T3 — `test(scheduler):` H1 regression — verify-rejected parent of Skipped goes Ready

MODIFY [`rio-scheduler/src/dag/tests.rs`](../../rio-scheduler/src/dag/tests.rs) (or `actor/tests/completion.rs` if integration-level). Scenario:

```rust
// r[verify sched.ca.cutoff-propagate]
#[test]
fn cascade_rejected_parent_promoted_not_stuck() {
    // Chain A→B→C all Queued. A completes ca_output_unchanged=true.
    // Cascade verify: B's output IS in store (verified.contains(B) = true)
    //               → B goes Skipped.
    //               C's output NOT in store (verified.contains(C) = false)
    //               → C REJECTED from cascade, stays Queued.
    // Pre-fix: C stuck Queued forever (find_newly_ready(A) doesn't reach
    //          C; B is Skipped so no event promotes C).
    // Post-fix: C promoted to Ready (find_newly_ready(B) sees
    //           C's only dep B is Skipped, T1's matches! accepts it).
    // ...
    assert_eq!(dag.node(&c).unwrap().status(), DerivationStatus::Ready);
}
```

### T4 — `test(scheduler):` H2 regression — merge onto pre-existing Skipped

MODIFY [`rio-scheduler/src/dag/tests.rs`](../../rio-scheduler/src/dag/tests.rs). Direct `compute_initial_states` test:

```rust
// r[verify sched.merge.dedup]
#[test]
fn merge_new_node_depending_on_skipped_goes_ready() {
    // Pre-existing DAG: Y is Skipped (from a prior cascade).
    // Merge: new node X depending on Y.
    // Pre-fix: compute_initial_states(X) → all_deps_completed(X)=false
    //          (Y != Completed) → X goes Queued, stuck forever.
    // Post-fix: all_deps_completed(X)=true (Y matches Completed|Skipped)
    //           → X goes Ready.
    let mut dag = DerivationDag::new();
    dag.insert(y_node);
    dag.transition(&y, DerivationStatus::Skipped).unwrap();
    let transitions = dag.compute_initial_states(&hashset![x]);
    // X should be Ready, not Queued
    assert!(transitions.iter().any(|(h, s)|
        h == &x && *s == DerivationStatus::Ready));
}
```

### T5 — `test(scheduler):` all_deps_completed rejects Poisoned/DependencyFailed (negative guard)

MODIFY [`rio-scheduler/src/dag/tests.rs`](../../rio-scheduler/src/dag/tests.rs). Ensures T1 didn't over-widen to `is_terminal()`:

```rust
#[test]
fn all_deps_completed_rejects_failure_terminal() {
    // Y is Poisoned (terminal but output unavailable).
    // all_deps_completed(X) must be FALSE — X should NOT go Ready
    // (it cascades DependencyFailed via any_dep_terminally_failed).
    assert!(!dag.all_deps_completed(&x));
}
```

## Exit criteria

- `/nbr .#ci` green
- `grep 'matches!.*Completed.*Skipped' rio-scheduler/src/dag/mod.rs` → ≥1 hit in `all_deps_completed` body (T1)
- `grep '== DerivationStatus::Completed' rio-scheduler/src/dag/mod.rs` → 0 hits in `all_deps_completed` body (T1: old check removed; other sites may legitimately compare to Completed only)
- T3 + T4 pass; both FAIL against pre-fix `all_deps_completed` (mutation check: revert T1 → T3 and T4 both fail)
- T5 passes (negative guard — `Poisoned` dep → `all_deps_completed` false)
- `cargo nextest run -p rio-scheduler dag::tests` → all pass including P0252's existing `ca_cutoff_cascades_through_chain` + `ca_cutoff_skips_running` + `ca_cutoff_depth_cap`

## Tracey

References existing markers:
- `r[sched.ca.cutoff-propagate]` — T1+T2 implement (the "transition downstream derivations … from Queued to Skipped" clause at [`scheduler.md:264`](../../docs/src/components/scheduler.md) implies parents-of-Skipped are then eligible for Ready). T3 verifies.
- `r[sched.merge.dedup]` — T4 verifies (merge onto pre-existing Skipped — the dedup clause at [`scheduler.md:152`](../../docs/src/components/scheduler.md) means X shares Y's node; T4 asserts X computes Ready not stuck-Queued).
- `r[sched.state.transitions]` — T1 refines (Skipped as "dep-output-available" for promotion purposes per [`scheduler.md:308`](../../docs/src/components/scheduler.md)).

No new markers — the existing `cutoff-propagate` marker's spec text already covers this (Skipped is terminal-with-output; the bug is the `== Completed` implementation gap, not a spec gap).

## Files

```json files
[
  {"path": "rio-scheduler/src/dag/mod.rs", "action": "MODIFY", "note": "T1: all_deps_completed :411-421 → matches!(Completed|Skipped). T2: cascade_cutoff returns Vec<DrvHash> of skipped (may already — grep at dispatch)"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T2: after cascade, loop find_newly_ready per-Skipped → Ready-promote (H1 fix) near :459-467"},
  {"path": "rio-scheduler/src/dag/tests.rs", "action": "MODIFY", "note": "T3: cascade_rejected_parent_promoted_not_stuck. T4: merge_new_node_depending_on_skipped_goes_ready. T5: all_deps_completed_rejects_failure_terminal"}
]
```

```
rio-scheduler/src/
├── dag/mod.rs              # T1: matches! (+T2 if signature change)
├── actor/completion.rs     # T2: find_newly_ready per-Skipped loop
└── dag/tests.rs            # T3+T4+T5: hang regression + negative guard
```

## Dependencies

```json deps
{"deps": [252], "soft_deps": [254, 397], "note": "URGENT — P0252 live on sprint-1, both hang scenarios reachable. H1 fires on any cascade where verify rejects ≥1 candidate (first-build of any non-trivial CA chain). H2 fires on any second-build that merges new work onto a Skipped graph. T1 is a 3-token swap (== Completed → matches!). T2 is ~6 lines of loop at the existing :459 call site. dag/mod.rs count~8, completion.rs count=29 HOT — T2's edit is at :459-467 (the cascade call site, not the :289-337 CA-compare hot zone)."}
```

**Depends on:** [P0252](plan-0252-ca-cutoff-propagate-skipped.md) — `Skipped` variant + `cascade_cutoff` exist (DONE).
**Soft-dep:** [P0254](plan-0254-ca-metrics-vm-demo.md) — the VM demo submits a CA chain twice; without this fix, the SECOND submit (the one asserting `cutoff_saves_total ≥ 2`) may hang on an H1-rejected node if the chain's first build had partial-store presence. P0254 should dispatch AFTER this. [P0397](plan-0397-ca-contentlookup-self-match-exclude.md) — also CA-cutoff correctness; non-overlapping (self-match is store-RPC side, this is DAG-promotion side).
**Conflicts with:** [`completion.rs`](../../rio-scheduler/src/actor/completion.rs) count=29 HOT — T2 at `:459-467` (cascade call site). P0304-T148/T157 at `:289-337` (CA-compare hook) — different section. [P0254-T7](plan-0254-ca-metrics-vm-demo.md) adds `insert_realisation_deps` call post-realisation-register — different section. [`dag/mod.rs`](../../rio-scheduler/src/dag/mod.rs) — [P0304](plan-0304-trivial-batch-p0222-harness.md)-T168 renames `MAX_CASCADE_DEPTH` at `:18`; T1 here at `:411-421` — non-overlapping. P0311-T67/T68 add tests to `dag/tests.rs` — additive.
