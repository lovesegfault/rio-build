# Plan 0325: rename-unassigned — content-rewrite skips plan docs post-ff-merge

**Bughunter finding (mc-35, empirically verified in scratch repo).** [`merge.py:332`](../../.claude/lib/onibus/merge.py) in `_rewrite_and_rename` derives the content-rewrite set from `git diff --name-only INTEGRATION_BRANCH...HEAD`. Three-dot diff = "commits reachable from HEAD but not from INTEGRATION_BRANCH". When the coordinator fast-forwards the docs branch to sprint-1 BEFORE calling `rename-unassigned`, docs-tip == sprint-1-tip → the three-dot range is empty → `touched` is `['.claude/dag.jsonl']` only (force-appended at `:338`).

The defect is asymmetric. The content-rewrite loop at [`merge.py:341-353`](../../.claude/lib/onibus/merge.py) iterates `touched` — skips the plan .md files. But the `git mv` loop at [`merge.py:355-361`](../../.claude/lib/onibus/merge.py) iterates `mapping` — which comes from `_find_placeholders` at [`merge.py:308`](../../.claude/lib/onibus/merge.py), which reads `git ls-files` (filesystem state, ff-merge-independent). So: file renamed to `plan-0318-foo.md`, content still says `P992687001`. `git add -u` + `commit` succeeds — broken tree committed.

**Manifested twice:** docs-926870 at [`55f1d050`](https://github.com/search?q=55f1d050&type=commits), docs-928654 at [`8cb27862`](https://github.com/search?q=8cb27862&type=commits). Introduced in [`b2980679`](https://github.com/search?q=b2980679&type=commits) (the onibus consolidation — before that, rename was a bash loop in the merger agent that globbed by placeholder, not by diff).

**Why the call-order happens:** the fast-path merge protocol has the coordinator do `git merge --ff-only docs-<runid>` on sprint-1, THEN call `onibus merge rename-unassigned docs-<runid>` as cleanup. The wrong order. The interim caller fix is "rename BEFORE ff" — but the tool should be call-order-robust. A tool that silently corrupts when called in the wrong order is a tool that will corrupt again.

**Two fix options.** Option 1 (guard): fail loud if `git merge-base --is-ancestor HEAD INTEGRATION_BRANCH` (HEAD is already an ancestor → ff already happened). Turns silent corruption into loud failure; caller still has to fix their sequence. Option 2 (derive from mapping): build the rewrite set from `mapping` itself — each `Rename` has `placeholder` + `slug`, so the target filename is `.claude/work/plan-{placeholder}-{slug}.md`, no git diff needed. Call-order-independent by construction.

**Option 2 is the primary fix.** Option 1 is a cheap secondary guard (belt-and-suspenders — if option 2 ever regresses, option 1 catches it).

## Tasks

### T1 — `fix(harness):` derive rewrite set from mapping, not git diff

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) at `_rewrite_and_rename` (`:329-353`):

```python
def _rewrite_and_rename(worktree: Path, mapping: list[Rename]) -> None:
    # Rewrite set derived from mapping, not git diff. Three-dot diff
    # (INTEGRATION_BRANCH...HEAD) is empty when the docs branch has
    # already been ff-merged — docs-tip == sprint-1-tip → no commits
    # unique to HEAD → rewrite loop silently skipped every .md file
    # while git-mv (below, mapping-driven) still ran. Manifested
    # 2× (docs-926870 @ 55f1d050, docs-928654 @ 8cb27862) before
    # bughunter-mc35 scratch-repo'd it.
    #
    # The mapping already has everything needed: placeholder + slug →
    # filename. The plan .md file is where the placeholder lives in
    # prose (P924999901, [P924999901](plan-924999901-...)). dag.jsonl
    # carries it as {"plan": 924999901, ...}. Rewrite both; no other
    # file type carries placeholders (by construction — the planner
    # only writes to .claude/work/plan-*.md and dag.jsonl).
    touched: list[str] = [
        f".claude/work/plan-{r.placeholder}-{r.slug}.md" for r in mapping
    ]
    touched.append(".claude/dag.jsonl")

    for rel in touched:
        p = worktree / rel
        # ... rest unchanged (read_text, replace loop, atomic_write_text)
```

