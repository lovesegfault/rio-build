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


class GitError(subprocess.CalledProcessError):
    """CalledProcessError with stderr in __str__. P0504 merger hit exit-128
    on git commit --amend; CalledProcessError.__str__ includes returncode
    and cmd but NOT stderr, so the traceback showed only "returned non-zero
    exit status 128" — the actual git error was invisible. This subclass
    surfaces it. git_try() still catches this via the base class."""

    def __str__(self) -> str:
        base = super().__str__()
        if self.stderr:
            return f"{base}\n{self.stderr.rstrip()}"
        return base


def git(*args: str, cwd: Path | None = None) -> str:
    """Run git, return stdout stripped. Non-zero → GitError (a
    CalledProcessError subclass with stderr surfaced in __str__)."""
    try:
        out = subprocess.run(
            ["git", *args],
            cwd=cwd or REPO_ROOT,
            capture_output=True,
            text=True,
            check=True,
        )
    except subprocess.CalledProcessError as e:
        raise GitError(e.returncode, e.cmd, e.output, e.stderr) from None
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


def diff_files(wt: Worktree) -> list[str]:
    """All files changed on this worktree vs the integration branch.

    P0448: formerly `diff_src_files` with a `^rio-*/src/*.rs$` filter. That
    filter made check_vs_running blind to collisions on .claude/work/ plan
    docs, nix/, docs/, migrations/, rio-*/tests/ — anything the PlanFile
    pattern accepts but the rust-src regex doesn't. P0295↔P0437 overlapping
    on plan-0304-*.md was the observed miss. No their-side filter needed:
    check_vs_running's intersection with this_set (already PlanFile-scoped
    or rust-src-scoped) does the scoping."""
    out = git_try("diff", "--name-only", f"{INTEGRATION_BRANCH}..HEAD", cwd=wt.path) or ""
    return sorted(f for f in out.splitlines() if f)


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


# merge-shas.jsonl was here but is gitignored (.gitignore:50 .claude/state/)
# — never in any commit, can never appear in `git diff --name-only`. Dead entry
# removed; dag.jsonl is the only file the merger's --amend touches.
_PHANTOM_AMEND_FILES = frozenset({".claude/dag.jsonl"})


def behind_check(worktree: Path) -> BehindCheck:
    """Validator's step-0 compound query. Was: rev-list + 3-dot diff bash.
    3-dot (TGT...HEAD) = merge-base to HEAD — what THIS worktree changed.
    3-dot (HEAD...TGT) = merge-base to TGT — what $TGT changed since fork.
    Intersection = files both sides touched = expected rebase conflict.
    (2-dot on the theirs side was a bug: tree-vs-tree diff includes our own
    changes as 'undo', so mine&theirs over-reported — phantom self-collision.)

    phantom_amend: the merger's step-7.5 `git commit --amend --no-edit`
    rewrites the tip after ff, orphaning any worktree that rebased onto the
    pre-amend SHA during the ff→amend window. Multiple amends in sequence
    (high-throughput DAG — worktree misses 2+ merge windows) compound: the
    worktree sees behind>=2, but the amended version of its orphaned base
    sits somewhere in $TGT's last-N commits with the same message (each
    `--amend --no-edit` preserves it). Detected by: behind>=1, AND the
    oldest commit exclusive to our side (the pre-amend candidate) has a
    message match against one of $TGT's last-N commits, AND differs from
    that matched commit only in dag.jsonl (or identical-tree). When true,
    `git rebase $TGT` auto-drops the patch-already-upstream commit(s).

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
        # Phantom-amend: the oldest commit exclusive to our side (the
        # pre-amend candidate) matches a commit in $TGT's last-N by
        # message (amend --no-edit keeps it) AND differs from that
        # matched commit only in the amend-files. Works for behind>=1
        # (stacked amends included — the matched commit isn't always
        # the tip, but it's within the last `behind` commits).
        ours_only = (git_try(
            "rev-list", "--reverse", "HEAD", f"^{INTEGRATION_BRANCH}",
            cwd=worktree,
        ) or "").splitlines()
        if ours_only:
            pre_amend = ours_only[0]  # oldest exclusively-ours commit
            msg_pre = git_try("log", "-1", "--format=%s", pre_amend, cwd=worktree)
            if msg_pre is not None:
                # Walk $TGT's last-N commits for a message match. For
                # behind=1 the only candidate is the tip (original
                # P0346 semantics). For behind>=2 (stacked), the amended
                # version of pre_amend sits deeper in the recent log.
                recent = (git_try(
                    "log", f"-{behind}", "--format=%H %s", INTEGRATION_BRANCH,
                    cwd=worktree,
                ) or "").splitlines()
                for line in recent:
                    sha, _, msg = line.partition(" ")
                    if msg != msg_pre:
                        continue
                    amend_diff = set((git_try(
                        "diff", "--name-only", pre_amend, sha,
                        cwd=worktree,
                    ) or "").splitlines())
                    amend_diff.discard("")
                    # Empty-set IS a subset — no-op amend (row already DONE
                    # → dag set-status no-op → --amend --no-edit rewrites
                    # with identical tree) still produces a new SHA (new
                    # committer timestamp), orphaning worktrees identically.
                    # Same-msg + identical-tree is definitionally a phantom;
                    # msg-check above already gated, so no false-positive risk.
                    if amend_diff <= _PHANTOM_AMEND_FILES:
                        phantom = True
                    break
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
