#!/usr/bin/env python3
"""/merge-impl: assign real plan numbers to UNASSIGNED docs (runs before atomicity).

Writers emit placeholder IDs — 9-digit numbers, 9<runid%10^6:06d><N:02d> — so
the same token appears in filename, tracey markers, dag.jsonl rows, links, prose.
One literal string-replace per placeholder rewrites everything. No counter
contention at write time; allocation happens here, and /merge-impl is serial
by construction (one merger at a time).

Idempotent: no 9-digit placeholders found → empty mapping, no commit.

Usage:
    rename_unassigned.py <branch>
    rename_unassigned.py --schema
"""

from __future__ import annotations

import re
import sys

from pydantic import BaseModel, Field

from _lib import INTEGRATION_BRANCH, REPO_ROOT, cli_schema_or_run, git

# REPO_ROOT resolves to main/ when invoked via /merge-impl (coordinator cwd).
# Scan MAIN's docs for the max real number — a docs branch that forked before
# P0188 merged would otherwise allocate 188 again from its stale worktree view.
DOCS_DIR = REPO_ROOT / ".claude" / "work"

# Placeholder: 9 digits, leading 9. Real phases are ≤4 digits.
_PLACEHOLDER_RE = re.compile(r"^plan-(9\d{8})-(.+)\.md$")
_REAL_RE = re.compile(r"^plan-(\d{1,4})-")


class Rename(BaseModel):
    placeholder: str = Field(description="9-digit fake number, e.g. '924999901'")
    assigned: int = Field(description="Real phase number")
    slug: str


class RenameReport(BaseModel):
    branch: str
    mapping: list[Rename] = Field(
        description="Empty → no placeholders on branch, no-op. "
        "Non-empty → files renamed, refs rewritten, commit created."
    )
    commit: str | None = Field(description="SHA of the rename commit, or None if no-op")


def _worktree_for(branch: str) -> str:
    out = git("worktree", "list", "--porcelain")
    for block in out.split("\n\n"):
        lines = {k: v for k, _, v in (ln.partition(" ") for ln in block.splitlines())}
        if lines.get("branch") == f"refs/heads/{branch}":
            return lines["worktree"]
    raise SystemExit(f"no worktree for branch {branch!r}")


def _find_placeholders(worktree: str) -> list[tuple[str, str]]:
    """Returns [(placeholder, slug)] sorted by placeholder (deterministic numbering)."""
    docs = f"{worktree}/.claude/work"
    found = []
    for name in sorted(git("ls-files", docs, cwd=worktree).splitlines()):
        m = _PLACEHOLDER_RE.match(name.rsplit("/", 1)[-1])
        if m:
            found.append((m.group(1), m.group(2)))
    return found


def _next_real() -> int:
    nums = [
        int(m.group(1))
        for p in DOCS_DIR.glob("plan-*.md")
        if (m := _REAL_RE.match(p.name))
    ]
    return max(nums, default=0) + 1


def _rewrite_and_rename(worktree: str, mapping: list[Rename]) -> None:
    """Literal replace across every tracked .md + dag.jsonl, then git mv."""
    touched = [
        f
        for f in git("diff", "--name-only", f"{INTEGRATION_BRANCH}...HEAD", cwd=worktree).splitlines()
        if f.endswith(".md")
    ]
    # Defensive: dag.jsonl must be in the set even if diff missed it (writer
    # SHOULD have touched it, but don't assume). It carries the placeholder
    # as {"plan":924999901,...,"deps":[924999901]} — same literal text hits
    # for str.replace.
    if ".claude/dag.jsonl" not in touched:
        touched.append(".claude/dag.jsonl")

    for rel in touched:
        p = f"{worktree}/{rel}"
        try:
            text = open(p).read()
        except FileNotFoundError:
            continue  # diff listed a deleted file
        new = text
        # JSON int literals can't have leading zeros — .jsonl gets unpadded,
        # .md gets :04d (filenames/markers/P-refs all want the pad).
        is_jsonl = rel.endswith(".jsonl")
        for r in mapping:
            repl = str(r.assigned) if is_jsonl else f"{r.assigned:04d}"
            new = new.replace(r.placeholder, repl)
        if new != text:
            open(p, "w").write(new)

    for r in mapping:
        git(
            "mv",
            f".claude/work/plan-{r.placeholder}-{r.slug}.md",
            f".claude/work/plan-{r.assigned:04d}-{r.slug}.md",
            cwd=worktree,
        )


def run(branch: str) -> RenameReport:
    worktree = _worktree_for(branch)
    placeholders = _find_placeholders(worktree)
    if not placeholders:
        return RenameReport(branch=branch, mapping=[], commit=None)

    start = _next_real()
    mapping = [
        Rename(placeholder=ph, assigned=start + i, slug=slug)
        for i, (ph, slug) in enumerate(placeholders)
    ]

    _rewrite_and_rename(worktree, mapping)
    git("add", "-u", cwd=worktree)
    lo, hi = mapping[0].assigned, mapping[-1].assigned
    label = f"P{lo:04d}" if lo == hi else f"P{lo:04d}-P{hi:04d}"
    git("commit", "-m", f"docs: assign plan numbers {label}", cwd=worktree)

    return RenameReport(
        branch=branch,
        mapping=mapping,
        commit=git("rev-parse", "--short", "HEAD", cwd=worktree),
    )


if __name__ == "__main__":
    cli_schema_or_run(RenameReport, lambda: run(sys.argv[1]))
