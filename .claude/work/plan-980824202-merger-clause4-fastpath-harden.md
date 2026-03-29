# Plan 980824202: TOOLING — harden merger Clause-4 fast-path against red tests

The merger's Clause-4 fast-path (codified from 29+ precedents in [P0304 §Clause-4](plan-0304-trivial-batch-p0222-harness.md)) lets docs-only / test-only changes skip the full `.#ci` gate when the diff is provably behavior-neutral. Coverage-sink analysis found it let **red tests through to sprint-1**: a test-only change added a new test attr, the fast-path said "test-only = skip gate", the new test was red, and it merged anyway.

Three fixes needed:

1. **Merger halts queue on `.#ci` red** unless hash-identity is *proven* (not assumed from diff category). Currently "test-only" is a free pass; it should be "test-only AND (new test attrs green OR no new test attrs)".
2. **New test attrs must be green.** Fast-path diffs that add `#[test]` / `#[tokio::test]` / nix-test subtests must run those specific tests before merge. A 30-second `cargo nextest run <new_test_name>` is still faster than full `.#ci`.
3. **dag-run checks queue-halted sentinel.** When the merger halts, it should write a sentinel file; `/dag-run` checks it before dispatching new work, so the coordinator sees the halt instead of stacking more merges behind a broken queue.

Tooling correctness — affects `.claude/lib/onibus/merge.py` and skill docs. Discovered by coverage-sink when a red-test merge showed up in `coverage-pending.jsonl` with a test that *never* passed.

## Tasks

### T1 — `fix(tooling):` merger halts on .#ci red unless hash-identity proven

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py). The Clause-4 fast-path currently categorizes diffs (docs-only, test-only, tooling-only) and skips `.#ci` for "safe" categories. Change: the skip is only valid when the resulting `.#ci` derivation *hash* is identical to the last-known-green hash. For test-only changes that add tests, the hash changes (new test is a new derivation input) → fast-path does NOT apply → run `.#ci`.

```python
# Clause-4: fast-path only when .#ci drv-hash unchanged vs last green.
# "test-only diff" is NOT sufficient — adding a test changes the hash.
ci_hash = nix_derivation_hash(".#ci")
if ci_hash == last_green_hash:
    log("clause-4: hash-identical to last green — skip .#ci")
    return FastPath.SKIP
# Hash changed → something observable changed → run the gate.
```

If hash-identity checking is too slow/brittle, fall back to: "fast-path only for diffs that touch ZERO files under `rio-*/src/` AND `rio-*/tests/` AND `nix/tests/`" — i.e., pure docs.

### T2 — `fix(tooling):` new test attrs must pass before fast-path merge

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py). When a fast-path diff adds test functions (detected via `git diff --name-only | xargs grep -l '#\[.*test\]'` on added lines), extract the test names and run them:

```python
new_tests = extract_new_test_names(diff)
if new_tests:
    rc = run(["cargo", "nextest", "run"] + new_tests)
    if rc != 0:
        halt_queue("new test(s) red: " + ", ".join(new_tests))
        return FastPath.HALT
```

This is still fast (~seconds for targeted tests vs ~minutes for `.#ci`) but catches the "added a red test" case.

### T3 — `feat(tooling):` queue-halted sentinel + dag-run check

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) — on halt, write `.claude/state/queue-halted` with the reason.

MODIFY [`.claude/skills/dag-run/SKILL.md`](../../.claude/skills/dag-run/SKILL.md) — add a pre-dispatch check: if `.claude/state/queue-halted` exists, surface the reason and do NOT dispatch new `/implement` calls until the halt is cleared (manually, after fixing the root cause).

### T4 — `test(tooling):` regression test for red-test fast-path

NEW test in [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py). Mock a diff that adds a `#[test] fn always_fails() { panic!() }`, run the merger's fast-path check, assert it returns `HALT` not `SKIP`.

## Exit criteria

- `pytest .claude/lib/test_scripts.py -k 'fast_path_red_test_halts'` → pass
- `grep 'queue-halted\|halt_queue' .claude/lib/onibus/merge.py` → ≥2 hits (write sentinel + check)
- `grep 'queue-halted' .claude/skills/dag-run/SKILL.md` → ≥1 hit (pre-dispatch check documented)
- `/nbr .#ci` green

## Tracey

No domain markers — tooling change, not rio-* behavior. The Clause-4 decision tree in [P0304](plan-0304-trivial-batch-p0222-harness.md) is the closest thing to a spec; this plan tightens it.

## Files

```json files
[
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T1: hash-identity check; T2: new-test-green check; T3: sentinel write"},
  {"path": ".claude/skills/dag-run/SKILL.md", "action": "MODIFY", "note": "T3: pre-dispatch queue-halted check"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T4: fast-path regression test"}
]
```

```
.claude/
├── lib/
│   ├── onibus/merge.py    # T1-T3
│   └── test_scripts.py    # T4
└── skills/dag-run/
    └── SKILL.md           # T3
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "Tooling — no rio-* deps. Touches merge.py which P0304 T980824202 also touches (_next_real scan) — serialize or combine."}
```

**Depends on:** None (tooling).

**Conflicts with:** [P0304](plan-0304-trivial-batch-p0222-harness.md) T980824202 also touches `.claude/lib/onibus/merge.py` (`_next_real` at `:505`). Different functions — this plan's edits are in the Clause-4 fast-path region; T980824202's are in `rename_unassigned`. Should rebase cleanly.
