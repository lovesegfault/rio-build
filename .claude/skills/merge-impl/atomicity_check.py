#!/usr/bin/env python3
"""/merge-impl step 0b: commit atomicity precondition.

Two gates:
  - mega-commit: batch plan (t_count >= 3) collapsed to c_count == 1
  - chore-touches-src: any `chore:` commit touching rio-*/src/*.rs

Both abort the merge. Run against a branch before ff-merge.

Usage:
    atomicity_check.py <branch>
    atomicity_check.py --schema
"""

from __future__ import annotations

import re
import sys
from typing import Literal

from pydantic import BaseModel, Field

from _lib import INTEGRATION_BRANCH, cli_schema_or_run, find_plan_doc, git, plan_doc_t_count


class ChoreViolation(BaseModel):
    sha: str
    subject: str
    src_files: list[str] = Field(description="The rio-*/src/*.rs files touched")


class AtomicityVerdict(BaseModel):
    branch: str
    t_count: int = Field(description="### T<N> headers in the plan doc")
    c_count: int = Field(description="Commits on branch ahead of main")
    mega_commit: bool = Field(
        description="t_count >= 3 && c_count == 1 — batch collapsed"
    )
    chore_violations: list[ChoreViolation]
    abort_reason: Literal["mega-commit", "chore-touches-src"] | None = Field(
        default=None,
        description="None means pass; anything else is the merger's abort_reason.",
    )


def _plan_num_from_branch(branch: str) -> int | None:
    if m := re.fullmatch(r"p(\d+)", branch):
        return int(m.group(1))
    return None


def _commit_src_files(sha: str) -> list[str]:
    out = git("show", "--name-only", "--format=", sha)
    return [f for f in out.splitlines() if re.match(r"^rio-[a-z-]+/src/.*\.rs$", f)]


def run(branch: str) -> AtomicityVerdict:
    # t_count from plan doc
    plan_num = _plan_num_from_branch(branch)
    t_count = 0
    if plan_num is not None:
        doc = find_plan_doc(plan_num)
        if doc:
            t_count = plan_doc_t_count(doc)

    # c_count and chore scan
    log = git("log", "--format=%H %s", f"{INTEGRATION_BRANCH}..{branch}")
    commits = [line.split(" ", 1) for line in log.splitlines() if line]
    c_count = len(commits)

    chore_violations = []
    for sha, subj in commits:
        if not subj.startswith("chore"):
            continue
        src = _commit_src_files(sha)
        if src:
            chore_violations.append(
                ChoreViolation(sha=sha, subject=subj, src_files=src)
            )

    mega = t_count >= 3 and c_count == 1
    abort = (
        "chore-touches-src" if chore_violations else ("mega-commit" if mega else None)
    )

    return AtomicityVerdict(
        branch=branch,
        t_count=t_count,
        c_count=c_count,
        mega_commit=mega,
        chore_violations=chore_violations,
        abort_reason=abort,
    )


if __name__ == "__main__":
    cli_schema_or_run(AtomicityVerdict, lambda: run(sys.argv[1]))
