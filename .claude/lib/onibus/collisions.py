"""CollisionIndex — live-computed path↔plan index. No jsonl cache.

The old collisions.jsonl was a derived artifact (state.py collisions-regen
scanned plan docs, wrote jsonl). Stale the moment a plan doc changed. Here
the same scan runs per CLI invocation — ~15ms for 302 docs, no staleness.
"""

from __future__ import annotations

import re
from collections import defaultdict
from typing import Iterable, Mapping

from onibus import STATE_DIR, WORK_DIR
from onibus.git_ops import diff_files, plan_worktrees
from onibus.jsonl import read_jsonl
from onibus.models import AgentRow, Collision, CollisionReport, CollisionRow, PlanFile
from onibus.plan_doc import find_plan_doc, plan_doc_files, plan_doc_src_files

_PLAN_RE = re.compile(r"^plan-(\d{4})-")

# Migration-number collision key. The P0332+P0264 both-picked-017 incident
# (resolved by coord renumbering P0264 to 018) wasn't catchable by path-index:
# different paths (017_tenant_keys_fk_cascade.sql vs 017_chunk_tenants.sql),
# same number prefix. Special-case migrations/*.sql — collide on NUMBER.
_MIGRATION_NUM = re.compile(r"^migrations/0*(\d+)_")


def _migration_collision_key(path: str) -> str | None:
    """Extract synthetic `migration:N` collision key for migrations/NNN_*.sql.
    Two plans with different migration filenames but same NNN prefix collide."""
    if m := _MIGRATION_NUM.match(path):
        return f"migration:{m.group(1)}"
    return None


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
                # Synthetic migration:N key — collide on migration number
                # even when filenames differ (017_foo.sql vs 017_bar.sql).
                mig_key = _migration_collision_key(p)
                if mig_key:
                    by_path[mig_key].append(n)
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
                # Tolerate stale paths (justfile, scripts/) in legacy plan
                # docs — skip invalids rather than crash the frontier.
                paths = []
                for f in fenced:
                    try:
                        paths.append(PlanFile.model_validate(f).path)
                    except Exception:
                        pass
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


def _verify_phase_plans() -> set[int]:
    """Plans whose latest in-flight role is verify (or later). Impl is done,
    worktree files won't change — exclude from collision check. At mc~250,
    P0314/P0410/P0413 sitting in verify blocked 10 launchable plans on
    false file-collisions."""
    # Latest row per plan wins (rows are append-order). A plan that has
    # reached role=verify has finished impl; a running verify/review/merge
    # row means the worktree is frozen pending validator/reviewer/merger.
    latest: dict[str, AgentRow] = {}
    for r in read_jsonl(STATE_DIR / "agents-running.jsonl", AgentRow):
        latest[r.plan] = r
    frozen_roles = {"verify", "review", "merge"}
    out: set[int] = set()
    for plan_str, r in latest.items():
        if r.role in frozen_roles and r.status == "running":
            m = re.fullmatch(r"P(\d+)", plan_str)
            if m:
                out.add(int(m.group(1)))
    return out


def check_vs_running(plan_num: int) -> CollisionReport:
    """Does plan N's declared files intersect any running worktree's ACTUAL diff?
    Running-side is ground truth (git diff); this-side is the plan doc's fence/grep.

    Skips worktrees whose plan is in verify/review/merge phase — impl is
    done, files won't change, so a collision is a false positive."""
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

    frozen = _verify_phase_plans()
    collisions = []
    for wt in plan_worktrees():
        if wt.plan_num == plan_num:
            continue  # don't collide with self
        if wt.plan_num in frozen:
            continue  # verify-phase worktree — files won't change
        their = set(diff_files(wt))
        overlap = sorted(this_set & their)
        if overlap:
            collisions.append(Collision(branch=wt.branch or "?", files=overlap))

    return CollisionReport(
        plan=plan_num,
        this_files=this_files,
        collisions=collisions,
        source=source,  # type: ignore[arg-type]
    )
