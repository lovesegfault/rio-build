# Plan 0414: Merger amend — branch-ref not updated (step 7.5 detached-HEAD window)

Coordinator finding during P0401 merge. Merger §7.5 at [`rio-impl-merger.md:120-148`](../../.claude/agents/rio-impl-merger.md) does:

```bash
git add .claude/dag.jsonl
git commit --amend --no-edit
```

P0401's merger reported `hash: d1449fad` (post-amend HEAD) but `sprint-1` branch-ref stayed at `4fc05cfe` (pre-amend). The coordinator's 4-check (`git merge-base --is-ancestor <hash> HEAD`) caught it: `d1449fad` was NOT an ancestor of `sprint-1` — it existed only in the reflog. Manual `git reset --hard d1449fad` in the main worktree recovered.

**Root cause:** `onibus merge ff-try` at [`git_ops.py:156-168`](../../.claude/lib/onibus/git_ops.py) runs `git merge --ff-only <branch>` with `cwd=REPO_ROOT` (Python-explicit) — always correct. But step 7.5's bare `git add .claude/dag.jsonl; git commit --amend --no-edit` runs in the merger agent's bash-cwd. If that cwd is (a) the plan worktree that step-7 just removed (`git worktree remove`), or (b) any worktree where HEAD isn't the integration branch, the amend creates a commit that isn't on sprint-1. `REPO_ROOT` at [`__init__.py:24`](../../.claude/lib/onibus/__init__.py) resolves per-worktree via `__file__` — so `onibus dag set-status` correctly writes to whichever worktree's `dag.jsonl` the invocation came from, but the follow-up `git commit --amend` is bash-cwd-dependent.

The memory note at [`rio-impl-merger.md:7`](../../.claude/agents/rio-impl-merger.md) says "step 7.5 you run the onibus CLI to flip the dag.jsonl status field — a mechanical edit via Bash" — the CLI does the edit correctly; it's the bash `git commit --amend` AFTER that's fragile. `count_bump()` at [`merge.py:142-145`](../../.claude/lib/onibus/merge.py) compounds: it records `git rev-parse INTEGRATION_BRANCH` with no `cwd=`, so it runs in `subprocess.run`'s default cwd (inherited from Python process, not guaranteed to be `REPO_ROOT`). If the merger's bash-cwd is wrong, count_bump records whatever `sprint-1` resolves to in that cwd — likely correct (git branches are repo-global, not worktree-local) but not guaranteed if cwd is outside the repo.

**Fix:** wrap step 7.5's compound action in `onibus merge dag-flip <N>` that does set-status + add + amend + count-bump with explicit `cwd=REPO_ROOT` at every git invocation. Same pattern as `ff_try`, `rebase_anchored` — Python owns the cwd, bash doesn't.

## Tasks

### T1 — `fix(harness):` merge.py — add `dag_flip` compound action with explicit cwd

NEW function in [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) (after `count_bump` at `:150`):

