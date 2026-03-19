# Plan 995431203: `phantom_amend: bool` on BehindCheck — auto-detect merger-orphaned bases

Consolidator finding (mc=84). The merger's step-7.5 `git commit --amend --no-edit` orphans any worktree that rebased onto the pre-amend SHA during the ff-to-amend window. Six+ hits this sprint (p266, p338, p260, p322, p264, p326 — plus at least 3 earlier recorded in prior memory). Every time, the validator sees:

- `behind=1`
- `file_collision=[".claude/dag.jsonl"]` (and/or `.claude/state/merge-shas.jsonl`)
- `trivial_rebase=false` (collision list non-empty)

…then hand-writes the same diagnosis ("phantom-amend, rebase --onto") each time. ~30s per hit × ~1 hit/merge during high-throughput DAG. The check is mechanical; `BehindCheck` should surface it.

**Phantom-amend signature** (all four must hold):

1. `behind == 1` (exactly one commit — the amended one)
2. `file_collision ⊆ {".claude/dag.jsonl", ".claude/state/merge-shas.jsonl"}` (the only files the amend touches)
3. `git for-each-ref --contains <merge-base>` → empty (base is reflog-only — no ref points at or beyond it)
4. `git log --oneline -1 <merge-base>` and `git log --oneline -1 <integration-tip>` have the same commit message (amend rewrote SHA, kept message)

Clause 4 is belt-and-suspenders — clauses 1-3 are sufficient in practice (the ONLY way to have an orphaned merge-base with a single-commit difference limited to dag.jsonl is the amend).

## Tasks

### T1 — `feat(harness):` phantom_amend field on BehindCheck

MODIFY [`.claude/lib/onibus/models.py`](../../.claude/lib/onibus/models.py) at [`:468-476`](../../.claude/lib/onibus/models.py):

```python
class BehindCheck(BaseModel):
    """onibus merge behind-check WORKTREE — validator's step-0 compound query.
    Was: `git rev-list --count HEAD..$TGT` + 3-dot file-intersection bash."""
    behind: int
    file_collision: list[str] = Field(
        description="3-dot intersection: files THIS worktree changed AND $TGT "
        "added since merge-base. Empty → rebase will be trivial (no conflicts)."
    )
    trivial_rebase: bool = Field(description="behind > 0 and file_collision empty")
    phantom_amend: bool = Field(
        default=False,
        description="behind==1 AND file_collision⊆{dag.jsonl, merge-shas.jsonl} "
        "AND merge-base is orphaned (no ref contains it). Merger's step-7.5 "
        "amend orphaned this worktree's base — git rebase auto-drops the "
        "patch-already-upstream commit; safe to rebase mechanically."
    )
```

MODIFY [`.claude/lib/onibus/git_ops.py`](../../.claude/lib/onibus/git_ops.py) at [`behind_check`](../../.claude/lib/onibus/git_ops.py) (`:171-189`):

```python
_PHANTOM_AMEND_FILES = frozenset({
    ".claude/dag.jsonl",
    ".claude/state/merge-shas.jsonl",
})


def behind_check(worktree: Path) -> BehindCheck:
    """Validator's step-0 compound query. Was: rev-list + 3-dot diff bash.
    …(existing docstring)…

    phantom_amend: the merger's step-7.5 `git commit --amend --no-edit`
    rewrites HEAD after ff, orphaning any worktree that rebased onto the
    pre-amend SHA in the ff→amend window. Detected by: exactly-1 behind,
    collision limited to dag.jsonl/merge-shas.jsonl (the amend's files),
    and the merge-base being reflog-only (no ref contains it). When true,
    `git rebase $TGT` auto-drops the patch-already-upstream commit."""
    behind_s = git_try("rev-list", "--count", f"HEAD..{INTEGRATION_BRANCH}", cwd=worktree)
    behind = int(behind_s) if behind_s and behind_s.isdigit() else 0
    collision: list[str] = []
    phantom = False
    if behind > 0:
        mine = set((git_try("diff", f"{INTEGRATION_BRANCH}...HEAD", "--name-only", cwd=worktree) or "").splitlines())
        theirs = set((git_try("diff", f"HEAD...{INTEGRATION_BRANCH}", "--name-only", cwd=worktree) or "").splitlines())
        collision = sorted(mine & theirs)
        # Phantom-amend detection: 1 behind, collision ⊆ amend-files, base orphaned.
        if behind == 1 and set(collision) <= _PHANTOM_AMEND_FILES:
            base = git_try("merge-base", "HEAD", INTEGRATION_BRANCH, cwd=worktree)
            if base:
                containing = git_try("for-each-ref", "--contains", base, cwd=worktree)
                phantom = not (containing or "").strip()
    return BehindCheck(
        behind=behind,
        file_collision=collision,
        trivial_rebase=behind > 0 and not collision,
        phantom_amend=phantom,
    )
```

