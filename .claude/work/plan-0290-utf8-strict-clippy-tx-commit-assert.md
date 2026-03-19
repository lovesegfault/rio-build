# Plan 0290: from_utf8 strict + clippy disallowed-methods + tx-commit debug_assert

**Retro P0020+P0191 finding.** Two regression-guard trivia, both with the same shape: a phase-4a remediation that committed to a specific fix but didn't land it, and a reintroduction of a pattern explicitly eliminated.

**P0020:** P0020's Outcome said "No `from_utf8_lossy` in production code paths." P0017 rationale: lossy→parse silently produces U+FFFD → confusing ATerm parse error instead of clear UTF-8 error. The exact pattern P0017 commit [`2f807a4`](https://github.com/search?q=2f807a4&type=commits) eliminated is **live at HEAD** in [`rio-worker/src/executor/mod.rs:315`](../../rio-worker/src/executor/mod.rs):

```rust
let drv_text = String::from_utf8_lossy(&assignment.drv_content);
```

Introduced by [`395e826f`](https://github.com/search?q=395e826f&type=commits) — ONE DAY after P0020 completed. The parallel branch of the SAME if-statement (`:313` → `fetch_drv_from_store` → `parse_from_nar` at [`rio-nix/src/derivation/mod.rs:168`](../../rio-nix/src/derivation/mod.rs)) uses strict `from_utf8`. Two branches, same logical bytes, different UTF-8 handling. Repo-wide grep confirms `:315` is the ONLY parse-downstream lossy case.

**P0191:** `r[sched.db.tx-commit-before-mutate]` has `r[impl]` at [`rio-scheduler/src/actor/merge.rs:429`](../../rio-scheduler/src/actor/merge.rs), NO `r[verify]`. Remediation doc [`12-pg-transaction-safety.md:1087-1107`](../../docs/src/remediations/phase4a/12-pg-transaction-safety.md) proposed option (b): ~8-line `#[cfg(debug_assertions)]` loop, **explicitly endorsed at line 1107 as "sufficient."** Never landed.

**User decision:** fix + **clippy disallowed-methods** to prevent round 3 of lossy reintroduction. Plus the 8-line tx-commit assert.

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync.md) merged (phase4b fan-out root)

## Tasks

### T1 — `fix(worker):` strict from_utf8 at executor/mod.rs:315

MODIFY [`rio-worker/src/executor/mod.rs`](../../rio-worker/src/executor/mod.rs) at `:315`:

```rust
// Strict UTF-8 — matches the else-branch (parse_from_nar uses
// strict from_utf8 at derivation/mod.rs:168). Lossy would silently
// produce U+FFFD → ATerm parse fails with a confusing "unexpected
// character" instead of the real UTF-8 error. P0017's 2f807a4
// eliminated this pattern; 395e826f reintroduced it one day after
// P0020 closed. Clippy disallowed-methods (T2) prevents round 3.
let drv_text = std::str::from_utf8(&assignment.drv_content)
    .map_err(|e| ExecutorError::BuildFailed(
        format!("drv content is not valid UTF-8: {e}")
    ))?;
```

### T2 — `refactor:` clippy.toml disallowed-methods

NEW [`clippy.toml`](../../clippy.toml) at workspace root:

```toml
# Disallowed methods — each entry is a pattern that was eliminated
# once, reintroduced, and eliminated again. Clippy enforces round 3
# doesn't happen.
disallowed-methods = [
    # Parse-path lossy. Silently produces U+FFFD on invalid bytes →
    # downstream ATerm/wire parser fails with a confusing error instead
    # of the real UTF-8 error. P0017 eliminated (2f807a4); 395e826f
    # reintroduced ONE DAY later. Use str::from_utf8 + proper error.
    # Allowed only in test assertions / log display / panic messages —
    # wrap those call sites with #[allow(clippy::disallowed_methods)].
    { path = "alloc::string::String::from_utf8_lossy", reason = "silently produces U+FFFD; use str::from_utf8 for parse paths (P0020/P0290)" },
]
```