```python
def dag_flip(plan_num: int) -> DagFlipResult:
    """Step 7.5 compound: set-status DONE + amend into tip + count-bump.

    Explicit cwd=REPO_ROOT on EVERY git invocation — the merger agent's
    bash-cwd is not reliable (step 7 worktree-remove may leave it in a
    deleted directory, or it may be a plan worktree where HEAD ≠ $TGT).
    P0401's merger amended to d1449fad but sprint-1 stayed at 4fc05cfe —
    the amend ran in a context where the branch-ref didn't follow HEAD.

    Verifies the integration branch is checked out in REPO_ROOT before
    amending — fail loud if not (merger should ALWAYS have $TGT checked
    out in main; if not, something upstream went wrong)."""
    from onibus.dag import set_status, unblocked_by, render
    from onibus import REPO_ROOT, DAG_JSONL, INTEGRATION_BRANCH

    # Sanity: REPO_ROOT has the integration branch checked out. The amend
    # below moves whatever branch HEAD is — MUST be sprint-N.
    cur_branch = git("rev-parse", "--abbrev-ref", "HEAD", cwd=REPO_ROOT)
    if cur_branch != INTEGRATION_BRANCH:
        raise SystemExit(
            f"dag-flip: {REPO_ROOT} has {cur_branch!r} checked out, "
            f"expected {INTEGRATION_BRANCH!r}. The amend would move the "
            f"wrong branch. Check step-4 ff-try and step-7 cleanup ordering."
        )

    was_done = set_status(plan_num, "DONE")
    unblocked = unblocked_by(plan_num)
    render()  # regenerate dag.md

    git("add", str(DAG_JSONL.relative_to(REPO_ROOT)), cwd=REPO_ROOT)
    # Verify stage is non-empty (or amend is a no-op rewrite — harmless but
    # surprising). If dag.jsonl was already DONE for this plan, nothing staged.
    staged = git_try("diff", "--cached", "--name-only", cwd=REPO_ROOT) or ""
    if not staged.strip():
        return DagFlipResult(
            plan=plan_num, amend_sha="already-done",
            mc=count_bump(), unblocked=unblocked,
        )

    git("commit", "--amend", "--no-edit", cwd=REPO_ROOT)
    amend_sha = git("rev-parse", "--short", "HEAD", cwd=REPO_ROOT)

    # Post-amend sanity: the integration branch ref moved with HEAD.
    # `git commit --amend` with a branch checked out DOES move the branch;
    # this check proves we weren't in detached-HEAD (which would leave
    # the branch behind — the P0401 failure mode).
    tgt_sha = git("rev-parse", INTEGRATION_BRANCH, cwd=REPO_ROOT)
    head_sha = git("rev-parse", "HEAD", cwd=REPO_ROOT)
    if tgt_sha != head_sha:
        # This is the P0401 bug live — log + force-update the ref.
        git("update-ref", f"refs/heads/{INTEGRATION_BRANCH}", head_sha, cwd=REPO_ROOT)
        # Belt-and-suspenders; the cur_branch precondition above should
        # make this unreachable. If it fires, something about git's
        # amend semantics under this worktree layout is surprising.

    mc = count_bump()  # records tgt_sha (post-amend) via rev-parse INTEGRATION_BRANCH
    return DagFlipResult(
        plan=plan_num, amend_sha=amend_sha, mc=mc, unblocked=unblocked,
    )
```

NEW pydantic model `DagFlipResult` in [`.claude/lib/onibus/models.py`](../../.claude/lib/onibus/models.py):

```python
class DagFlipResult(BaseModel):
    """onibus merge dag-flip N — step 7.5 compound (set-status + amend + count-bump)."""
    plan: int
    amend_sha: str  # post-amend HEAD, or "already-done" if dag was pre-flipped
    mc: int         # merge-count after bump
    unblocked: list[int]  # plans entering frontier because of this flip
```

### T2 — `fix(harness):` merge.py — count_bump explicit cwd on rev-parse

MODIFY [`.claude/lib/onibus/merge.py:142-145`](../../.claude/lib/onibus/merge.py):

```python
# Before:
tip = subprocess.run(
    ["git", "rev-parse", INTEGRATION_BRANCH],
    capture_output=True, text=True,
).stdout.strip()

# After:
tip = subprocess.run(
    ["git", "rev-parse", INTEGRATION_BRANCH],
    cwd=REPO_ROOT,  # explicit — merger's bash-cwd may not be the repo
    capture_output=True, text=True,
).stdout.strip()
```

Git branches are repo-global so this USUALLY works without cwd, but if the merger's bash-cwd is outside the repo entirely (removed worktree directory → fs-gone), `git rev-parse` fails → `tip=""` → sha not recorded. The `if tip:` at `:146` silently skips — merge-count bumps but the mc→sha map gets a gap, and the next cadence computation (`_cadence_range` at `:153-182`) returns `None` for any window that crosses the gap. Silent cadence-agent-not-spawned.

### T3 — `feat(harness):` cli.py + rio-impl-merger.md — wire dag-flip subcommand, update step 7.5

MODIFY [`.claude/lib/onibus/cli.py`](../../.claude/lib/onibus/cli.py) — add `merge dag-flip <N>` subcommand wiring (near `count-bump` at `:304`):

```python
if c == "dag-flip":
    _emit(merge.dag_flip(args.plan))
    return
```