The `try: read_text() except FileNotFoundError: continue` guard at `:343-346` stays — if a placeholder was somehow deleted between `_find_placeholders` and here, the skip is correct (the `git mv` below will fail loudly, which is the right outcome).

**Deleted:** the `git diff --name-only` call at `:330-335`, the `.endswith('.md')` filter, the `if ".claude/dag.jsonl" not in touched` conditional (always not-in now — just append unconditionally).

**What about batch-appended .md files?** The planner also appends to existing batch docs (e.g. P0311 T10). Those files have REAL plan numbers in their filenames (`plan-0311-*.md`) — no placeholder, not in `mapping`, not in the rewrite set. Correct: batch appends don't create placeholders; nothing to rewrite there. The old diff-based approach would have included them in `touched` (they're `.md` files that changed) and run the replace loop on them as a no-op (no placeholder string present → `new == text` → skip). Dropping them from `touched` is a no-op-elimination.

### T2 — `fix(harness):` add is-ancestor guard at rename_unassigned entry

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) at `rename_unassigned` (`:364-368`):

```python
def rename_unassigned(branch: str) -> RenameReport:
    worktree = _worktree_for(branch)

    # Fail loud if this branch has already been ff-merged. T1 makes the
    # rewrite logic call-order-robust anyway, but this catches a caller
    # who's about to do something confused: if HEAD is already an ancestor
    # of INTEGRATION_BRANCH, there's nothing left to merge AFTER rename.
    # Either the caller already merged (wrong order — rename commits a
    # NEW commit on top, diverging the branch) or they're calling rename
    # on a branch that was never a docs branch.
    rc = subprocess.run(
        ["git", "merge-base", "--is-ancestor", "HEAD", INTEGRATION_BRANCH],
        cwd=worktree,
    ).returncode
    if rc == 0:
        raise SystemExit(
            f"rename-unassigned: {branch!r} HEAD is already an ancestor of "
            f"{INTEGRATION_BRANCH!r}. If you ff-merged before renaming, the "
            f"rename commit will DIVERGE the branch. Rename BEFORE merge, "
            f"or skip rename (placeholders are already live on "
            f"{INTEGRATION_BRANCH} — fix them there manually)."
        )

    placeholders = _find_placeholders(worktree)
    # ... rest unchanged
```

`--is-ancestor A B` exits 0 if A is reachable from B. Post-ff-merge, HEAD (docs branch tip) is reachable from INTEGRATION_BRANCH → rc=0 → SystemExit. Pre-merge (normal case), HEAD has commits INTEGRATION_BRANCH doesn't → rc=1 → proceed.

**Why SystemExit, not an exception:** this is a CLI entrypoint. `SystemExit` with a message → stderr + nonzero exit → coordinator sees it in the subprocess output. An exception would print a traceback, which is noise for a caller-error-not-code-bug condition.

### T3 — `test(harness):` regression test — post-ff rename still rewrites content

MODIFY [`.claude/lib/test_onibus_dag.py`](../../.claude/lib/test_onibus_dag.py) — add near the existing merge tests (around `:595` is the `count_bump` cluster; place this after `:780` where other rename/merge tests live, or grep for `rename_unassigned` to find the right neighborhood):