### T2 — `test(harness):` phantom_amend detection unit test

NEW test in [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py). Constructs the phantom-amend scenario in a scratch git repo:

```python
def test_behind_check_phantom_amend_detection(tmp_path):
    """phantom_amend flag fires when merge-base is orphaned post-amend.
    Scenario: integration branch ff→amend (the merger's step-7.5 dance),
    worktree rebased onto pre-amend SHA."""
    repo = _mk_repo(tmp_path)
    # Seed dag.jsonl so the amend has something to change.
    (repo / ".claude").mkdir(parents=True)
    (repo / ".claude" / "dag.jsonl").write_text('{"plan":1,"status":"UNIMPL"}\n')
    _git(repo, "add", ".")
    _git(repo, "commit", "-m", "seed")
    base_sha = _git(repo, "rev-parse", "HEAD").strip()

    # Integration branch: ff (one real commit), then amend dag.jsonl.
    _git(repo, "checkout", "-b", "sprint-1")
    (repo / "real-change.txt").write_text("feature")
    _git(repo, "add", ".")
    _git(repo, "commit", "-m", "feat: real change")
    pre_amend = _git(repo, "rev-parse", "HEAD").strip()
    # The amend — flips dag status, rewrites HEAD.
    (repo / ".claude" / "dag.jsonl").write_text('{"plan":1,"status":"DONE"}\n')
    _git(repo, "add", ".claude/dag.jsonl")
    _git(repo, "commit", "--amend", "--no-edit")
    post_amend = _git(repo, "rev-parse", "HEAD").strip()
    assert pre_amend != post_amend, "amend must rewrite SHA"

    # Worktree: branched from pre_amend (the orphaned SHA).
    wt = tmp_path / "p999"
    _git(repo, "worktree", "add", str(wt), pre_amend)
    # Worktree also touches dag.jsonl (as any plan-doc commit does when
    # merge.py:rename-unassigned flips its row) — this is the collision.
    (wt / ".claude" / "dag.jsonl").write_text('{"plan":1,"status":"UNIMPL"}\n{"plan":2,"status":"UNIMPL"}\n')
    _git(wt, "add", ".claude/dag.jsonl")
    _git(wt, "commit", "-m", "docs: add plan-2")

    # behind_check from the worktree: phantom_amend should be True.
    # (INTEGRATION_BRANCH needs to resolve — patch or use env override.)
    bc = behind_check(wt)
    assert bc.behind == 1
    assert bc.file_collision == [".claude/dag.jsonl"]
    assert bc.trivial_rebase is False  # collision non-empty
    assert bc.phantom_amend is True, (
        f"expected phantom_amend=True: pre_amend={pre_amend} is orphaned "
        f"(for-each-ref should be empty), collision={bc.file_collision}"
    )

    # Negative: worktree that branched from post_amend sees behind=0, no phantom.
    wt2 = tmp_path / "p998"
    _git(repo, "worktree", "add", str(wt2), post_amend)
    bc2 = behind_check(wt2)
    assert bc2.behind == 0
    assert bc2.phantom_amend is False
```

**INTEGRATION_BRANCH override:** `git_ops.py` reads `INTEGRATION_BRANCH` from [`.claude/integration-branch`](../../.claude/integration-branch) via `__file__`-relative path. The test's scratch repo won't have it. Either (a) write `.claude/integration-branch` in `_mk_repo`, or (b) monkeypatch `INTEGRATION_BRANCH` to `"sprint-1"` for the test. **Check at dispatch** which is cleaner (depends on whether `git_ops.py` re-reads on each call or caches at import).