And the argparse:
```python
sp = g.add_parser("dag-flip"); sp.add_argument("plan", type=int)
```

MODIFY [`.claude/agents/rio-impl-merger.md:120-148`](../../.claude/agents/rio-impl-merger.md) — replace step 7.5's 7-command bash block with the single compound:

```markdown
### 7.5 DAG status flip + merge-count bump

```bash
.claude/bin/onibus merge dag-flip $N
```

Returns `DagFlipResult` JSON: `{plan, amend_sha, mc, unblocked}`. The
compound does set-status → git-add → amend → count-bump with explicit
`cwd=REPO_ROOT` at every git call — the merger agent's bash-cwd is not
reliable (P0401: amended to d1449fad but sprint-1 stayed at 4fc05cfe;
the amend ran in a context where the branch-ref didn't follow HEAD).

Include `unblocked` in `report.unblocked`. `amend_sha` IS the final
`hash` in the MergerReport. If `amend_sha == "already-done"`, the dag
row was pre-flipped (coordinator fast-path or re-invoked merger) —
harmless no-op, report it verbatim.

After dag-flip, fetch cadence:
```bash
.claude/bin/onibus merge cadence
```
```

Also remove the inline `onibus dag set-status $N DONE` + `onibus dag unblocked-by $N` + `onibus merge queue-consume` + `git add` + `git commit --amend` + `onibus merge count-bump` sequence — all absorbed into `dag-flip`. Keep `merge cadence` as a separate call (it's read-only, doesn't need cwd-pinning, and the merger agent includes it in the report separately). Keep `merge queue-consume "P$N"` OUT of dag-flip (it's state-file edit, not git — and T1's scope is the amend-fragility fix) OR fold it in for a true one-shot step-7.5. Prefer folding in: fewer moving parts for the merger, and `queue-consume` is also cwd-agnostic.

### T4 — `test(harness):` dag_flip regression — amend moves integration-branch ref

MODIFY [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py) near existing merge tests:

```python
def test_dag_flip_moves_integration_branch_ref(tmp_git_repo):
    """P0401 regression: amend creates new SHA, integration-branch ref MUST
    point at it. If the amend runs in detached-HEAD or wrong-worktree, the
    branch stays at pre-amend and the commit is reflog-only orphan."""
    repo = tmp_git_repo  # fixture: sprint-1 checked out, 1 commit, plan-99
    pre_amend = git("rev-parse", "sprint-1", cwd=repo)

    # Seed dag.jsonl with plan 99 status=UNIMPL
    (repo / ".claude" / "dag.jsonl").write_text('{"plan":99,"status":"UNIMPL",...}\n')

    result = merge.dag_flip(99)

    post_branch = git("rev-parse", "sprint-1", cwd=repo)
    post_head = git("rev-parse", "HEAD", cwd=repo)

    assert post_branch == post_head, "amend must move sprint-1, not just HEAD"
    assert post_branch != pre_amend, "amend must create new commit"
    assert result.amend_sha in post_branch  # short SHA prefix match
    assert result.mc == 1  # count bumped
    assert 99 not in result.unblocked  # (99 isn't a dep of anything in test dag)

def test_dag_flip_refuses_wrong_branch_checked_out(tmp_git_repo):
    """Precondition: REPO_ROOT must have integration-branch checked out.
    If merger's worktree layout somehow has main on a different branch,
    fail loud instead of amending the wrong branch."""
    repo = tmp_git_repo
    git("checkout", "-b", "not-sprint-1", cwd=repo)
    with pytest.raises(SystemExit, match="expected .sprint-1"):
        merge.dag_flip(99)
```

## Exit criteria

