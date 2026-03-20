# Plan 0421: merge.py rename-cluster → plan_assign.py extraction

consol-mc236 feature. [`merge.py`](../../.claude/lib/onibus/merge.py) is 604L with 14 commits since [`b2980679`](https://github.com/search?q=b2980679&type=commits). Two high-churn clusters with zero call-edges between them:

| Cluster | Lines | Fns | Touched by | Purpose |
|---|---|---|---|---|
| rename | [`:368-604`](../../.claude/lib/onibus/merge.py) (~236L) | 9 private + 1 public `rename_unassigned` | [P0325](plan-0325-rename-unassigned-post-ff-rewrite-skip.md), [P0401](plan-0401-docs-writer-lazy-t-assignment.md), [P0418](plan-0418-onibus-rename-canonicalization-hardening.md)-T3/T4/T5, any future P1000-headroom fix | P-/T-placeholder rewrite + docs-worktree git-mv |
| count/flip | [`:123-233`](../../.claude/lib/onibus/merge.py) (~110L) | `count_bump`, `dag_flip`, `_cadence_range` | [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md), [P0414](plan-0414-merger-amend-branch-ref-fix.md), [P0417](plan-0417-dag-flip-already-done-double-bump.md), [P0420](plan-0420-count-bump-toctou-write-order.md) | merge-count state + dag.jsonl DONE-flip + amend |
| lock/queue | [`:55-120`](../../.claude/lib/onibus/merge.py) (~65L) | `lock`, `unlock`, `lock_status`, `queue_consume` | [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md)-T2, [P0418](plan-0418-onibus-rename-canonicalization-hardening.md)-T2 | merger lease + merge-queue consume |


**Extraction target:** `.claude/lib/onibus/plan_assign.py` (P-/T-placeholder assignment machinery). The count/flip + lock/queue clusters stay in `merge.py` (together they're ~180L, cohesive around the merger's state files).

Cost: 1 new file, ~10L imports, 1 `cli.py` subcommand re-route, 1 `test_scripts.py` import-line change. `/merge-impl`'s fast-path invokes `rename_unassigned` via `onibus merge rename-unassigned` — re-route that subcommand to `onibus.plan_assign`.

**Sequence:** after [P0417](plan-0417-dag-flip-already-done-double-bump.md) AND [P0418](plan-0418-onibus-rename-canonicalization-hardening.md) land. P0417 touches `:123-233` (count/flip, stays in merge.py — no conflict with extraction). P0418 touches `:376-576` (rename cluster — the extraction SUBJECT). Extracting while P0418 is in-flight means rebase-pain; wait for it.

## Entry criteria

- [P0417](plan-0417-dag-flip-already-done-double-bump.md) merged (`dag_flip` + `count_bump(plan=)` stable at `:123-233`)
- [P0418](plan-0418-onibus-rename-canonicalization-hardening.md) merged (rename-cluster edits at `:376-576` landed — anchored P-replace, T-rewrite scope, regex widen)

## Tasks

### T1 — `refactor(harness):` extract rename cluster → plan_assign.py

NEW [`.claude/lib/onibus/plan_assign.py`](../../.claude/lib/onibus/plan_assign.py). Move from [`merge.py:368-604`](../../.claude/lib/onibus/merge.py):

- module docstring explaining the P-/T-placeholder lifecycle (docs-writer emits → rename_unassigned rewrites → merger amends)
- `_T_PLACEHOLDER_RE`, `_P_PLACEHOLDER_RE`, `_PLAN_FNAME_RE`, any other regex constants from the `:368-380` region
- `rename_unassigned` (the single public entry)
- `RenameReport` model import (from `onibus.models`) or move the model if it's merge.py-local

Imports needed: `REPO_ROOT`, `INTEGRATION_BRANCH` from `onibus` (same as merge.py today); `git`, `git_try` from `onibus.git_ops`; `read_jsonl`, `atomic_write_text` from `onibus.jsonl`; models.

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) — delete `:368-604` (the rename cluster), keep `:1-367` (lock/queue + count/flip + cadence). Add a backward-compat re-export at module end:

```python
# Backward-compat — rename_unassigned moved to plan_assign.py (P0421).
# Callers via `onibus merge rename-unassigned` are re-routed in cli.py; this
# re-export keeps `from onibus.merge import rename_unassigned` working for
# one sprint (test_scripts.py + any stragglers).
from onibus.plan_assign import rename_unassigned, RenameReport  # noqa: F401
```

### T2 — `refactor(harness):` cli.py — re-route rename-unassigned subcommand

MODIFY [`.claude/lib/onibus/cli.py`](../../.claude/lib/onibus/cli.py). Find the `merge rename-unassigned` subcommand handler (grep `rename_unassigned\|rename-unassigned`). Change import from `onibus.merge` to `onibus.plan_assign`. The subcommand name stays `merge rename-unassigned` for backward-compat with `/merge-impl`'s fast-path invocation — only the implementation module changes.

Alternatively: add a top-level `onibus plan-assign rename` subcommand and keep `merge rename-unassigned` as a deprecated alias. Lower priority — the current callsite is one fast-path script.

### T3 — `test(harness):` test_scripts.py — update rename import

MODIFY [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py). Grep for `from onibus.merge import rename_unassigned` (and related `_find_placeholders`, `_rewrite_t_placeholders` private-fn imports if any tests reach into them). Either:

