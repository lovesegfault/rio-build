# Plan 0358: phantom_amend detection for stacked amends — behind>1 case

Consolidator mc=110. [P0346](plan-0346-phantom-amend-behindcheck.md) added `phantom_amend: bool` to `BehindCheck`, but the detection at [`git_ops.py:208`](../../.claude/lib/onibus/git_ops.py) is gated on `if behind == 1:`. When two (or more) merger amends happen in sequence before a worktree rebases — which happens during high-throughput DAG when one worktree's validator is slow — the worktree sees `behind >= 2` and the phantom detection **doesn't fire**. The validator falls back to hand-diagnosing a multi-commit phantom, which is the same ~30s tax P0346 was meant to eliminate.

The stacked-amend signature: N commits behind where EACH of those N commits on the integration branch is an amend of the worktree's base commit. The oldest exclusively-ours commit (same as P0346's `pre_amend = ours_only[0]`) differs from integration-tip only in `_PHANTOM_AMEND_FILES`, regardless of N. The check doesn't need to inspect each intermediate amend — just compare the pre-amend candidate against the current integration tip.

Current code at [`:208-224`](../../.claude/lib/onibus/git_ops.py):
```python
if behind == 1:
    ours_only = (git_try("rev-list", "--reverse", "HEAD", f"^{INTEGRATION_BRANCH}", ...) or "").splitlines()
    if ours_only:
        pre_amend = ours_only[0]
        amend_diff = set((git_try("diff", "--name-only", pre_amend, INTEGRATION_BRANCH, ...) or "").splitlines())
        amend_diff.discard("")
        if amend_diff and amend_diff <= _PHANTOM_AMEND_FILES:
            msg_pre = git_try("log", "-1", "--format=%s", pre_amend, ...)
            msg_tip = git_try("log", "-1", "--format=%s", INTEGRATION_BRANCH, ...)
            phantom = msg_pre is not None and msg_pre == msg_tip
```

The `behind == 1` guard is too strict; the `amend_diff <= _PHANTOM_AMEND_FILES` + message-match check is sufficient for any N. However, generalizing naively risks false positives: a worktree genuinely behind by 2 real commits (not amends) where the first commit only touched `dag.jsonl` would trigger. The message-match is what distinguishes amend (`--no-edit` keeps message) from a real commit.

## Entry criteria

- [P0346](plan-0346-phantom-amend-behindcheck.md) merged — `phantom_amend` field exists, `_PHANTOM_AMEND_FILES` constant exists, single-amend detection works

## Tasks

### T1 — `feat(harness):` generalize phantom_amend to behind>=1

MODIFY [`.claude/lib/onibus/git_ops.py`](../../.claude/lib/onibus/git_ops.py) at `:208`. Change `if behind == 1:` → `if behind >= 1:`. The subsequent check (oldest-exclusively-ours commit differs from integration tip only in `_PHANTOM_AMEND_FILES` + same message) already works for stacked amends — each amend rewrites the SAME commit with the same message, accumulating only `dag.jsonl` changes.

**Additional guard:** The message-match check at `:222-224` compares `pre_amend` (oldest exclusively-ours commit) against `INTEGRATION_BRANCH` tip. For stacked amends this still works: every amend in the stack has the SAME message (each `--amend --no-edit` preserves it), so integration-tip's message matches pre_amend's. For a genuine 2-commit-behind case, integration-tip is a DIFFERENT commit with a different message → phantom stays False. No extra guard needed; the message-match already discriminates.

**Docstring update** at `:187-191`:
```python
"""...
phantom_amend: the merger's step-7.5 `git commit --amend --no-edit`
rewrites the tip after ff, orphaning any worktree that rebased onto the
pre-amend SHA during the ff→amend window. Multiple amends in sequence
(high-throughput DAG — worktree misses 2+ merge windows) compound: the
worktree sees behind>=2, but EACH integration-tip amend is the same
commit rewritten with the same message. Detected by: behind>=1, AND
the oldest commit exclusive to our side (the pre-amend candidate)
differs from $TGT's tip only in dag.jsonl/merge-shas.jsonl, AND
carries the same commit message (amend --no-edit keeps it). When true,
`git rebase $TGT` auto-drops the patch-already-upstream commit(s).
"""
```

