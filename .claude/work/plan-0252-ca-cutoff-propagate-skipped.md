# Plan 0252: CA cutoff PROPAGATE — Skipped variant + dag cascade

[P0251](plan-0251-ca-cutoff-compare.md) recorded `ca_output_unchanged`. This plan acts on it: transition downstream `Queued` derivations to terminal-skipped without running them. The transition cascades — if skipping B also satisfies C's dependencies, C skips too.

**USER Q2:** the new terminal variant is `DerivationStatus::Skipped`, NOT `CompletedViaCutoff`. Shorter, matches existing enum style. Metric stays `rio_scheduler_ca_cutoff_saves_total` (counts `Queued→Skipped` transitions).

**R1 (state-machine ripple):** adding an enum variant requires updating EVERY `match DerivationStatus` site. Rust exhaustiveness catches missing arms, BUT `_ =>` wildcards silently swallow the new variant. **DISPATCH SOLO** — no concurrent `actor/` plans.

**Running builds are NEVER killed.** Per `r[sched.preempt.never-running]`, cutoff applies to `Queued` only. If A→B, A completes with match while B is already running — B finishes. Wastes CPU but produces the right output.

## Entry criteria

- [P0251](plan-0251-ca-cutoff-compare.md) merged (`ca_output_unchanged` set on completion)

## Tasks

### T1 — `feat(scheduler):` add Skipped variant + audit every match site

MODIFY [`rio-scheduler/src/state/derivation.rs`](../../rio-scheduler/src/state/derivation.rs):

```rust
pub enum DerivationStatus {
    // ... existing variants ...
    /// Terminal. CA early-cutoff: output hash matched content index,
    /// downstream work skipped. Distinct from Completed for metrics
    /// (rio_scheduler_ca_cutoff_saves_total) and audit trail.
    /// Queued|Ready → Skipped (audit C #24: matches DependencyFailed
    /// precedent completion.rs:678-680; order-independent vs find_newly_ready).
    Skipped,
}

impl DerivationStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Poisoned
                     | Self::DependencyFailed | Self::Skipped)
    }
}

// In validate_transition():
(Queued | Ready, Skipped) => Ok(()),  // CA cutoff — audit C #24: Ready too (cascade order-independent)
```

**AUDIT:** `rg -n 'DerivationStatus::' rio-scheduler/src/` — record count. Each `match` site needs a decision:
- Display/serialization: add `Skipped => "skipped"`
- Terminal-state checks: handled by `is_terminal()` if the site uses it; otherwise add explicit arm
- **WATCH FOR `_ =>` wildcards** — they compile but may do the wrong thing for `Skipped`

### T2 — `feat(scheduler):` find_cutoff_eligible walker

MODIFY [`rio-scheduler/src/dag/mod.rs`](../../rio-scheduler/src/dag/mod.rs) — NEW function sibling to `find_newly_ready()`:

```rust
/// r[impl sched.ca.cutoff-propagate]
/// Walk downstream from a CA-unchanged completion. Return derivations
/// where (a) ONLY remaining incomplete dep was the CA drv, (b) status
/// is Queued (never touch Running — r[sched.preempt.never-running]).
pub fn find_cutoff_eligible(&self, completed_ca: &DrvHash) -> Vec<DrvHash> {
    let mut eligible = Vec::new();
    for downstream in self.edges_from(completed_ca) {
        let state = &self.nodes[downstream];
        if state.status != DerivationStatus::Queued { continue; }
        // Eligible iff ALL deps now terminal AND the triggering dep
        // was ca_output_unchanged. Other incomplete deps → skip.
        let all_deps_terminal = self.deps_of(downstream)
            .all(|d| self.nodes[d].status.is_terminal());
        if all_deps_terminal {
            eligible.push(downstream.clone());
        }
    }
    eligible
}
```

### T3 — `feat(scheduler):` cascade in completion.rs

MODIFY [`rio-scheduler/src/actor/completion.rs`](../../rio-scheduler/src/actor/completion.rs) — after P0251's compare, if `ca_output_unchanged`:

```rust
// r[impl sched.ca.cutoff-propagate]
if state.ca_output_unchanged {
    let mut frontier = vec![drv_hash.clone()];
    let mut depth = 0;
    const MAX_CASCADE_DEPTH: usize = 1000;  // R14: pathological-DAG cap
    while let Some(current) = frontier.pop() {
        if depth >= MAX_CASCADE_DEPTH {
            tracing::warn!(depth, "CA cutoff cascade hit depth cap");
            metrics::counter!("rio_scheduler_ca_cutoff_depth_cap_hits_total").increment(1);
            break;
        }
        for eligible in dag.find_cutoff_eligible(&current) {
            dag.transition(&eligible, DerivationStatus::Skipped)?;
            metrics::counter!("rio_scheduler_ca_cutoff_saves_total").increment(1);
            // Recurse: if eligible was also CA + unchanged, keep walking.
            // (Transitivity: A unchanged → B skipped → C depended only
            //  on B → C eligible too.)
            frontier.push(eligible);
        }
        depth += 1;
    }
}
```