### T3 — `docs(harness):` validator agent — phantom_amend handling

MODIFY [`.claude/agents/rio-impl-validator.md`](../../.claude/agents/rio-impl-validator.md). Find the step-0 `behind-check` section and add:

```markdown
**When `phantom_amend: true`:** the merger's step-7.5 amend orphaned this
worktree's base. Mechanical fix: `git rebase <integration-branch>` from
the worktree — git auto-drops the "patch contents already upstream"
commit. No validation-relaunch needed; `behind-check` post-rebase will
show `behind=0`. This is NOT a real collision despite `trivial_rebase=false`;
the collision is dag.jsonl/merge-shas.jsonl only, and the amend rewrote
exactly those.
```

## Exit criteria

- `/nbr .#ci` green (tracey + pre-commit only for `.claude/`-only change — clause-4(a) docs-only fast-path eligible)
- T1: `grep -c 'phantom_amend' .claude/lib/onibus/models.py .claude/lib/onibus/git_ops.py` → ≥3 (field decl + computation + docstring mention)
- T1: `grep '_PHANTOM_AMEND_FILES' .claude/lib/onibus/git_ops.py` → ≥2 (const def + use in behind_check)
- T2: `nix develop -c pytest .claude/lib/test_scripts.py -k phantom_amend` → 1 passed
- T2 mutation: change the `for-each-ref --contains` to always return a non-empty string → test fails on `phantom_amend is True`
- T3: `grep -c 'phantom_amend' .claude/agents/rio-impl-validator.md` → ≥1

## Tracey

No markers. Harness tooling (`.claude/lib/onibus/` + agent file) is not spec'd in `docs/src/`.

## Files

```json files
[
  {"path": ".claude/lib/onibus/models.py", "action": "MODIFY", "note": "T1: BehindCheck +phantom_amend field at :468-476"},
  {"path": ".claude/lib/onibus/git_ops.py", "action": "MODIFY", "note": "T1: behind_check computes phantom_amend (for-each-ref --contains check) at :171-189; +_PHANTOM_AMEND_FILES const"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T2: +test_behind_check_phantom_amend_detection (scratch-repo amend scenario)"},
  {"path": ".claude/agents/rio-impl-validator.md", "action": "MODIFY", "note": "T3: phantom_amend handling in step-0 section (mechanical rebase, no relaunch)"}
]
```

```
.claude/lib/onibus/
├── models.py              # T1: +phantom_amend field
└── git_ops.py             # T1: +phantom_amend computation
.claude/lib/
└── test_scripts.py        # T2: detection unit test
.claude/agents/
└── rio-impl-validator.md  # T3: handling docs
```

## Dependencies

```json deps
{"deps": [306], "soft_deps": [304], "note": "P0306 (DONE) — behind_check exists at git_ops.py:171, BehindCheck model at models.py:468; T1 extends both. discovered_from=consolidator(mc84). 6+ session hits (p266/p338/p260/p322/p264/p326). Soft-dep P0304-T1: also touches models.py at :322 (PlanFile regex) — different section, non-overlapping. Soft-conflict .claude/lib/test_scripts.py: P0304-T30, P0311-T9, P0322-T3, P0323-T4 all add tests there — additive, distinct test-fn names."}
```

**Depends on:** [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md) — merged (DONE). `behind_check` and `BehindCheck` exist with the 3-dot fix.

**Conflicts with:** [`.claude/lib/onibus/models.py`](../../.claude/lib/onibus/models.py) — [P0304](plan-0304-trivial-batch-p0222-harness.md) T1 touches `:322` (PlanFile regex), T1 here touches `:468-476` — non-overlapping. [`.claude/lib/onibus/git_ops.py`](../../.claude/lib/onibus/git_ops.py) — no other UNIMPL plan in collision matrix. [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py) — additive test-fn, shares with 4 other plans, no name collision. [`rio-impl-validator.md`](../../.claude/agents/rio-impl-validator.md) — session-cached; lands on next worktree-add.