```python
def test_rewrite_and_rename_derives_from_mapping_not_diff(tmp_repo_patched: tuple[Path, Path]):
    """P0325 T1 regression: _rewrite_and_rename must derive the
    rewrite set from mapping[].placeholder, not from git diff.

    The bug: touched = git diff --name-only INTEGRATION_BRANCH...HEAD.
    Post-ff-merge, docs-tip == sprint-1-tip → three-dot range EMPTY →
    touched = ['.claude/dag.jsonl'] only → plan .md rewrite SKIPPED.
    But git-mv (mapping-driven) still ran. Result: plan-0318-foo.md
    exists, content says P992687001. Manifested docs-926870 @ 55f1d050,
    docs-928654 @ 8cb27862.

    This test doesn't simulate the ff-merge directly — it calls
    _rewrite_and_rename with an empty-diff-equivalent state and asserts
    the .md content got rewritten anyway. The discriminator is: does
    the function READ the diff, or does it READ the mapping?"""
    import onibus.merge
    from onibus.merge import Rename
    tmp_repo, _ = tmp_repo_patched

    # Seed: plan doc with placeholder in content, on a branch that's
    # AT INTEGRATION_BRANCH tip (simulating post-ff state: git diff
    # INTEGRATION_BRANCH...HEAD would be empty).
    work = tmp_repo / ".claude" / "work"
    work.mkdir(parents=True, exist_ok=True)
    plan_doc = work / "plan-912345601-test-slug.md"
    plan_doc.write_text(
        "# Plan 912345601: test\n\n"
        "See [P912345601](plan-912345601-test-slug.md).\n"
    )
    dag = tmp_repo / ".claude" / "dag.jsonl"
    dag.write_text('{"plan": 912345601, "title": "t", "deps": []}\n')

    # Commit it — we're on INTEGRATION_BRANCH, so after this commit,
    # diff INTEGRATION_BRANCH...HEAD is STILL empty (HEAD == branch tip).
    # The old diff-based code would see zero .md files in touched.
    _git(tmp_repo, "add", "-A")
    _git(tmp_repo, "commit", "-m", "docs: add placeholder", "--no-verify")

    # Precondition: three-dot diff is empty. This is the bug's trigger
    # condition. If this assert fails, the test fixture is wrong and
    # the test proves nothing.
    from onibus import INTEGRATION_BRANCH
    diff = _git(tmp_repo, "diff", "--name-only", f"{INTEGRATION_BRANCH}...HEAD")
    assert diff == "", f"precondition: three-dot diff must be empty, got {diff!r}"

    mapping = [Rename(placeholder="912345601", assigned=318, slug="test-slug")]
    onibus.merge._rewrite_and_rename(tmp_repo, mapping)

    # The file was renamed (git mv ran — mapping-driven, always worked).
    assert not plan_doc.exists()
    renamed = work / "plan-0318-test-slug.md"
    assert renamed.exists(), "git mv should have renamed the file"

    # THE BUG: content must be rewritten too. Old code left P912345601
    # in the content because the rewrite loop never saw this file.
    content = renamed.read_text()
    assert "912345601" not in content, (
        f"placeholder must be rewritten in content — found in:\n{content}"
    )
    assert "P0318" in content, "assigned number must appear in content"
    assert "[P0318](plan-0318-test-slug.md)" in content, (
        "self-link must be fully rewritten (both label and href)"
    )

    # dag.jsonl also rewritten (integer form, not zero-padded).
    dag_content = dag.read_text()
    assert '"plan": 318' in dag_content
    assert "912345601" not in dag_content


def test_rename_unassigned_rejects_post_ff_branch(tmp_repo_patched: tuple[Path, Path]):
    """P0325 T2: rename_unassigned fails loud if the branch is
    already an ancestor of INTEGRATION_BRANCH. This is the secondary
    guard — T1 makes the rewrite correct anyway, but this catches a
    caller about to diverge the branch with a post-ff rename commit."""
    import onibus.merge
    import pytest
    tmp_repo, _ = tmp_repo_patched

    # tmp_repo is init'd ON INTEGRATION_BRANCH. Create a docs branch
    # at the same tip — it's already an ancestor (it IS the tip).
    _git(tmp_repo, "branch", "docs-test")

    # _worktree_for needs a worktree entry. Shim it — the guard fires
    # before any worktree interaction beyond path resolution.
    # Check at dispatch: _worktree_for may need a real worktree; if so,
    # use `git worktree add` instead of this monkeypatch.
    # (The guard reads HEAD via `cwd=worktree`; tmp_repo is HEAD ==
    # INTEGRATION_BRANCH already.)
    with pytest.raises(SystemExit, match="already an ancestor"):
        # If _worktree_for needs the branch checked out somewhere, add
        # the worktree here. The guard's `cwd=worktree` + `HEAD` means
        # the worktree's HEAD must be the docs branch tip.
        onibus.merge.rename_unassigned("docs-test")
```

**Check at dispatch:** `_worktree_for` at [`merge.py:300-305`](../../.claude/lib/onibus/merge.py) reads `git worktree list --porcelain` and matches `refs/heads/<branch>`. A branch without a worktree raises `SystemExit("no worktree for branch")` — which would shadow the guard's SystemExit. The test may need `git worktree add ../docs-test docs-test` before calling `rename_unassigned`. Alternatively, monkeypatch `_worktree_for` to return `tmp_repo` directly (the guard only needs a cwd where `HEAD` resolves). Pick whichever is cleaner at dispatch.

