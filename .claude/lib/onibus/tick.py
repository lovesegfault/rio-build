"""Mechanical reflex (tick) + handoff snapshot (stop).

Both read state, report what the coordinator should do. Neither launches
agents or mutates — the script computes, the skill invokes, the coordinator
acts. Idempotent: tick twice with no state change → second is all-zeros.
"""

from __future__ import annotations

from onibus import INTEGRATION_BRANCH, KNOWN_FLAKES, STATE_DIR
from onibus.git_ops import git
from onibus.jsonl import consume_jsonl, read_jsonl
from onibus.models import (
    AgentRow,
    CoverageRegression,
    CoverageResult,
    Followup,
    ImplNeedsVerify,
    InFlightAgent,
    KnownFlake,
    MergeQueueRow,
    QueuedMerge,
    StopSnapshot,
    TickReport,
    VerifyOutcome,
)

# Cadence-agent proposals accumulate but must NOT trigger auto-flush —
# consolidator/bughunter output is advisory. Match on typed origin, not
# description prefix.
_CADENCE_ORIGINS = frozenset({"consolidator", "bughunter"})


def _scan_agents_running() -> tuple[list[ImplNeedsVerify], set[str]]:
    agents = read_jsonl(STATE_DIR / "agents-running.jsonl", AgentRow)
    verify_phases = {a.plan for a in agents if a.role == "verify"}
    impls = [
        ImplNeedsVerify(plan=a.plan, worktree=a.worktree or "", note=a.note)
        for a in agents
        if a.role == "impl" and a.status == "done" and a.plan not in verify_phases
    ]
    return impls, verify_phases


def _scan_verify_outcomes(verify_phases: set[str]) -> tuple[list[VerifyOutcome], list[VerifyOutcome]]:
    # Task outputs live in /tmp/claude-*/*/tasks/*.output but agent-id mapping
    # may be None (prior-session). Coordinator handles verify outcomes manually
    # until task-output scanning is wired.
    _ = verify_phases
    return [], []


def _scan_coverage_pending() -> list[CoverageRegression]:
    results = consume_jsonl(STATE_DIR / "coverage-pending.jsonl", CoverageResult)
    return [
        CoverageRegression(branch=r.branch, exit_code=r.exit_code, log_path=r.log_path)
        for r in results
        if r.exit_code != 0
    ]


def _followups_state() -> tuple[int, int, bool]:
    followups = read_jsonl(STATE_DIR / "followups-pending.jsonl", Followup)
    cadence = [f for f in followups if f.origin in _CADENCE_ORIGINS]
    actionable = [f for f in followups if f.origin not in _CADENCE_ORIGINS]
    has_standalone = any(f.proposed_plan == "P-new" for f in actionable)
    return len(followups), len(cadence), len(actionable) > 15 or has_standalone


def tick() -> TickReport:
    impls, verify_phases = _scan_agents_running()
    verify_pass, verify_judgment = _scan_verify_outcomes(verify_phases)
    followups_count, cadence_count, should_flush = _followups_state()
    flake_phases = sorted({f.owner_plan for f in read_jsonl(KNOWN_FLAKES, KnownFlake)})
    return TickReport(
        impls_needing_verify=impls,
        verify_pass=verify_pass,
        verify_needs_judgment=verify_judgment,
        followups_row_count=followups_count,
        followups_cadence_count=cadence_count,
        followups_should_flush=should_flush,
        coverage_regressions=_scan_coverage_pending(),
        flake_fix_phases=flake_phases,
    )


def stop() -> StopSnapshot:
    agents = read_jsonl(STATE_DIR / "agents-running.jsonl", AgentRow)
    queue = read_jsonl(STATE_DIR / "merge-queue.jsonl", MergeQueueRow)
    followups = read_jsonl(STATE_DIR / "followups-pending.jsonl", Followup)
    return StopSnapshot(
        main_sha=git("rev-parse", "--short", INTEGRATION_BRANCH),
        in_flight=[
            InFlightAgent(
                plan=a.plan, role=a.role, agent_id=a.agent_id,
                worktree=a.worktree, note=a.note,
            )
            for a in agents if a.status == "running"
        ],
        merge_queue=[
            QueuedMerge(plan=m.plan, verdict=m.verdict, commit=m.commit) for m in queue
        ],
        followups_total=len(followups),
        followups_pnew=sum(1 for f in followups if f.proposed_plan == "P-new"),
    )
