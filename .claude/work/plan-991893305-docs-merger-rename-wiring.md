# Plan 991893305: wire `rename-unassigned` into docs-branch merger — pre-ff step

**SECOND TIME** this has required coordinator manual-fixup. Precedent: [`4e755ae0`](https://github.com/search?q=4e755ae0&type=commits).

[`merge.py:986-993`](../../.claude/lib/onibus/merge.py) — `rename-unassigned` checks `git merge-base --is-ancestor HEAD {INTEGRATION_BRANCH}` and refuses if the docs branch is already merged:

> "If you ff-merged before renaming, the rename commit will DIVERGE the branch. Rename BEFORE merge."

But the pNNN merger protocol has no pre-ff rename step — it's docs-batch-specific. The merger runs: rebase → ff-try → done. By the time anyone notices the placeholders are still `9ddddddNN`, HEAD is already an ancestor and the check bails. The coordinator then has to fix manually on `sprint-1` post-merge. origin=coordinator.

Fix routes in the followup:
- **Route A:** wire `rename-unassigned` into merger step 3.5 (after rebase, before ff-try) when branch-name matches `docs-NNNNNN` OR diff contains `plan-9ddddddNN-*.md` files
- **Route B:** make the docs merger a distinct `subagent_type` with the step baked in

Route A is simpler — one conditional in the existing merger, no new agent type. The detection predicate (`_touched_batch_docs` at [`merge.py:752-774`](../../.claude/lib/onibus/merge.py)) already exists for the T-placeholder rewrite; the P-placeholder detection is `_find_placeholders` (same file). discovered_from=None.

## Tasks

### T1 — `fix(tooling):` merger step 3.5 — call `rename-unassigned` for docs branches

In the merger sequence (around `merge.py:758` step-3 rebase → step-4 ff-try), insert a conditional call:

```python
# Step 3.5 — docs-branch pre-ff rename.
# rename-unassigned at :986-993 REFUSES post-ff (HEAD ancestor of
# INTEGRATION_BRANCH → rename commit diverges). Must run HERE,
# after rebase (so the diff is clean) but before ff.
#
# Detection: either predicate suffices —
#   - branch name matches docs-NNNNNN (6-digit runid convention)
#   - diff contains plan-9ddddddNN-*.md (9-digit P-placeholder files)
# The second catches ad-hoc docs branches with nonstandard names.
_DOCS_BRANCH_RE = re.compile(r"^docs-\d{6}$")
has_placeholder_docs = bool(_find_placeholders(worktree))
is_docs_branch = _DOCS_BRANCH_RE.match(branch) is not None

if is_docs_branch or has_placeholder_docs:
    log.info(f"docs-branch detected ({branch}): running rename-unassigned pre-ff")
    rename_unassigned(worktree)  # commits the rename; ff-try then picks it up
```

The `rename_unassigned` function already commits its changes ([`merge.py`](../../.claude/lib/onibus/merge.py) post-`:993` — it's a full rewrite pass + `git commit`). The ff-try at step 4 then includes the rename commit in the merge.

**Edge case:** a docs branch with ZERO placeholders (all rows were batch-appends to existing docs, no new P-docs). `_find_placeholders` returns `[]`, but `_touched_batch_docs` is non-empty → T-placeholders need renaming. `rename_unassigned` already handles this (it scans for both P- and T-placeholders). The `is_docs_branch` branch-name check covers this.

### T2 — `test(tooling):` docs-branch merge renames placeholders pre-ff

In [`test_scripts.py`](../../.claude/lib/test_scripts.py):

```python
def test_docs_merger_renames_placeholders_pre_ff(tmp_path):
    """Regression: 4e755ae0 + followup-coordinator. docs-NNNNNN
    branch with plan-9ddddddNN-*.md placeholder files must have
    rename-unassigned run BEFORE ff — post-ff, merge-base
    --is-ancestor fires and rename refuses.

    Setup: git repo with sprint-1 + docs-123456 branch carrying
    plan-912345601-foo.md. Run merger. Assert:
      - plan-912345601-foo.md RENAMED to plan-0NNN-foo.md
      - dag.jsonl's 912345601 row REWRITTEN to 0NNN
      - merge-base --is-ancestor post-merge is True (ff happened)
      - NO stranded 9-digit tokens in the merged tree
    """
    # ... fixture setup ...
    # Key assertion: placeholder file gone, real-number file present.
    assert not (repo / ".claude/work/plan-912345601-foo.md").exists()
    assert any(p.name.startswith("plan-0") and "foo" in p.name
               for p in (repo / ".claude/work").glob("plan-*.md"))
    # No stranded tokens:
    assert "912345601" not in (repo / ".claude/dag.jsonl").read_text()
```

### T3 — `docs(tooling):` update merger protocol doc

Wherever the merger step-sequence is documented (likely `.claude/skills/merge-impl.md` or inline at `merge.py` module docstring), add step 3.5 to the numbered sequence. The `rio-planner` agent doc (`.claude/agents/rio-planner.md`) already warns about this in its "IMPORTANT — docs merger rename-unassigned" footer — after this plan lands, that footer can be simplified to "rename-unassigned runs automatically at merger step 3.5."

## Exit criteria

- `/nbr .#ci` green
- `grep 'docs-branch detected\|rename-unassigned pre-ff\|_DOCS_BRANCH_RE' .claude/lib/onibus/merge.py` → ≥2 hits (step 3.5 present)
- `nix develop -c pytest .claude/lib/test_scripts.py -k docs_merger_renames_placeholders` → 1 passed
- Next docs-merge (any `docs-NNNNNN` branch) requires ZERO coordinator manual-fixup for placeholders — verify by inspection on the first post-plan docs merge
- `grep '4e755ae0\|SECOND TIME\|manual.fixup' .claude/lib/onibus/merge.py` → ≥1 hit (precedent noted in the step-3.5 comment — why this is needed)

## Tracey

No domain markers — this is tooling, not spec-governed rio-build behavior.

## Files

```json files
[
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T1: insert step 3.5 after rebase before ff-try, near :758. Conditional on docs-NNNNNN branch name OR _find_placeholders nonempty"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T2: test_docs_merger_renames_placeholders_pre_ff fixture + assertions. P0304-T991893304 also touches this file (tracey_domains fix, diff section)"},
  {"path": ".claude/agents/rio-planner.md", "action": "MODIFY", "note": "T3: simplify 'IMPORTANT — docs merger rename-unassigned' footer post-fix"}
]
```

```
.claude/
├── lib/
│   ├── onibus/merge.py    # T1: step 3.5 near :758
│   └── test_scripts.py    # T2: regression test
└── agents/rio-planner.md  # T3: simplify footer
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "No plan deps — this is a tooling fix for the merger itself. The precedent 4e755ae0 was a one-off manual fixup; this makes it automatic."}
```

**Depends on:** none — standalone tooling fix.

**Conflicts with:** `.claude/lib/onibus/merge.py` is touched by T-placeholder work ([P0304](plan-0304-trivial-batch-p0222-harness.md)-T513 already documented the per-doc→contiguous convention change). `test_scripts.py` is shared with P0304-T991893304 (tracey_domains fix) — diff section, both additive.