**If [P0324](plan-0324-conftest-tmp-repo-patched-fixture.md) hasn't landed yet:** replace `tmp_repo_patched: tuple[Path, Path]` with the inline 5-line block (`tmp_repo: Path, monkeypatch` + state.mkdir + chdir + two setattrs). The tests work either way; the fixture is just cleaner.

## Exit criteria

- `nix develop -c pytest .claude/lib/test_onibus_dag.py -k 'rewrite_and_rename or rename_unassigned_rejects'` → 2 passed
- `grep 'git diff.*name-only.*INTEGRATION_BRANCH' .claude/lib/onibus/merge.py` → 0 hits in `_rewrite_and_rename` (T1: diff-based touched derivation removed)
- `grep 'merge-base.*is-ancestor' .claude/lib/onibus/merge.py` → ≥1 hit in `rename_unassigned` (T2: guard present)
- **The precondition self-check is load-bearing:** `test_rewrite_and_rename_derives_from_mapping_not_diff` asserts `diff == ""` BEFORE calling `_rewrite_and_rename`. Without this, the test would pass against the OLD code when the fixture happens to produce a non-empty diff (e.g. if a future conftest change makes `tmp_repo` init on a different branch). The precondition proves the test is exercising the bug's trigger condition.
- **Belt-and-suspenders verified:** run `test_rename_unassigned_rejects_post_ff_branch` with T1 reverted (locally, not committed) — it must still fail with SystemExit. The guard is independent of the rewrite fix.
- `nix develop -c pytest .claude/lib/` → full harness test suite green (no regression in existing rename/merge tests)

## Tracey

No markers. This plan fixes `.claude/lib/onibus/merge.py` — harness tooling, not spec'd in `docs/src/components/`. No `r[...]` annotations apply.

## Files

```json files
[
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T1: _rewrite_and_rename :329-339 derive touched from mapping not diff (~8L replaced). T2: rename_unassigned :364-368 add is-ancestor guard (~12L inserted)"},
  {"path": ".claude/lib/test_onibus_dag.py", "action": "MODIFY", "note": "T3: +test_rewrite_and_rename_derives_from_mapping_not_diff + test_rename_unassigned_rejects_post_ff_branch (~65L, place near existing merge/rename tests)"}
]
```

```
.claude/lib/
├── onibus/
│   └── merge.py              # T1+T2: mapping-derived touched + is-ancestor guard
└── test_onibus_dag.py        # T3: 2 regression tests
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [0324, 323], "note": "No hard deps — bug is live on sprint-1 since b2980679 (the onibus consolidation), fix is self-contained. Bughunter origin (mc-35, empirically verified in scratch repo). Priority: 75 — highest-priority harness bug, silently corrupting fast-path merges. Soft-dep P0324: T3's tests want tmp_repo_patched fixture if available; fall back to inline block if not. Soft-conflict P0323: also edits merge.py at :145-185 (count_bump/_cadence_range); this plan edits :329-368 (_rewrite_and_rename/rename_unassigned). Different functions, ~150 lines apart, textual merge clean. Both can land in either order."}
```

**Depends on:** none. The bug is at [`merge.py:332`](../../.claude/lib/onibus/merge.py) live since [`b2980679`](https://github.com/search?q=b2980679&type=commits). The fix is self-contained.

**Ship order:** ASAP. Two fast-path merges already corrupted (docs-926870, docs-928654). Every fast-path merge until this lands risks a third. The interim caller fix ("rename BEFORE ff") is in place but depends on the coordinator remembering — a tool fix removes the memory load.

**Conflicts with:**
- [`merge.py`](../../.claude/lib/onibus/merge.py) — [P0323](plan-0323-mergesha-pydantic-model.md) edits `:145-185` (count_bump + _cadence_range MergeSha model adoption). This plan edits `:329-368`. Zero line overlap. Both can land in either order; rebase is trivial.
- [`test_onibus_dag.py`](../../.claude/lib/test_onibus_dag.py) — [P0324](plan-0324-conftest-tmp-repo-patched-fixture.md) T2 rewrites ~15 existing test fns. T3 here ADDS 2 new test fns. Non-overlapping — P0324 modifies, this adds. If P0324 lands first, T3's tests use `tmp_repo_patched`; if not, inline the setup block (noted in T3 prose).