Grep for existing acceptable uses (log display / test assertions / panic messages per retrospective) and wrap each with `#[allow(clippy::disallowed_methods)]`:

```bash
rg 'from_utf8_lossy' --type rust -l | xargs -I{} grep -n 'from_utf8_lossy' {}
# Each hit: decide if it's parse-path (FIX) or display-path (ALLOW annotation).
```

### T3 — `test(scheduler):` tx-commit debug_assert (12-doc:1091-1100)

MODIFY [`rio-scheduler/src/actor/merge.rs`](../../rio-scheduler/src/actor/merge.rs) — insert immediately before `tx.commit().await?` at `:427`:

```rust
// r[verify sched.db.tx-commit-before-mutate]
// In-mem mutation ordering invariant: no newly-inserted node has
// db_id set BEFORE commit. If this fires, someone re-introduced
// the in-tx write (the P0191 bug). Fires in every existing merge
// test's happy path — zero new test scaffolding.
// (rem-12 option b, endorsed at 12-pg-transaction-safety.md:1107)
#[cfg(debug_assertions)]
for hash in &newly_inserted {
    if let Some(state) = self.dag.node(hash.as_str()) {
        debug_assert!(
            state.db_id.is_none(),
            "newly-inserted node {hash} has db_id set before commit — \
             in-mem mutation leaked into tx scope"
        );
    }
}
```

The existing `test_merge_db_failure_rolls_back_memory` at `merge.rs:13-51` closes the pool BEFORE sending `MergeDag` — `insert_build` fails first, never reaches this block. The happy-path merge tests DO reach it; the debug_assert makes them cover the ordering invariant for free.

## Exit criteria

- `/nbr .#ci` green (includes `cargo clippy --all-targets -- --deny warnings` — disallowed-methods is opt-in via clippy.toml presence)
- `grep from_utf8_lossy rio-worker/src/executor/mod.rs` → 0 hits
- `tracey query untested | grep tx-commit-before-mutate` → GONE
- `test -f clippy.toml` + `grep from_utf8_lossy clippy.toml` → 1 hit
- `rg 'from_utf8_lossy' --type rust | rg -v 'allow.*disallowed|test|panic'` → 0 parse-path hits

## Tracey

References existing markers:
- `r[sched.db.tx-commit-before-mutate]` — T3 adds the `r[verify]` (was: `r[impl]` at `merge.rs:429`, NO verify)

## Files

```json files
[
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "T1: lossy→strict at :315, ~5 LoC"},
  {"path": "rio-scheduler/src/actor/merge.rs", "action": "MODIFY", "note": "T3: 8-line #[cfg(debug_assertions)] loop before tx.commit at :427, r[verify sched.db.tx-commit-before-mutate]"}
]
```

**Root-level file (outside Files-fence validator pattern):** `clippy.toml` NEW at workspace root — T2 disallowed-methods. Zero collision risk (no other plan creates it).

Allow-annotation sweep (T2): likely touches several files (test modules, log-display helpers). Exact list determined at impl time via `rg 'from_utf8_lossy' --type rust -l`. Retrospective confirms "others are log display / test assertions / panic messages — acceptable" — those get `#[allow]`, not the fix.

```
clippy.toml                       # T2: NEW disallowed-methods
rio-worker/src/executor/mod.rs    # T1: strict from_utf8
rio-scheduler/src/actor/merge.rs  # T3: debug_assert loop
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [], "note": "retro P0020+P0191 — discovered_from=20,191. P0017 pattern reintroduced 1 day post-elimination (395e826f). Clippy disallowed-methods prevents round 3. tx-commit: 8-line debug_assert from 12-doc:1091 (endorsed :1107), fires in existing merge tests' happy path — closes tracey gap with zero new scaffolding."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync.md) — phase4b fan-out root.
**Conflicts with:** [`executor/mod.rs`](../../rio-worker/src/executor/mod.rs) low-traffic. [`merge.rs`](../../rio-scheduler/src/actor/merge.rs) low-traffic. `clippy.toml` is NEW — only collision risk is another plan creating it; none currently does.