**Coordinate with [P0304-T64/T65](plan-0304-trivial-batch-p0222-harness.md):** T64 removes `merge-shas.jsonl` from `_PHANTOM_AMEND_FILES` (dead entry — amend doesn't touch it). T65 drops the `amend_diff and` truthiness guard at `:220` (empty-diff = no-op amend should also be phantom). If P0304 lands first, T1 applies to the T65-modified guard. If T1 lands first, P0304-T64/T65 apply on top — both edits are to the same `if` block but different conditions (T1 changes `behind==1`→`behind>=1`; T65 removes `amend_diff and`). Rebase-clean.

### T2 — `test(harness):` stacked-amend regression test

MODIFY [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py). Add after the existing P0346 phantom_amend tests (grep `test_behind_check_phantom_amend` at dispatch; P0304-T65 adds `phantom_amend_identical_tree` after `:1600`):

```python
def test_behind_check_phantom_amend_stacked(tmp_path, monkeypatch):
    """phantom_amend fires for behind==2 (stacked amends).

    Scenario: merger amends TWICE in succession before the worktree
    rebases. Worktree branched from pre-first-amend; sees behind=2.
    P0346's behind==1 gate would miss this; P0358 generalizes.
    """
    repo = _mk_repo(tmp_path)
    (repo / ".claude").mkdir(parents=True)
    (repo / ".claude" / "dag.jsonl").write_text('{"plan":1,"status":"UNIMPL"}\n')
    _git(repo, "add", ".")
    _git(repo, "commit", "-m", "seed")

    _git(repo, "checkout", "-b", "sprint-1")
    # Feature commit (will be amended twice).
    (repo / "feat.txt").write_text("feature")
    _git(repo, "add", ".")
    _git(repo, "commit", "-m", "feat: real change")
    pre_amend = _git(repo, "rev-parse", "HEAD").strip()

    # First amend — dag flip 1.
    (repo / ".claude" / "dag.jsonl").write_text('{"plan":1,"status":"DONE"}\n')
    _git(repo, "add", ".claude/dag.jsonl")
    _git(repo, "commit", "--amend", "--no-edit")
    amend1 = _git(repo, "rev-parse", "HEAD").strip()
    assert amend1 != pre_amend

    # Second amend — dag flip 2 (another plan marked DONE).
    (repo / ".claude" / "dag.jsonl").write_text('{"plan":1,"status":"DONE"}\n{"plan":2,"status":"DONE"}\n')
    _git(repo, "add", ".claude/dag.jsonl")
    _git(repo, "commit", "--amend", "--no-edit")
    amend2 = _git(repo, "rev-parse", "HEAD").strip()
    assert amend2 != amend1

    # Worktree: branched from PRE-FIRST-amend, now behind by 2 amends.
    # (rev-list HEAD..sprint-1 counts reachable-from-sprint-1-not-HEAD.
    # pre_amend is orphaned after first amend; amend1 is orphaned after
    # second. Worktree's merge-base is pre_amend's parent → behind=1
    # actually, because amends REPLACE the tip, not stack. Re-check
    # at dispatch: stacked amends on the SAME commit produce behind=1
    # each time a new worktree branches. The stacked case is: worktree
    # branched from pre_amend, then TWO real merges each amended,
    # worktree now behind by 2 AMENDED commits.)
    #
    # CORRECTED scenario: two separate feature commits, each amended.
    wt = tmp_path / "p997"
    _git(repo, "worktree", "add", str(wt), pre_amend)
    (wt / ".claude" / "dag.jsonl").write_text(
        '{"plan":1,"status":"UNIMPL"}\n{"plan":3,"status":"UNIMPL"}\n'
    )
    _git(wt, "add", ".claude/dag.jsonl")
    _git(wt, "commit", "-m", "docs: add plan-3")

    monkeypatch.setattr("onibus.git_ops.INTEGRATION_BRANCH", "sprint-1")
    bc = behind_check(wt)
    # NOTE: the above scenario gives behind=1 because amends on the
    # same commit collapse. For TRUE behind>=2, need:
    #   commit A (ff'd, amended) → commit B (ff'd on top of amended-A,
    #   then B amended). Worktree branched before A's amend sees
    #   behind=2 (amended-A and amended-B both new).
    # Restructure at dispatch to build THAT scenario, or verify the
    # real high-throughput case that triggered consol-mc110 and mirror.
    assert bc.phantom_amend is True, (
        f"stacked amend: expected phantom_amend=True, got {bc!r}"
    )


def test_behind_check_not_phantom_real_two_behind(tmp_path, monkeypatch):
    """Negative: behind=2 with REAL commits (not amends) → phantom=False.

    Distinguishes stacked-amend (same message, dag.jsonl-only diff)
    from genuine behind-by-2 (different messages, real file changes).
    """
    repo = _mk_repo(tmp_path)
    (repo / ".claude").mkdir(parents=True)
    (repo / ".claude" / "dag.jsonl").write_text('{"plan":1}\n')
    _git(repo, "add", ".")
    _git(repo, "commit", "-m", "seed")

    _git(repo, "checkout", "-b", "sprint-1")
    (repo / "a.txt").write_text("a")
    _git(repo, "add", ".")
    _git(repo, "commit", "-m", "feat: a")
    base = _git(repo, "rev-parse", "HEAD").strip()

    (repo / "b.txt").write_text("b")
    _git(repo, "add", ".")
    _git(repo, "commit", "-m", "feat: b")
    (repo / "c.txt").write_text("c")
    _git(repo, "add", ".")
    _git(repo, "commit", "-m", "feat: c")

    wt = tmp_path / "p998"
    _git(repo, "worktree", "add", str(wt), base)
    (wt / ".claude" / "dag.jsonl").write_text('{"plan":1}\n{"plan":9}\n')
    _git(wt, "add", ".claude/dag.jsonl")
    _git(wt, "commit", "-m", "docs: plan-9")

    monkeypatch.setattr("onibus.git_ops.INTEGRATION_BRANCH", "sprint-1")
    bc = behind_check(wt)
    assert bc.behind == 2
    # amend_diff (pre_amend vs sprint-1 tip) includes b.txt, c.txt —
    # NOT subset of _PHANTOM_AMEND_FILES → phantom stays False.
    assert bc.phantom_amend is False, (
        f"real behind-by-2: expected phantom_amend=False, got {bc!r}"
    )
```

**The stacked-amend scenario in `test_behind_check_phantom_amend_stacked` needs restructuring at dispatch** — amending the SAME commit N times gives `behind=1` each time (the amended commit REPLACES, doesn't stack). The true stacked case is: merge A (amended), merge B on top (amended), worktree branched before A's amend → behind=2 because both amended-A and amended-B are new reachable-from-sprint-1. Build THAT scenario:

```python
# Merge 1: feature A → ff → amend.
(repo / "a.txt").write_text("a"); _git(repo, "add", ".")
_git(repo, "commit", "-m", "feat: a")
pre_amend_a = _git(repo, "rev-parse", "HEAD").strip()
(repo / ".claude/dag.jsonl").write_text('{"plan":1,"s":"DONE"}\n')
_git(repo, "add", "."); _git(repo, "commit", "--amend", "--no-edit")

# Merge 2: feature B on top → ff → amend.
(repo / "b.txt").write_text("b"); _git(repo, "add", ".")
_git(repo, "commit", "-m", "feat: b")
(repo / ".claude/dag.jsonl").write_text('{"plan":1,"s":"DONE"}\n{"plan":2,"s":"DONE"}\n')
_git(repo, "add", "."); _git(repo, "commit", "--amend", "--no-edit")

# Worktree: branched from pre_amend_a (before FIRST amend).
# Now behind by 2: amended-A and amended-B.
wt = tmp_path / "p997"
_git(repo, "worktree", "add", str(wt), pre_amend_a)
# ... (worktree commits touching dag.jsonl)

bc = behind_check(wt)
assert bc.behind == 2
# pre_amend candidate (ours_only[0]) = pre_amend_a.
# Diff pre_amend_a vs sprint-1-tip = a.txt? NO — pre_amend_a HAS a.txt
# (it's the feature commit before amend). Diff = b.txt + dag.jsonl.
# That's NOT subset of _PHANTOM_AMEND_FILES → phantom = False.
#
# THE CHECK AS WRITTEN DOESN'T GENERALIZE CLEANLY for behind>=2.
```

**DESIGN NOTE:** For the stacked case, the `pre_amend vs tip` diff includes intermediate-merge feature files (b.txt in the example), so `amend_diff <= _PHANTOM_AMEND_FILES` is FALSE. The P0346 check doesn't straightforwardly generalize. Alternative approaches:

1. **Per-commit check:** For each of the N behind-commits, compare it to its amended successor. If EVERY pair differs only in `_PHANTOM_AMEND_FILES`, it's a pure-amend stack. This is O(N) git invocations.
2. **Orphan-base check only:** Drop the diff check; rely on `for-each-ref --contains <ours_only[0]>` → empty (base is orphaned). An orphaned merge-base is ALWAYS an amend-artifact (no other workflow produces it). Accept behind>=1.
3. **rev-list cherry-mark:** `git rev-list --cherry-mark --left-right HEAD...sprint-1` marks patch-identical commits. If all `>` (sprint-1-only) commits are `=` (patch-identical to a `<` commit) except for dag.jsonl hunks — complex.

**Recommend approach 2** — the orphan-base check at [`:220`](../../.claude/lib/onibus/git_ops.py) (`for-each-ref --contains`) was in P0346's clause 3 but the current implementation doesn't use it (it uses diff-subset + message-match instead). Reintroduce it as the primary signal:

```python
if behind >= 1:
    # ... collision computation ...
    ours_only = (git_try(
        "rev-list", "--reverse", "HEAD", f"^{INTEGRATION_BRANCH}",
        cwd=worktree,
    ) or "").splitlines()
    if ours_only:
        pre_amend = ours_only[0]
        # Orphan check: no ref contains pre_amend → it's an
        # amend-orphaned SHA. This is the DEFINITIONAL signal —
        # only an amend (or a force-push, which the merger never
        # does) can leave a reflog-only SHA with no ref pointing
        # at or beyond it.
        containing = git_try("for-each-ref", "--contains", pre_amend, cwd=worktree)
        if not (containing or "").strip():
            # Belt-and-suspenders: the pre_amend commit should have
            # the same message as SOME integration-branch commit
            # (amend --no-edit preserved it). For behind=1, that's
            # the tip. For behind>=2, walk the integration-branch
            # log and match.
            msg_pre = git_try("log", "-1", "--format=%s", pre_amend, cwd=worktree)
            recent_msgs = set((git_try(
                "log", f"-{behind}", "--format=%s", INTEGRATION_BRANCH,
                cwd=worktree,
            ) or "").splitlines())
            phantom = msg_pre is not None and msg_pre in recent_msgs
```

This handles both behind=1 (P0346's case) and behind>=2 (stacked).

**CHECK AT DISPATCH:** Whether the message-match is necessary at all. If `for-each-ref --contains` empty is SUFFICIENT (no false-positive scenario produces an orphaned SHA in the worktree's history), drop the message check entirely. The only known producer of orphaned SHAs in this workflow is the merger's amend; a force-push would also orphan, but the merger never force-pushes.

### T3 — `docs(harness):` validator agent phantom_amend note — behind>=1

MODIFY [`.claude/agents/rio-impl-validator.md`](../../.claude/agents/rio-impl-validator.md) at the phantom_amend section (grep `phantom_amend` — [P0295-T41](plan-0295-doc-rot-batch-sweep.md) already clarifies the relaunch prose at `:48`). Add:

```markdown
`phantom_amend: true` with `behind >= 2` means the worktree missed
TWO+ merge windows (stacked amends). Same resolution: `git rebase
$TGT` auto-drops the patch-already-upstream commits. The behind=1
common case is described above; the stacked case is rarer but
mechanically identical.
```

## Exit criteria

- `grep 'behind >= 1\|behind>=1' .claude/lib/onibus/git_ops.py` → ≥1 hit (T1: guard generalized)
- `grep 'behind == 1' .claude/lib/onibus/git_ops.py` → 0 hits (T1: old guard removed)
- `nix develop -c pytest .claude/lib/test_scripts.py -k 'phantom_amend_stacked'` → 1 passed (T2: stacked scenario)
- `nix develop -c pytest .claude/lib/test_scripts.py -k 'not_phantom_real_two_behind'` → 1 passed (T2: negative)
- `nix develop -c pytest .claude/lib/test_scripts.py -k 'phantom_amend'` → ALL passed (T2: P0346's single-amend test still passes)
- **Mutation:** revert `behind >= 1` → `behind == 1`, `phantom_amend_stacked` test FAILS
- `grep 'behind >= 2\|stacked amends\|missed TWO' .claude/agents/rio-impl-validator.md` → ≥1 hit (T3)

## Tracey

No markers. Harness tooling is outside tracey's spec-coverage domain (the `.claude/` directory isn't in `config.styx` include globs, and there's no `r[harness.*]` domain).

## Files

```json files
[
  {"path": ".claude/lib/onibus/git_ops.py", "action": "MODIFY", "note": "T1: :208 behind==1 → behind>=1; reintroduce for-each-ref orphan check; message-match against last-N msgs not just tip; docstring update :187-191"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T2: +test_behind_check_phantom_amend_stacked (behind=2 scenario) + test_behind_check_not_phantom_real_two_behind (negative) after P0346's tests ~:1600"},
  {"path": ".claude/agents/rio-impl-validator.md", "action": "MODIFY", "note": "T3: phantom_amend stacked-amend note (extends P0295-T41's :48 clarification)"}
]
```

```
.claude/
├── lib/
│   ├── onibus/git_ops.py      # T1: generalize guard
│   └── test_scripts.py        # T2: stacked + negative tests
└── agents/rio-impl-validator.md  # T3: stacked note
```

## Dependencies

```json deps
{"deps": [346], "soft_deps": [304, 295], "note": "P0346 added phantom_amend field + single-amend detection + _PHANTOM_AMEND_FILES constant. Soft-dep P0304-T64 (drops merge-shas.jsonl from _PHANTOM_AMEND_FILES) + T65 (drops truthiness guard at :220) — both edit the same if-block as T1; T1 changes the behind-comparison, T64/T65 change the diff-comparison. Non-overlapping conditions, rebase-clean either order. Soft-dep P0295-T41 (clarifies :48 validator prose) — T3 extends the same section. discovered_from=consolidator(mc110)."}
```

**Depends on:** [P0346](plan-0346-phantom-amend-behindcheck.md) — `phantom_amend` field, `_PHANTOM_AMEND_FILES`, detection scaffold.

**Soft-deps:** [P0304](plan-0304-trivial-batch-p0222-harness.md) T64/T65 — same if-block, different conditions. [P0295](plan-0295-doc-rot-batch-sweep.md) T41 — same validator prose section.

**Conflicts with:** [`git_ops.py`](../../.claude/lib/onibus/git_ops.py) touched by P0304-T64/T65 (same function, different lines within `:208-224` — T64 at `:173`, T65 at `:220`, T1 at `:208`). [`test_scripts.py`](../../.claude/lib/test_scripts.py) touched by P0304-T30/T65 (append-only, different test names). [`rio-impl-validator.md`](../../.claude/agents/rio-impl-validator.md) touched by P0295-T41 at `:48` — T3 adds AFTER T41's clarification.
