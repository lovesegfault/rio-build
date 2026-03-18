#!/usr/bin/env python3
"""One pass of the DAG runner's mechanical reflex.

Reads state, reports what the coordinator should do. Does NOT launch agents
itself — that's the coordinator's job (agents-launching-agents isn't the
pattern here; the script computes, the skill invokes, the coordinator acts).

Idempotent: running twice with no state change → second run is all-zeros.

Usage:
    dag_tick.py
    dag_tick.py --schema
"""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field

from _lib import cli_schema_or_run
from state import (
    KNOWN_FLAKES,
    STATE_DIR,
    AgentRow,
    CoverageResult,
    Followup,
    KnownFlake,
    consume_jsonl,
    read_jsonl,
)

# Cadence-agent proposals accumulate in followups-pending but must NOT trigger
# auto-flush — consolidator/bughunter output is advisory; coordinator reviews
# and promotes manually. A P-new consolidation proposal would otherwise flush
# the whole sink the moment it lands. Filter on the typed `origin` field, not
# a description prefix — the prefix is cosmetic.
_CADENCE_ORIGINS = frozenset({"consolidator", "bughunter"})


class ImplNeedsVerify(BaseModel):
    plan: str
    worktree: str
    note: str = Field(description="Scrutiny-seed hints from the agents-running row")


class VerifyOutcome(BaseModel):
    plan: str
    verdict: Literal["PASS", "PARTIAL", "FAIL", "BEHIND"]
    output_path: str | None


class CoverageRegression(BaseModel):
    branch: str
    exit_code: int
    log_path: str


class TickReport(BaseModel):
    impls_needing_verify: list[ImplNeedsVerify] = Field(
        description="role=impl, status=done, no verify row yet. "
        "Coordinator: launch /validate-impl for each."
    )
    verify_pass: list[VerifyOutcome] = Field(
        description="VERDICT: PASS, not yet in merge-queue. "
        "Coordinator: append to merge-queue (you decide order)."
    )
    verify_needs_judgment: list[VerifyOutcome] = Field(
        description="PARTIAL/FAIL/BEHIND. Coordinator judgment required — "
        "fix-then-merge vs accept-with-followups vs bounce-to-impl."
    )
    followups_row_count: int = Field(description="Rows in followups-pending.jsonl")
    followups_cadence_count: int = Field(
        description="Subset: cadence-agent rows (origin in {consolidator,bughunter}). "
        "Excluded from flush trigger — advisory, coordinator reviews manually."
    )
    followups_should_flush: bool = Field(
        description="row_count > 15 OR any P-new row present. P-new becomes a "
        "schedulable DAG node — don't sit on it. P-batch-* rows are T-task "
        "appends, they can wait. Allocation is serial in /plan (not "
        "here), so concurrent writers are fine. Coordinator: invoke /plan."
    )
    coverage_regressions: list[CoverageRegression]
    flake_fix_phases: list[int] = Field(
        description="Phase numbers from known-flakes.jsonl fix_owner field. "
        "Coordinator: prioritize these in frontier launches."
    )


def _scan_agents_running() -> tuple[list[ImplNeedsVerify], set[str]]:
    """Returns (impls-needing-verify, phases-with-verify-row)."""
    agents = read_jsonl(STATE_DIR / "agents-running.jsonl", AgentRow)
    verify_phases = {a.plan for a in agents if a.role == "verify"}
    impls = [
        ImplNeedsVerify(plan=a.plan, worktree=a.worktree or "", note=a.note)
        for a in agents
        if a.role == "impl" and a.status == "done" and a.plan not in verify_phases
    ]
    return impls, verify_phases


def _scan_verify_outcomes(verify_phases: set[str]) -> tuple[list, list]:
    """Scan task output files for VERDICT lines. Returns (pass, needs_judgment)."""
    # Task outputs live in /tmp/claude-*/*/tasks/*.output — but we don't have
    # the agent-id mapping if it's from a prior session (agents-running has None).
    # For now, return empty; a future pass could scan all task outputs and
    # correlate by worktree path mentioned in the output. Coordinator handles
    # verify outcomes manually until that's wired.
    _ = verify_phases  # will be used when task-output scanning is wired
    return [], []


def _scan_coverage_pending() -> list[CoverageRegression]:
    # consume semantics: read-all-then-truncate — each result processed once.
    results = consume_jsonl(STATE_DIR / "coverage-pending.jsonl", CoverageResult)
    return [
        CoverageRegression(branch=r.branch, exit_code=r.exit_code, log_path=r.log_path)
        for r in results
        if r.exit_code != 0
    ]


def _followups_state() -> tuple[int, int, bool]:
    followups = read_jsonl(STATE_DIR / "followups-pending.jsonl", Followup)
    # Cadence proposals don't count — they're advisory, coordinator-reviewed.
    # Filtering them out of BOTH the >15 count AND the P-new trigger matters:
    # consolidator writes proposed_plan="P-new" which would insta-flush otherwise.
    # Match on the typed origin field; the CONSOLIDATION:/BUGHUNT: description
    # prefix is cosmetic (human-readable, not load-bearing).
    cadence = [f for f in followups if f.origin in _CADENCE_ORIGINS]
    actionable = [f for f in followups if f.origin not in _CADENCE_ORIGINS]
    has_standalone = any(f.proposed_plan == "P-new" for f in actionable)
    return len(followups), len(cadence), len(actionable) > 15 or has_standalone


def run() -> TickReport:
    impls, verify_phases = _scan_agents_running()
    verify_pass, verify_judgment = _scan_verify_outcomes(verify_phases)
    followups_count, cadence_count, should_flush = _followups_state()
    # Validator guarantees owner_plan is never None — simpler than before.
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


if __name__ == "__main__":
    cli_schema_or_run(TickReport, run)
