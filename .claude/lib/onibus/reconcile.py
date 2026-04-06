"""Cross-check agents-running.jsonl against filesystem worktree reality.

Stale row = row says running, worktree gone. Orphan = worktree exists, no row.
Doesn't fix — reports. Coordinator decides (stale rows usually mean the agent
finished in a prior session and wasn't consumed; orphans are usually manual
worktrees for debugging).
"""

from __future__ import annotations

from pathlib import Path

from onibus import INTEGRATION_BRANCH, STATE_DIR
from onibus.git_ops import git_try, plan_worktrees
from onibus.jsonl import read_jsonl
from onibus.models import AgentRow, ReconcileReport


def reconcile() -> ReconcileReport:
    rows = read_jsonl(STATE_DIR / "agents-running.jsonl", AgentRow)
    wts = plan_worktrees()
    wt_paths = {str(w.path) for w in wts}
    row_paths = {r.worktree for r in rows if r.worktree}

    stale = [
        r for r in rows
        if r.status == "running" and r.worktree and not Path(r.worktree).is_dir()
    ]
    orphans = sorted(wt_paths - row_paths)

    # Stuck: running row + worktree exists + zero commits ahead. One rev-list
    # per live running row (≤10). Replaces the manual per-suspect check.
    stuck: list[str] = []
    for r in rows:
        if r.status != "running" or not r.worktree:
            continue
        wt = Path(r.worktree)
        if not wt.is_dir():
            continue
        ahead = git_try("rev-list", "--count", f"{INTEGRATION_BRANCH}..HEAD", cwd=wt)
        if ahead == "0":
            stuck.append(f"{r.plan}:{r.role} @ {r.worktree}")

    return ReconcileReport(
        stale_rows=stale, orphan_worktrees=orphans, stuck_agents=stuck,
        ok=(not stale and not orphans and not stuck),
    )
