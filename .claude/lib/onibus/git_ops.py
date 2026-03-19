"""Git plumbing + compound merger operations.

The low-level helpers (git, git_try, list_worktrees) are lifted straight.
The compound ops (preflight, convco_check, rebase_anchored, ff_try,
behind_report) are NEW — they absorb the untested bash blocks from
rio-impl-merger.md steps 2/3/4/5/8 into typed, testable functions that
return pydantic JSON. The agent reads structured output instead of holding
SHAs in shell variables across tool calls.
"""

from __future__ import annotations

import re
import subprocess
from pathlib import Path

from onibus import INTEGRATION_BRANCH, REPO_ROOT
from onibus.models import (
    BehindCheck,
    BehindReport,
    BehindWorktree,
    ConvcoResult,
    FfResult,
    Preflight,
    RebaseResult,
    Worktree,
)


# ─── low-level git ───────────────────────────────────────────────────────────


def git(*args: str, cwd: Path | None = None) -> str:
    """Run git, return stdout stripped. Non-zero → CalledProcessError."""
    out = subprocess.run(
        ["git", *args],
        cwd=cwd or REPO_ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    return out.stdout.strip()


def git_try(*args: str, cwd: Path | None = None) -> str | None:
    """Like git() but returns None on non-zero instead of raising."""
    try:
        return git(*args, cwd=cwd)
    except subprocess.CalledProcessError:
        return None


def list_worktrees() -> list[Worktree]:
    """Parse `git worktree list --porcelain`."""
    out = git("worktree", "list", "--porcelain")
    wts = []
    cur: dict = {}
    for line in out.splitlines():
        if not line:
            if cur:
                wts.append(cur)
                cur = {}
            continue
        key, _, val = line.partition(" ")
        cur[key] = val
    if cur:
        wts.append(cur)
    return [
        Worktree(
            path=Path(w["worktree"]),
            branch=w.get("branch", "").removeprefix("refs/heads/") or None,
            head=w["HEAD"],
        )
        for w in wts
    ]


def plan_worktrees() -> list[Worktree]:
    """Worktrees on a `pNNN` branch, excluding main."""
    return [w for w in list_worktrees() if w.plan_num is not None]


def diff_src_files(wt: Worktree) -> list[str]:
    """Files matching rio-*/src/*.rs changed on this worktree vs the integration branch."""
    out = git_try("diff", "--name-only", f"{INTEGRATION_BRANCH}..HEAD", cwd=wt.path) or ""
    return sorted(f for f in out.splitlines() if re.match(r"^rio-[a-z-]+/src/.*\.rs$", f))


# ─── compound merger ops (NEW — absorbed from merger.md bash) ────────────────

_CONVCO_RE = re.compile(r"^(feat|fix|perf|refactor|test|docs|chore)(\([a-z0-9-]+\))?: ")


def preflight(branch: str, *, repo: Path | None = None) -> Preflight:
    """Was merger.md:45-50. Checks worktree+branch exist, commits ahead of $TGT."""
    repo = repo or REPO_ROOT
    wt_path = repo.parent / branch
    wt_exists = wt_path.is_dir()
    br_exists = git_try("rev-parse", "--verify", branch, cwd=repo) is not None

    ahead = 0
    if br_exists:
        log = git_try("log", "--oneline", f"{INTEGRATION_BRANCH}..{branch}", cwd=repo) or ""
        ahead = len(log.splitlines()) if log else 0

    if not wt_exists:
        reason = f"worktree {wt_path} missing"
    elif not br_exists:
        reason = f"branch {branch!r} not found"
    elif ahead == 0:
        reason = f"no commits ahead of {INTEGRATION_BRANCH}"
    else:
        reason = ""
    return Preflight(
        worktree_exists=wt_exists,
        branch_exists=br_exists,
        commits_ahead=ahead,
        clean=(reason == ""),
        reason=reason,
    )


def convco_check(range_spec: str, *, cwd: Path | None = None) -> ConvcoResult:
    """Was merger.md:59-63 bash loop. range_spec e.g. 'sprint-1..p134'."""
    log = git("log", "--format=%s", range_spec, cwd=cwd or REPO_ROOT)
    violations = [s for s in log.splitlines() if s and not _CONVCO_RE.match(s)]
    return ConvcoResult(violations=violations, clean=not violations)


def rebase_anchored(branch: str, *, repo: Path | None = None) -> RebaseResult:
    """Was merger.md:70-81. Rebase onto $TGT from the branch's worktree.
    On conflict, aborts the rebase so the tree is clean before returning."""
    wt = (repo or REPO_ROOT).parent / branch
    pre = git("rev-parse", "HEAD", cwd=wt)

    proc = subprocess.run(
        ["git", "rebase", INTEGRATION_BRANCH],
        cwd=wt, capture_output=True, text=True,
    )
    if proc.returncode != 0:
        # Conflict — grab conflicting paths, then abort so tree is clean.
        status = git_try("diff", "--name-only", "--diff-filter=U", cwd=wt) or ""
        conflicts = status.splitlines()
        git_try("rebase", "--abort", cwd=wt)
        return RebaseResult(
            status="conflict", pre_rebase=pre, moved=0, conflict_files=conflicts
        )

    post = git("rev-parse", "HEAD", cwd=wt)
    if post == pre:
        return RebaseResult(status="no-op", pre_rebase=pre, moved=0)
    moved = int(git("rev-list", "--count", f"{pre}..{post}", cwd=wt))
    return RebaseResult(status="ok", pre_rebase=pre, moved=moved)


def ff_try(branch: str, *, repo: Path | None = None) -> FfResult:
    """Was merger.md:88-93. ff-only merge from the integration-branch worktree.
    Caller owns rollback: on CI fail, `git reset --hard <pre_merge>` using the
    SHA from this result."""
    repo = repo or REPO_ROOT
    pre = git("rev-parse", "HEAD", cwd=repo)
    proc = subprocess.run(
        ["git", "merge", "--ff-only", branch],
        cwd=repo, capture_output=True, text=True,
    )
    if proc.returncode != 0:
        return FfResult(status="not-ff", pre_merge=pre, post_merge=pre)
    return FfResult(status="ok", pre_merge=pre, post_merge=git("rev-parse", "HEAD", cwd=repo))


_PHANTOM_AMEND_FILES = frozenset({
    ".claude/dag.jsonl",
    ".claude/state/merge-shas.jsonl",
})


def behind_check(worktree: Path) -> BehindCheck:
    """Validator's step-0 compound query. Was: rev-list + 3-dot diff bash.
    3-dot (TGT...HEAD) = merge-base to HEAD — what THIS worktree changed.
    3-dot (HEAD...TGT) = merge-base to TGT — what $TGT changed since fork.
    Intersection = files both sides touched = expected rebase conflict.
    (2-dot on the theirs side was a bug: tree-vs-tree diff includes our own
    changes as 'undo', so mine&theirs over-reported — phantom self-collision.)

    phantom_amend: the merger's step-7.5 `git commit --amend --no-edit`
    rewrites the tip after ff, orphaning any worktree that rebased onto the
    pre-amend SHA during the ff→amend window. Detected by: exactly-1 behind,
    AND the oldest commit exclusive to our side (the pre-amend candidate)
    differs from $TGT's tip only in dag.jsonl/merge-shas.jsonl, AND carries
    the same commit message (amend --no-edit keeps it). When true,
    `git rebase $TGT` auto-drops the patch-already-upstream commit.

    The file_collision list in this case includes files from the pre-amend
    commit (the feature work that was ff'd-then-amended) — NOT just
    dag.jsonl. That's why trivial_rebase reads false despite the rebase
    being mechanically safe."""
    behind_s = git_try("rev-list", "--count", f"HEAD..{INTEGRATION_BRANCH}", cwd=worktree)
    behind = int(behind_s) if behind_s and behind_s.isdigit() else 0
    collision: list[str] = []
    phantom = False
    if behind > 0:
        mine = set((git_try("diff", f"{INTEGRATION_BRANCH}...HEAD", "--name-only", cwd=worktree) or "").splitlines())
        theirs = set((git_try("diff", f"HEAD...{INTEGRATION_BRANCH}", "--name-only", cwd=worktree) or "").splitlines())
        collision = sorted(mine & theirs)
        # Phantom-amend: behind==1 AND the oldest commit exclusive to our side
        # (the pre-amend candidate) differs from $TGT's tip only in the
        # amend-files AND has the same commit message.
        if behind == 1:
            ours_only = (git_try(
                "rev-list", "--reverse", "HEAD", f"^{INTEGRATION_BRANCH}",
                cwd=worktree,
            ) or "").splitlines()
            if ours_only:
                pre_amend = ours_only[0]  # oldest exclusively-ours commit
                amend_diff = set((git_try(
                    "diff", "--name-only", pre_amend, INTEGRATION_BRANCH,
                    cwd=worktree,
                ) or "").splitlines())
                amend_diff.discard("")
                if amend_diff and amend_diff <= _PHANTOM_AMEND_FILES:
                    # Belt-and-suspenders: same subject → amend --no-edit.
                    msg_pre = git_try("log", "-1", "--format=%s", pre_amend, cwd=worktree)
                    msg_tip = git_try("log", "-1", "--format=%s", INTEGRATION_BRANCH, cwd=worktree)
                    phantom = msg_pre is not None and msg_pre == msg_tip
    return BehindCheck(
        behind=behind,
        file_collision=collision,
        trivial_rebase=behind > 0 and not collision,
        phantom_amend=phantom,
    )


def behind_report(*, repo: Path | None = None) -> BehindReport:
    """Was merger.md:159-166 bash loop. How far each non-main worktree is behind $TGT."""
    repo = repo or REPO_ROOT
    out = []
    for wt in list_worktrees():
        if wt.path == repo:
            continue
        count = git_try("rev-list", "--count", f"HEAD..{INTEGRATION_BRANCH}", cwd=wt.path)
        behind = int(count) if count and count.isdigit() else 0
        if behind > 0:
            out.append(BehindWorktree(path=str(wt.path), branch=wt.branch or "?", behind=behind))
    return BehindReport(worktrees=out)
