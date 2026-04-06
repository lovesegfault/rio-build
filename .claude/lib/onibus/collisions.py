"""CollisionIndex — live-computed path↔plan index. No jsonl cache.

The old collisions.jsonl was a derived artifact (state.py collisions-regen
scanned plan docs, wrote jsonl). Stale the moment a plan doc changed. Here
the same scan runs per CLI invocation — ~15ms for 302 docs, no staleness.
"""

from __future__ import annotations

import re
from collections import defaultdict
from typing import Iterable, Mapping

from onibus import WORK_DIR
from onibus.git_ops import diff_src_files, plan_worktrees
from onibus.models import Collision, CollisionReport, CollisionRow, PlanFile
from onibus.plan_doc import find_plan_doc, plan_doc_files, plan_doc_src_files

_PLAN_RE = re.compile(r"^plan-(\d{4})-")


class CollisionIndex:
    """Bidirectional path↔plan index from plan-doc ```json files fences."""

    __slots__ = ("_by_path", "_by_plan")

    def __init__(self, by_plan: Mapping[int, Iterable[str]]):
        self._by_plan: dict[int, frozenset[str]] = {
            n: frozenset(paths) for n, paths in by_plan.items()
        }
        by_path: dict[str, list[int]] = defaultdict(list)
        for n, paths in self._by_plan.items():
            for p in paths:
                by_path[p].append(n)
        self._by_path = {p: sorted(ns) for p, ns in by_path.items()}

    @classmethod
    def load(cls, *, only: set[int] | None = None) -> CollisionIndex:
        """Scan WORK_DIR for plan docs, build the index. `only` restricts to
        specific plan numbers (e.g. frontier) to avoid parsing DONE docs."""
        by_plan: dict[int, list[str]] = {}
        for doc in sorted(WORK_DIR.glob("plan-*.md")):
            m = _PLAN_RE.match(doc.name)
            if not m:
                continue
            n = int(m.group(1))
            if only is not None and n not in only:
                continue
            fenced = plan_doc_files(doc)
            if fenced is not None:
                paths = [PlanFile.model_validate(f).path for f in fenced]
            else:
                paths = plan_doc_src_files(doc)
            by_plan[n] = paths
        return cls(by_plan)

    def files_of(self, n: int) -> frozenset[str]:
        return self._by_plan.get(n, frozenset())

    def check(self, n: int, against: set[int]) -> list[str]:
        """Paths that plan n shares with any plan in `against`."""
        mine = self._by_plan.get(n, frozenset())
        overlap: set[str] = set()
        for m in against:
            overlap |= mine & self._by_plan.get(m, frozenset())
        return sorted(overlap)

    def hot(self, k: int = 20) -> list[CollisionRow]:
        """Top-k paths by plan-count (count > 1 only)."""
        rows = [
            CollisionRow(path=p, plans=ns, count=len(ns))
            for p, ns in self._by_path.items()
            if len(ns) > 1
        ]
        rows.sort(key=lambda r: (-r.count, r.path))
        return rows[:k]


# ─── check against running worktrees (lifted from collision_check.py) ────────


def check_vs_running(plan_num: int) -> CollisionReport:
    """Does plan N's declared files intersect any running worktree's ACTUAL diff?
    Running-side is ground truth (git diff); this-side is the plan doc's fence/grep."""
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
        source=source,  # type: ignore[arg-type]
    )