### T4 — `feat(scheduler):` recovery query — Skipped persists

MODIFY [`rio-scheduler/src/db.rs`](../../rio-scheduler/src/db.rs) — the recovery `SELECT` that re-queues incomplete work on leader failover MUST exclude `Skipped`:

```sql
-- Before: WHERE status NOT IN ('completed', 'failed', 'poisoned', 'dependency_failed')
-- After:  WHERE status NOT IN ('completed', 'failed', 'poisoned', 'dependency_failed', 'skipped')
```

`Skipped` is terminal — leader failover must NOT re-queue.

### T5 — `test(scheduler):` 3-node cascade unit

MODIFY [`rio-scheduler/src/dag/tests.rs`](../../rio-scheduler/src/dag/tests.rs):

```rust
// r[verify sched.ca.cutoff-propagate]
#[test]
fn ca_cutoff_cascades_through_chain() {
    // A→B→C, all Queued. A is CA, completes with ca_output_unchanged=true.
    // Assert B transitions to Skipped.
    // Assert C transitions to Skipped (cascade).
    // Neither ever ran.
}

#[test]
fn ca_cutoff_skips_running() {
    // A→B, B is Running (not Queued). A completes unchanged.
    // Assert B stays Running — never-running invariant.
}

#[test]
fn ca_cutoff_depth_cap() {
    // Chain of 1001 Queued nodes. Trigger cascade.
    // Assert 1000 skipped, 1001st stays Queued, depth-cap counter +1.
}
```

## Exit criteria

- `/nbr .#ci` green
- `rg 'DerivationStatus::' rio-scheduler/src/ | wc -l` before == after (exhaustiveness caught all — no NEW wildcards added)
- `rg '_ =>' rio-scheduler/src/state/derivation.rs rio-scheduler/src/actor/` → review each hit manually; `Skipped` must be handled explicitly OR the wildcard behavior is demonstrably correct
- 3-node cascade test passes
- `nix develop -c tracey query rule sched.ca.cutoff-propagate` shows impl (dag/mod.rs + completion.rs) + verify (dag/tests.rs). VM verify comes in [P0254](plan-0254-ca-metrics-vm-demo.md) — transient `untested` between P0252/P0254 merges is EXPECTED.

## Tracey

References existing markers:
- `r[sched.ca.cutoff-propagate]` — T2+T3 implement, T5 verifies unit-level (seeded by P0245). VM verify in P0254.
- `r[sched.preempt.never-running]` — referenced in T2 comment + T5 skips-running test (NOT bumped — cutoff respects it)

## Files

```json files
[
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "T1: add Skipped variant + is_terminal + validate_transition + EVERY match site audit"},
  {"path": "rio-scheduler/src/dag/mod.rs", "action": "MODIFY", "note": "T2: find_cutoff_eligible() sibling to find_newly_ready()"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T3: cascade loop after P0251's compare"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "T4: recovery WHERE excludes 'skipped'"},
  {"path": "rio-scheduler/src/dag/tests.rs", "action": "MODIFY", "note": "T5: cascade + never-running + depth-cap tests"}
]
```

```
rio-scheduler/src/
├── state/derivation.rs           # T1: Skipped variant (state-machine ripple)
├── dag/mod.rs                    # T2: find_cutoff_eligible()
├── actor/completion.rs           # T3: cascade (serial after P0251)
├── db.rs                         # T4: recovery WHERE
└── dag/tests.rs                  # T5: cascade tests
```

## Dependencies

```json deps
{"deps": [251], "soft_deps": [], "note": "DISPATCH SOLO — state-machine ripple (R1). completion.rs serial after P0251 (dep enforces). dag/mod.rs low collision. db.rs serial after P0250 (transitive via P0251→P0250). USER Q2: Skipped variant not CompletedViaCutoff."}
```

**Depends on:** [P0251](plan-0251-ca-cutoff-compare.md) — `ca_output_unchanged` set.
**Conflicts with:** `derivation.rs` state-machine touches EVERY `match DerivationStatus` — **DISPATCH SOLO**, no concurrent `actor/` plans (4c R4 pattern). `completion.rs` + `db.rs` serial via dep chain.
