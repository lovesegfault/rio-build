# Plan 0251: CA cutoff COMPARE — completion.rs hash-check hook

When a CA derivation completes, compare its output `nar_hash` against the content index. A match means the output is byte-identical to a prior build. **This plan does the comparison only** — propagation (transitioning downstream to `Skipped`) is [P0252](plan-0252-ca-cutoff-propagate-skipped.md).

Per GT5 (`.claude/notes/phase5-partition.md` §1): cutoff does NOT read `realisations.output_hash` (that's zeros per [`opcodes_read.rs:493`](../../rio-gateway/src/handler/opcodes_read.rs) and a signing concern for [P0256](plan-0256-per-tenant-signing-output-hash.md)). Cutoff compares the completion's `nar_hash` against [`content_index.rs:12`](../../rio-store/src/metadata/content_index.rs) semantics: "content_hash = nar_hash".

**Anchor warning (R9):** `completion.rs` touched by 4b P0206+P0219 and 4c P0228. Line `:298` in the partition note is STALE. At dispatch: `git log -p --since='2 months' -- rio-scheduler/src/actor/completion.rs | head -100` — find the EMA-update anchor P0228 left.

## Entry criteria

- [P0250](plan-0250-ca-detect-plumb-is-ca.md) merged (`DerivationState.is_ca` populated)
- [P0228](plan-0228-ema-duration-completion-hook.md) merged (4c — ONLY 4c `completion.rs` touch; anchor for this hook)

## Tasks

### T1 — `feat(scheduler):` hash-check branch in completion success path

MODIFY [`rio-scheduler/src/actor/completion.rs`](../../rio-scheduler/src/actor/completion.rs) — in the success path, near P0228's EMA update anchor:

```rust
// r[impl sched.ca.cutoff-compare]
// If this was a CA derivation, check if its output hash matches
// something already in the content index. Match → P0252 will skip
// downstream builds (this hook just records the compare result).
if state.is_ca {
    for output in &completion.outputs {
        let matched = store_client
            .query_content_index(QueryContentIndexRequest {
                content_hash: output.nar_hash.clone(),
            })
            .await
            .map(|r| r.into_inner().found)
            .unwrap_or(false);
        metrics::counter!(
            "rio_scheduler_ca_hash_compares_total",
            "outcome" => if matched { "match" } else { "miss" }
        ).increment(1);
        // MVP: single bool. Multi-output CA is rare; per-output map
        // is a followup if the VM test (P0254) shows it matters.
        state.ca_output_unchanged = matched;
    }
}
```

### T2 — `feat(scheduler):` add ca_output_unchanged to DerivationState

MODIFY [`rio-scheduler/src/state/derivation.rs`](../../rio-scheduler/src/state/derivation.rs):

```rust
/// Set by completion.rs hash-check (P0251). Consumed by
/// dag/mod.rs find_cutoff_eligible() (P0252).
pub ca_output_unchanged: bool,
```

Default `false`.

### T3 — `feat(scheduler):` register counter

MODIFY [`rio-scheduler/src/lib.rs`](../../rio-scheduler/src/lib.rs) — register `rio_scheduler_ca_hash_compares_total{outcome}`.

### T4 — `test(scheduler):` hash match increments counter

NEW test in [`rio-scheduler/src/actor/tests/completion.rs`](../../rio-scheduler/src/actor/tests/completion.rs) (or wherever completion tests live):

```rust
// r[verify sched.ca.cutoff-compare]
#[tokio::test]
async fn ca_completion_with_matching_hash_sets_unchanged() {
    // Mock store_client.query_content_index to return found=true.
    // Submit CA drv with is_ca=true, complete it.
    // Assert state.ca_output_unchanged == true.
    // Assert counter rio_scheduler_ca_hash_compares_total{outcome="match"} +1.
}
```

## Exit criteria

- `/nbr .#ci` green
- `nix develop -c tracey query rule sched.ca.cutoff-compare` shows impl + verify

## Tracey

References existing markers:
- `r[sched.ca.cutoff-compare]` — T1 implements, T4 verifies (seeded by P0245)

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T1: hash-check branch after P0228's EMA anchor (~30 lines)"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "T2: add ca_output_unchanged: bool"},
  {"path": "rio-scheduler/src/lib.rs", "action": "MODIFY", "note": "T3: register ca_hash_compares_total counter"},
  {"path": "rio-scheduler/src/actor/tests/completion.rs", "action": "MODIFY", "note": "T4: hash-match unit test"}
]
```

```
rio-scheduler/src/
├── actor/completion.rs           # T1: hash-check (find P0228 anchor at dispatch)
├── state/derivation.rs           # T2: +ca_output_unchanged
├── lib.rs                        # T3: counter register
└── actor/tests/completion.rs     # T4: verify test
```

## Dependencies

```json deps
{"deps": [250, 228], "soft_deps": [], "note": "completion.rs count=20+ — serial after P0228. ONLY phase5 completion.rs touch before P0253 (resolution). R9: git log -p at dispatch, do NOT trust :298."}
```

**Depends on:** [P0250](plan-0250-ca-detect-plumb-is-ca.md) — `is_ca` flag populated. [P0228](plan-0228-ema-duration-completion-hook.md) — 4c's only `completion.rs` touch; this hook anchors near its EMA update.
**Conflicts with:** `completion.rs` count=20+. Serial after P0228 (dep enforces). [P0253](plan-0253-ca-resolution-dependentrealisations.md) may also touch completion.rs — dep chain serializes.
