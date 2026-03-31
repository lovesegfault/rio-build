# Plan 992487503: `by_drv` dict collapses duplicate drv_name — retry policy reads are file-order-dependent

[`build.py:233`](../../.claude/lib/onibus/build.py) builds `by_drv = {f.drv_name: f for f in flake_rows if f.drv_name}` — dict comprehension, last-wins on duplicate keys. [`known-flakes.jsonl:14`](../../.claude/known-flakes.jsonl) adds a SECOND `rio-lifecycle-core` entry (flannel line 5 + disruption-drain line 14, per [P0518](plan-0518-disruption-drain-timeout-cov.md) reviewer); `rio-lifecycle-recovery` already had two (lines 7+12).

Boolean `excusable()` is still correct — set membership is unaffected (either entry makes the drv "known"). But [`build.py:254`](../../.claude/lib/onibus/build.py) `matched_row.retry` reads the WRONG entry's policy. Both colliding pairs today are `retry=Once` so there's no observable bug — but a future `retry=Never` on the same drv_name is silently file-order-dependent: the entry that happens to be LATER in the jsonl wins.

**The landmine:** someone adds a `retry=Never` entry for `rio-lifecycle-core` (say, a real test bug that should block merge). If it lands on line 5-ish (before line 14's `retry=Once`), line 14 wins and the Never entry is invisible. CI retries when it shouldn't.

discovered_from=518. origin=reviewer.

## Tasks

### T1 — `fix(tooling):` by_drv → `dict[str, list[KnownFlake]]` + pick-most-restrictive

Two routes. **Route A preferred** — matches the data model (multiple flakes per drv is valid; one VM test has multiple subtests, each with its own flake entry).

**Route A (accumulate, pick most restrictive):**

```python
# build.py:233 — replace the comprehension:
from collections import defaultdict
by_drv: dict[str, list[KnownFlake]] = defaultdict(list)
for f in flake_rows:
    if f.drv_name:
        by_drv[f.drv_name].append(f)

# build.py:240 — matched_row lookup now picks most-restrictive:
# retry ordering: Never > Once (Never is more restrictive — blocks retry).
# If ANY entry for this drv says Never, treat the whole drv as Never.
def _most_restrictive(entries: list[KnownFlake]) -> KnownFlake:
    # Never > Once. Prefer the first Never if any; else first Once.
    nevers = [e for e in entries if e.retry == "Never"]
    return nevers[0] if nevers else entries[0]

matched_row = None
if matched:
    key = matched[0]
    if key in by_test:
        matched_row = by_test[key]
    elif key in by_drv:
        matched_row = _most_restrictive(by_drv[key])
```

**`by_test` (line 232) is NOT touched** — `test` keys are `crate::path::test_name` form, collision is unlikely and would indicate a real data error (two entries for the same unit test). If paranoid, apply the same `list[]` treatment — but that's scope creep.

**Route B (reject dupes at write):** add a model validator on `KnownFlake` that scans siblings at `onibus flake add` time:

```python
# models.py — conceptually (validators don't see siblings, so this
# goes in the add-command handler, not the model):
existing = {f.drv_name for f in read_jsonl(KNOWN_FLAKES, KnownFlake) if f.drv_name}
if new_entry.drv_name and new_entry.drv_name in existing:
    raise ValueError(
        f"drv_name {new_entry.drv_name!r} already in known-flakes.jsonl — "
        f"one entry per drv. Multiple subtests of the same VM scenario "
        f"share the drv_name; consolidate or use test-name keys."
    )
```

**Route B rejected for now:** multiple entries per drv_name is LEGITIMATE. `rio-lifecycle-core` has flannel flake (line 5) AND disruption-drain flake (line 14) — two distinct subtest flakes, same VM drv. Rejecting dupes would force consolidation into one entry with a vaguer symptom description.

### T2 — `test(tooling):` dict-collapse regression test

```python
# test_scripts.py — new test near the existing build.py tests
def test_by_drv_collapse_picks_most_restrictive():
    """build.py:233 dict comprehension used to last-win on dupe drv_name.
    A retry=Never entry earlier in the file was shadowed by a later
    retry=Once. P992487503 fix: accumulate + pick most restrictive.
    """
    # Synthetic known-flakes: two entries same drv_name, conflicting retry.
    # Order 1: Never then Once — pre-fix, Once wins (wrong).
    # Order 2: Once then Never — pre-fix, Never wins (accidentally right).
    # Post-fix: both orders → Never wins.
    flakes_never_first = [
        KnownFlake(test="t1", drv_name="rio-test", retry="Never", ...),
        KnownFlake(test="t2", drv_name="rio-test", retry="Once", ...),
    ]
    flakes_once_first = list(reversed(flakes_never_first))

    for flakes in (flakes_never_first, flakes_once_first):
        # ... mock read_jsonl to return `flakes`, invoke the by_drv
        # construction + _most_restrictive ...
        assert picked.retry == "Never", (
            f"retry=Never entry present but {picked.retry!r} picked — "
            f"file-order-dependent (pre-P992487503 bug)"
        )
```

## Exit criteria

- `/nbr .#ci` green
- `grep 'by_drv\[f.drv_name\].append\|defaultdict' .claude/lib/onibus/build.py` → ≥1 hit (accumulator pattern, not comprehension)
- `grep '_most_restrictive\|nevers\[0\]' .claude/lib/onibus/build.py` → ≥1 hit (restrictiveness policy encoded)
- `nix develop -c pytest .claude/lib/test_scripts.py::test_by_drv_collapse_picks_most_restrictive` → passes
- Mutation: revert `by_drv` to dict comprehension → T2's test FAILS on the Never-then-Once ordering
- `grep -c 'rio-lifecycle-core' .claude/known-flakes.jsonl` ≥ 2 (confirms the dupe is still valid data — this plan doesn't delete it)

## Tracey

No domain markers — `.claude/lib/onibus` tooling is below the spec surface. `docs/src/components/` has no `r[tooling.*]` domain. The known-flakes mechanism is coordinator-protocol, not product behavior.

## Files

```json files
[
  {"path": ".claude/lib/onibus/build.py", "action": "MODIFY", "note": "T1: :233 by_drv comprehension → defaultdict(list) accumulator. :240 matched_row → _most_restrictive helper. P0304-T992487502 (stale-subshell re-import) touches same file — DIFFERENT section (merger step-6), clean rebase"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T2: test_by_drv_collapse_picks_most_restrictive near existing build.py tests. P0304-T532 touches :813 (tracey domains test) — diff section. P0523-T2 also touches this file (docs-merger test) — diff section"}
]
```

```
.claude/lib/
├── onibus/build.py    # T1: :233 accumulator, :240 _most_restrictive
└── test_scripts.py    # T2: regression test
```

## Dependencies

```json deps
{"deps": [518], "soft_deps": [], "note": "P0518 is discovered_from — its known-flakes entry at :14 is the second rio-lifecycle-core, making the collapse observable. P0518 DONE. Ships independently."}
```

**Depends on:** [P0518](plan-0518-disruption-drain-timeout-cov.md) (DONE) — discovered_from; its `known-flakes.jsonl:14` entry is the second `rio-lifecycle-core`, making the dict collapse observable (was latent with only lines 7+12 colliding on `rio-lifecycle-recovery`).

**Conflicts with:** `build.py` is low-traffic. [P0304](plan-0304-trivial-batch-p0222-harness.md) T992487502 (stale-subshell re-import) touches the same file — different section (merger step-6 subshell wiring vs `:233` excusable logic), clean rebase. `test_scripts.py` shared with P0304-T532 (`:813` tracey-domains test) and P0523-T2 (docs-merger test) — all additive, different sections.
