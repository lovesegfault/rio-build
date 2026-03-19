#!/usr/bin/env python3
"""Live collision check: does plan N's files intersect with any running
worktree's actual changes?

Running-worktree side uses `git diff --name-only <integration-branch>..HEAD` (ground truth).
This-plan side prefers the ```json files fenced block (state.PlanFile list);
falls back to grepping the doc for crates/…/src/….rs paths if absent. The
fence makes empty-result explicit — source='fence' means the list is intent,
not a grep miss. source='none' means grep fallback found nothing.

Usage:
    collision_check.py <plan_num>
    collision_check.py --schema
"""

from __future__ import annotations

import sys
from typing import Literal

from pydantic import BaseModel, Field

from _lib import (
    cli_schema_or_run,
    diff_src_files,
    find_plan_doc,
    plan_doc_files,
    plan_doc_src_files,
    plan_worktrees,
)


class Collision(BaseModel):
    branch: str
    files: list[str] = Field(description="Intersection — the colliding paths")


class CollisionReport(BaseModel):
    plan: int
    this_files: list[str] = Field(
        description="Files this plan's doc declares. From ```json files fence "
        "if present; else best-effort grep (may be empty for bare-name docs)."
    )
    collisions: list[Collision]
    source: Literal["fence", "grep", "none"] = Field(
        description="Where this_files came from. 'fence' means the list is intent; "
        "'none' means grep fallback found nothing (empty is NOT clean)."
    )


def run(plan_num: int) -> CollisionReport:
    doc = find_plan_doc(plan_num)
    source = "none"
    this_files: list[str] = []
    if doc:
        fenced = plan_doc_files(doc)
        if fenced is not None:
            this_files = sorted({f["path"] for f in fenced})
            source = "fence"
        else:
            this_files = plan_doc_src_files(doc)
            source = "grep" if this_files else "none"
    this_set = set(this_files)

    collisions = []
    for wt in plan_worktrees():
        if wt.plan_num == plan_num:
            continue  # don't collide with self
        their = set(diff_src_files(wt))
        overlap = sorted(this_set & their)
        if overlap:
            collisions.append(Collision(branch=wt.branch or "?", files=overlap))

    return CollisionReport(
        plan=plan_num,
        this_files=this_files,
        collisions=collisions,
        source=source,
    )


if __name__ == "__main__":
    cli_schema_or_run(CollisionReport, lambda: run(int(sys.argv[1])))