- `/nixbuild .#ci` green (or clause-4c if VM-flake)
- `grep 'def dag_flip' .claude/lib/onibus/merge.py` → 1 hit
- `grep 'class DagFlipResult' .claude/lib/onibus/models.py` → 1 hit
- `grep 'cwd=REPO_ROOT' .claude/lib/onibus/merge.py | wc -l` → ≥5 (T1's git calls + T2's count_bump fix; was 0 pre-fix for amend+rev-parse)
- `grep 'update-ref.*refs/heads' .claude/lib/onibus/merge.py` → ≥1 hit (T1's belt-and-suspenders fallback)
- `grep 'git commit --amend' .claude/agents/rio-impl-merger.md` → 0 hits (T3: bare bash amend removed; absorbed into dag-flip)
- `grep 'onibus merge dag-flip' .claude/agents/rio-impl-merger.md` → ≥1 hit (T3: single compound call)
- `grep '"dag-flip"' .claude/lib/onibus/cli.py` → ≥2 hits (T3: subcommand + argparse)
- `nix develop -c pytest .claude/lib/test_scripts.py -k 'dag_flip'` → ≥2 passed (T4)
- **T4 mutation:** comment out `git("commit", "--amend", …, cwd=REPO_ROOT)`'s `cwd=` kwarg in T1 → `test_dag_flip_moves_integration_branch_ref` still passes IF pytest's cwd happens to be the repo (likely). The REAL regression is only reproducible with a bash-cwd that's NOT the repo — hard to test in pytest. The `cur_branch != INTEGRATION_BRANCH` precondition + `tgt_sha != head_sha` post-check are the structural guards; T4's second test (`refuses_wrong_branch`) proves the precondition.
- **4-check integration:** next merger run after this lands → coordinator's `git merge-base --is-ancestor <report.hash> HEAD` passes (no more reflog-only orphan SHAs in merger reports)

## Tracey

No markers. Harness-tooling correctness; not spec-behavior.

## Files

```json files
[
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T1: +dag_flip() compound fn after :150 (set-status + amend + count-bump, explicit cwd=REPO_ROOT, branch-ref sanity + update-ref fallback). T2: :142-145 count_bump rev-parse +cwd=REPO_ROOT"},
  {"path": ".claude/lib/onibus/models.py", "action": "MODIFY", "note": "T1: +DagFlipResult pydantic model"},
  {"path": ".claude/lib/onibus/cli.py", "action": "MODIFY", "note": "T3: +merge dag-flip <N> subcommand wiring + argparse near :304,:532"},
  {"path": ".claude/agents/rio-impl-merger.md", "action": "MODIFY", "note": "T3: replace step-7.5 7-command bash block (:124-136) with single 'onibus merge dag-flip $N' + DagFlipResult consumption"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T4: +test_dag_flip_moves_integration_branch_ref + test_dag_flip_refuses_wrong_branch_checked_out"}
]
```

```
.claude/lib/onibus/
├── merge.py             # T1: +dag_flip(); T2: count_bump cwd fix
├── models.py            # T1: +DagFlipResult
└── cli.py               # T3: +merge dag-flip subcommand
.claude/agents/
└── rio-impl-merger.md   # T3: step-7.5 → single compound call
.claude/lib/
└── test_scripts.py      # T4: +2 regression tests
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [401, 346, 358, 319], "note": "Coordinator-discovered during P0401 merge (discovered_from=401, but no hard dep — P0401 is already merged, the bug is structural not introduced-by-P0401). Soft-dep P0346 (phantom-amend behind-check — related amend machinery in git_ops.py behind_check; non-overlapping with merge.py dag_flip). Soft-dep P0358 (phantom-amend stacked detection — same). Soft-dep P0319 (merge-shas pre-amend dangling — fixed the amend-THEN-count-bump ordering; T1's dag_flip keeps that ordering correct by construction). No hard deps — applies to whatever merge.py HEAD is. PREFERRED ORDER: land early in a quiet window; next merger run is the production smoke-test."}
```

**Depends on:** none — standalone harness fix.

**Conflicts with:** [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) — [P0401](plan-0401-docs-writer-lazy-t-assignment.md) (DONE) added `_T_PLACEHOLDER` + `_find_t_placeholders` at `:295+`; T1 adds `dag_flip` after `:150` — non-overlapping. [P0304](plan-0304-trivial-batch-p0222-harness.md)-T17 bumps `_LEASE_SECS` at `:58`; non-overlapping. [`.claude/agents/rio-impl-merger.md`](../../.claude/agents/rio-impl-merger.md) — [P0304](plan-0304-trivial-batch-p0222-harness.md)-T110 adds a Clause-4 fast-path decision tree section; non-overlapping with step-7.5 at `:120-148`.