- (a) leave `from onibus.merge import rename_unassigned` as-is (T1's re-export covers it — zero-change option)
- (b) update to `from onibus.plan_assign import rename_unassigned` + private fns

Prefer (b) so the re-export can be removed after one sprint. Grep at dispatch for exact imports to update — [P0418](plan-0418-onibus-rename-canonicalization-hardening.md)-T6 adds `test_rename_placeholder_doc_t_crossref` + `test_rename_deps_fence_leading_zero` which import from the rename cluster.

### T4 — `docs(harness):` merge.py module docstring — reflect split

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) module docstring (add or extend at `:1`):

```python
"""Merger state machinery: lock/lease, merge-count, dag-flip, cadence-window.

Split from the original merge.py monolith (P0421). The rename_unassigned
subsystem (P-/T-placeholder rewrite for docs-writer → merger pipeline)
moved to plan_assign.py — 236L self-contained, zero call-edges into this
module. What stays here: merger's state-file surface (merge-count.txt,
merge-shas.jsonl, merge-queue.jsonl, the lease lock) + the dag_flip compound
that the merger's step-7.5 collapses to one CLI call."""
```

Add a parallel docstring at the top of `plan_assign.py` (T1) explaining the P-/T-placeholder lifecycle.

## Exit criteria

- `/nixbuild .#ci` green (or clause-4c — `.claude/`-only, behavioral-identical per [P0319](plan-0319-merge-shas-pre-amend-dangling.md))
- `wc -l .claude/lib/onibus/merge.py` → ≤400 (T1: ~236L moved out; post-P0418 merge.py may be ~640L, post-extraction ~400L)
- `test -f .claude/lib/onibus/plan_assign.py` (T1: new file exists)
- `grep '_T_PLACEHOLDER_RE\|_worktree_for\|_find_placeholders\|_rewrite_t_placeholders\|_rewrite_and_rename' .claude/lib/onibus/merge.py` → 0 hits (T1: all moved)
- `grep 'rename_unassigned' .claude/lib/onibus/merge.py` → ≤2 hits (only the re-export line + its comment; T1)
- `python3 -c "from onibus.plan_assign import rename_unassigned, RenameReport"` — no ImportError (T1)
- `python3 -c "from onibus.merge import rename_unassigned"` — no ImportError (T1 backward-compat re-export)
- `grep 'from onibus.plan_assign import\|from onibus\.plan_assign' .claude/lib/onibus/cli.py` → ≥1 hit (T2: subcommand re-routed)
- `nix develop -c pytest .claude/lib/test_scripts.py -k 'rename'` → all pass (same count as pre-extraction; T3: imports work)
- `.claude/bin/onibus merge rename-unassigned --help` → exits 0, help text shown (T2: subcommand still works)
- `grep 'P0421\|plan_assign' .claude/lib/onibus/merge.py` → ≥1 hit in module docstring (T4)

## Tracey

No markers. Harness-tooling refactor; not spec-behavior.

## Files

```json files
[
  {"path": ".claude/lib/onibus/plan_assign.py", "action": "NEW", "note": "T1: rename cluster from merge.py:368-604 — 8 private fns + rename_unassigned entry + regex constants + module docstring"},
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T1: delete :368-604 rename cluster, add backward-compat re-export at module end. T4: module docstring reflecting split"},
  {"path": ".claude/lib/onibus/cli.py", "action": "MODIFY", "note": "T2: merge rename-unassigned subcommand — import from onibus.plan_assign"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T3: update rename_unassigned + private-fn imports to onibus.plan_assign (or leave as-is via T1 re-export)"}
]
```

```
.claude/lib/onibus/
├── plan_assign.py   # T1: NEW — rename_unassigned + 8 private fns + regex
├── merge.py         # T1+T4: delete :368-604, re-export, docstring
└── cli.py           # T2: subcommand re-route
.claude/lib/
└── test_scripts.py  # T3: import update
```

## Dependencies

```json deps
{"deps": [417, 418], "soft_deps": [414, 401, 325, 420, 304], "note": "HARD-dep P0417 (touches merge.py :123-233 count/flip cluster — stays in merge.py, but landing it first keeps the split's base stable). HARD-dep P0418 (touches :376-576 rename cluster — the extraction SUBJECT; T3/T4/T5 edit the exact lines being moved. Extracting mid-P0418-impl = rebase-pain). Soft-dep P0414 (DONE — dag_flip compound exists; stays in merge.py). Soft-dep P0401 (DONE — _T_PLACEHOLDER_RE + _rewrite_t_placeholders + _touched_batch_docs are the rename-cluster fns being moved). Soft-dep P0325 (DONE — _rewrite_and_rename mapping-derived logic being moved). Soft-dep P0420 (touches count_bump :123-156 — stays in merge.py; non-overlapping with extraction). Soft-dep P0304-T191 (update-ref fallback at :217-223 — stays in merge.py). COLLISION CAUTION: merge.py has 4+ in-flight touchers at plan-write time. Sequence AFTER P0417+P0418 drain makes the extraction a single clean cut. If a 5th rename-touching plan appears post-P0418 (P0418's own doc notes the :432 plan-0\\d{3} hardcode guarantees a P1000-headroom fix — which P0418-T5 already addresses), check at dispatch whether another rename-cluster plan is in-flight."}
```

**Depends on:** [P0417](plan-0417-dag-flip-already-done-double-bump.md) + [P0418](plan-0418-onibus-rename-canonicalization-hardening.md) — both touch merge.py heavily; wait for both to stabilize the base.

**Conflicts with:** [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) — this plan is a FILE-LEVEL rewrite (delete 236L block). Any concurrent plan touching `:368-604` rebase-conflicts. SEQUENCE LAST in the merge.py-touching chain. [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py) — T3 is import-line edit only; additive, non-conflicting with P0417-T4 / P0418-T6 test additions.
