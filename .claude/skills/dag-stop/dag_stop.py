#!/usr/bin/env python3
"""Snapshot in-flight DAG state for handoff.

Reads the three pending-state files and reports what will survive the stop.
Does NOT write the sentinel or cancel crons — the skill does those (the
script stays pure read so it's testable without side effects).

Usage:
    dag_stop.py
    dag_stop.py --schema
"""

from __future__ import annotations

from pydantic import BaseModel, Field

from _lib import INTEGRATION_BRANCH, cli_schema_or_run, git
from state import (
    STATE_DIR,
    AgentRole,
    AgentRow,
    Followup,
    MergeQueueRow,
    Verdict,
    read_jsonl,
)


class InFlightAgent(BaseModel):
    plan: str
    role: AgentRole
    agent_id: str | None = Field(
        description="For TaskStop on hard-stop. None if prior-session."
    )
    worktree: str | None
    note: str


class QueuedMerge(BaseModel):
    plan: str
    verdict: Verdict
    commit: str


class StopSnapshot(BaseModel):
    main_sha: str = Field(description="Short SHA of main at stop time")
    in_flight: list[InFlightAgent] = Field(
        description="status=running agents. They continue in background; "
        "results land in worktrees + agents-running.jsonl. Check on resume."
    )
    merge_queue: list[QueuedMerge] = Field(
        description="Queued but not merged. /dag-run picks up from here."
    )
    followups_total: int = Field(description="Rows in followups-pending.jsonl")
    followups_pnew: int = Field(
        description="Of those, how many are P-new (standalone-phase candidates). "
        "Non-zero means the next tick would flush; consider /plan manually."
    )


def run() -> StopSnapshot:
    agents = read_jsonl(STATE_DIR / "agents-running.jsonl", AgentRow)
    queue = read_jsonl(STATE_DIR / "merge-queue.jsonl", MergeQueueRow)
    followups = read_jsonl(STATE_DIR / "followups-pending.jsonl", Followup)

    return StopSnapshot(
        main_sha=git("rev-parse", "--short", INTEGRATION_BRANCH),
        in_flight=[
            InFlightAgent(
                plan=a.plan,
                role=a.role,
                agent_id=a.agent_id,
                worktree=a.worktree,
                note=a.note,
            )
            for a in agents
            if a.status == "running"
        ],
        merge_queue=[
            QueuedMerge(plan=m.plan, verdict=m.verdict, commit=m.commit) for m in queue
        ],
        followups_total=len(followups),
        followups_pnew=sum(1 for f in followups if f.proposed_plan == "P-new"),
    )


if __name__ == "__main__":
    cli_schema_or_run(StopSnapshot, run)
