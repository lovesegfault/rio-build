"""Typed agent-boundary contracts — all pydantic models in one place.

Leaf module: no onibus imports. Pure data. Every model here is a wire contract
between an agent (producer) and a skill/coordinator (consumer). Drift is caught
at .model_validate_json() time.

Scope: every file code reads becomes JSONL. Markdown pipe-tables survive ONLY
as display (verifier's ## Follow-ups in its human-read report — never parsed).
"""

from __future__ import annotations

import re
from datetime import datetime
from pathlib import Path
from typing import Literal, get_args

from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator


# ─── AgentRow ────────────────────────────────────────────────────────────────
# Lifecycle: running → done → consumed
#   done     = agent finished; dag_tick hasn't acted yet
#   consumed = dag_tick acted (launched verifier / appended merge-queue)

AgentRole = Literal["impl", "verify", "review", "merge", "writer", "qa"]
AgentStatus = Literal["running", "done", "consumed"]


class AgentRow(BaseModel):
    plan: str
    role: AgentRole
    agent_id: str | None = None
    worktree: str | None = None
    status: AgentStatus
    note: str = ""


# ─── MergeQueueRow ───────────────────────────────────────────────────────────

Verdict = Literal["PASS", "PARTIAL-accepted"]
GateKind = Literal["plan_merged", "ci_green", "manual"]


class Gate(BaseModel):
    """Structured merge-queue blocker. Replaces free-text `blocker`.

    Semantics (for /dag-tick or whatever consumes the queue):
      plan_merged  — clears when dag.jsonl[plan].status == "DONE"
      ci_green     — clears when log_path exists and contains "status = Built"
                     (the S3-403-aware check)
      manual       — never auto-clears; coordinator removes the gate explicitly

    Example — P0127's old blocker:"P0132 (cmd/eval.rs rebase order)" becomes:
      {"kind": "plan_merged", "plan": 132}
    """

    kind: GateKind
    plan: int | None = Field(default=None, description="For kind=plan_merged")
    log_path: str | None = Field(default=None, description="For kind=ci_green")
    reason: str = Field(default="", description="For kind=manual")


class MergeQueueRow(BaseModel):
    plan: str
    worktree: str
    verdict: Verdict
    commit: str
    gate: Gate | None = Field(
        default=None,
        description="Structured blocker. None = ready to merge. "
        "gate_is_clear() checks the condition.",
    )


# ─── Followup ────────────────────────────────────────────────────────────────
# Severities from rio-impl-reviewer.md §5 (verifier narrowed to verdict-only;
# reviewer owns the smell catalog + followup sink now)

Severity = Literal["trivial", "test-gap", "doc-bug", "perf", "correctness", "feature"]

# FollowupOrigin is the typed twin of source_plan — replaces CADENCE_PREFIXES
# string-sniffing. `reviewer` is inferred when the CLI positional is P<N>; the
# rest are passed literally.
FollowupOrigin = Literal[
    "reviewer", "consolidator", "bughunter", "coverage", "inline", "coordinator"
]
FOLLOWUP_ORIGINS = frozenset(get_args(FollowupOrigin))

_PROPOSED_RE = re.compile(r"^(P-new|P-batch-[a-z-]+|P\d{4})$")


class Followup(BaseModel):
    severity: Severity
    description: str
    file_line: str | None = None
    proposed_plan: str = Field(description="'P-batch-trivial' | 'P-new' | 'P0143'")
    deps: str | None = None
    source_plan: str = Field(
        description="Display string: which phase's verify produced this "
        "('P0109', 'consolidator', 'coverage'). Human-readable; not parsed."
    )
    origin: FollowupOrigin = Field(
        description="Typed producer identity. dag_tick filters cadence rows on "
        "this (consolidator|bughunter), not on description prefix. CLI sets it "
        "from the positional arg.",
    )
    discovered_from: int | None = Field(
        default=None,
        description="Plan number where this was found (None for tooling/meta). "
        "Structured twin of source_plan. When /plan promotes this to a "
        "plan doc, the new plan's dag.jsonl deps should include this — closes "
        "the loop (P0192 discovered during P0109's verify → P0192 deps include 109).",
    )
    timestamp: str = Field(description="ISO 8601")

    @field_validator("proposed_plan")
    @classmethod
    def _proposed_plan_wellformed(cls, v: str) -> str:
        # Catches "p-new", "P-New", "P-123" at write time. Same pattern as
        # KnownFlake.fix_owner — prose rule → validator.
        if not _PROPOSED_RE.match(v):
            raise ValueError(
                f"proposed_plan must match P-new|P-batch-<kind>|P<NNNN>, got {v!r}"
            )
        return v


# ─── KnownFlake ──────────────────────────────────────────────────────────────
# Full model — old _lib.py only read test+fix_owner, discarded symptom/root_cause/
# retry. JSONL preserves all. fix_description split from fix_owner (old table
# had prose after the ID).

Retry = Literal["Once", "Never", "Twice"]

_OWNER_RE = re.compile(r"^P\d+( T\d+)?$")


class Mitigation(BaseModel):
    """One entry in a KnownFlake's fix-history. Was: [PXXX LANDED sha: note]
    bracket-appends in fix_description (grew to 1584 chars on
    known-flakes.jsonl:11 after 4 same-shape appends; 5c68733e did
    string-surgery to fix a premature append — structured list avoids that
    failure mode)."""
    plan: int = Field(description="P<N> → N")
    landed_sha: str = Field(pattern=r"^[0-9a-f]{8,40}$")
    note: str = Field(description="What the mitigation does + any new symptom string")


class KnownFlake(BaseModel):
    test: str = Field(
        description="Flake-attr name (vm-lifecycle-recovery-k3s) for VM tests, "
        "crate::module::test_name for nextest. Human-identifier."
    )
    drv_name: str | None = Field(
        default=None,
        description="nixosTest name attr — the <N> in vm-test-run-<N>.drv as it "
        "appears in `error: Cannot build` CI log lines. VM tests ONLY (nextest "
        "entries leave this None). Match key for excusable() _VM_FAIL_RE. Set "
        "from nix/tests/scenarios/*.nix `name = \"rio-...\"` composition, NOT "
        "the default.nix attrset key. e.g., vm-lifecycle-recovery-k3s → "
        "rio-lifecycle-recovery (lifecycle.nix name=\"rio-lifecycle-${name}\" "
        "+ default.nix name=\"recovery\")."
    )
    symptom: str
    root_cause: str
    fix_owner: str = Field(
        description="Concrete P<N> or P<N> T<M> — validator enforces"
    )
    fix_description: str = Field(
        description="What the fix does (was prose after fix_owner)"
    )
    retry: Retry
    mitigations: list[Mitigation] = Field(
        default_factory=list,
        description="Ordered fix-history. Replaces [PXXX LANDED sha: note] "
        "bracket-appends in fix_description. Appended via `onibus flake "
        "mitigation <test> <plan> <sha> <note>`.",
    )

    @field_validator("fix_owner")
    @classmethod
    def _concrete_owner(cls, v: str) -> str:
        # Enforces the "bridge not parking lot" rule. P-batch-* → ValidationError at write.
        if not _OWNER_RE.match(v):
            raise ValueError(
                f"fix_owner must be 'P<N>' or 'P<N> T<M>', got {v!r}. "
                "Create the plan via /plan --inline first."
            )
        return v

    @model_validator(mode="after")
    def _match_surface_defined(self) -> "KnownFlake":
        # Sentinel entries (<tcg-builder-allocation> etc.) are intentionally
        # unmatchable by name — provenance-only rows that P0304 T10's
        # _TCG_MARKERS handles via log-grep. They need neither surface.
        if self.test.startswith("<"):
            return self
        # VM-test entries (vm-*-k3s, vm-*-standalone) need drv_name for
        # _VM_FAIL_RE matching. Nextest entries (crate::path) match via test.
        if self.test.startswith("vm-") and self.drv_name is None:
            raise ValueError(
                f"VM-test known-flake {self.test!r} missing drv_name — "
                f"_VM_FAIL_RE matches against drv_name, not test. Set drv_name "
                f"to the nixosTest name attr (see nix/tests/scenarios/*.nix)."
            )
        return self

    @property
    def owner_plan(self) -> int:
        # Validator guarantees the match succeeds.
        m = re.match(r"^P(\d+)", self.fix_owner)
        assert m is not None  # validator guarantee
        return int(m.group(1))


# ─── MergerReport ────────────────────────────────────────────────────────────
# rio-impl-merger emits this as a fenced ```json block in its report. The prose
# above the fence is human-readable; the fence IS the contract. merge-impl/SKILL
# and dag-run match on `report.abort_reason`, not on section headers.

MergerStatus = Literal["merged", "aborted"]
MergerAbortReason = Literal[
    "rebase-conflict",
    "ff-rejected",
    "ci-failed",
    "non-convco-commits",
    "worktree-missing",
    "already-merged",
    "lock-held",
]


class MergerReport(BaseModel):
    status: MergerStatus
    abort_reason: MergerAbortReason | None = None
    hash: str | None = Field(default=None, description="New main sha (if merged)")
    commits_merged: int | None = None
    stale_verify_commits_moved: int = Field(
        default=0,
        description="Rebase moved N commits at merge time. >3 means the verify "
        "PASS examined older code — soft signal, .#ci still gated.",
    )
    dag_delta_commit: str | None = Field(
        default=None, description="sha of dag.jsonl flip, or 'already-done'"
    )
    cov_log: str | None = Field(
        default=None, description="/tmp/merge-cov-<branch>.log (backgrounded)"
    )
    failure_detail: str = Field(
        default="", description="Log tail / conflict files / non-convco subjects"
    )
    behind_worktrees: list[str] = Field(
        default_factory=list,
        description="Informational — impls self-rebase at their gate",
    )
    unblocked: list[int] = Field(
        default_factory=list,
        description="Plans that entered frontier because this merge flipped N→DONE. "
        "Computed via Dag.unblocked_by(N) at merger step 7.5. Coordinator "
        "launches these immediately — replaces the 're-check frontier' prose.",
    )
    cadence: CadenceReport | None = Field(
        default=None,
        description="Merger step 7.5. Coordinator reads consolidator.due / "
        "bughunter.due + ranges. Typed so `onibus schema MergerReport` shows "
        "the actual shape instead of {additionalProperties:true}.",
    )
    cleanup: str = "ok"


# ─── CoverageResult ──────────────────────────────────────────────────────────


class CoverageResult(BaseModel):
    branch: str
    exit_code: int
    log_path: str
    merged_at: str = Field(description="main hash at merge time")


# ─── MergeSha ────────────────────────────────────────────────────────────────


class MergeSha(BaseModel):
    """One row in state/merge-shas.jsonl — merge-count → integration-branch
    tip at that merge. Written by count_bump() AFTER the merger's amend
    (P0319 fix @ 8a1ed8cd — before the fix, pre-amend SHAs dangled).
    Read by _cadence_range() to compute git ranges for consolidator/
    bughunter agents. Last-row-per-mc wins (count-bump --set-to can
    re-record the same mc with a different tip after a reset).

    `plan` added by P0417: identifies which plan's merge caused
    this bump. Used by dag_flip's already-done path to distinguish
    coordinator-fast-path-never-bumped (no row for plan → bump) from
    merger-crashed-post-bump-re-invoked (row exists → skip).
    Optional — older rows have `plan=None`; `--set-to` manual rewinds
    also write `plan=None` (no single plan corresponds to a rewind)."""
    mc: int = Field(ge=0, description="merge-count — value AFTER this bump")
    sha: str = Field(
        pattern=r"^[0-9a-f]{8,40}$",
        description="git rev-parse <integration-branch> — full 40-hex or "
        "abbreviated ≥8-hex. Same pattern as Mitigation.landed_sha.",
    )
    ts: datetime = Field(description="UTC timestamp of the bump")
    plan: int | None = Field(
        default=None,
        description="plan number whose merge caused this bump; None for "
        "older rows pre-P0417 or for --set-to manual rewinds",
    )


# ─── PlanRow ─────────────────────────────────────────────────────────────────
# dag.jsonl is the DAG. `dag render` emits a markdown pipe-table to stdout for
# human review. No file splice — dag.jsonl IS the DAG.

PlanStatus = Literal["UNIMPL", "PARTIAL", "DONE", "RESERVED"]
PlanComplexity = Literal["LOW", "MED", "HIGH"]


class PlanRow(BaseModel):
    model_config = ConfigDict(validate_assignment=True)

    plan: int = Field(description="P0022 → 22")
    title: str
    deps: list[int] = Field(
        default_factory=list,
        description="Integer deps parsed from Deps column. Queryable subset.",
    )
    deps_raw: str | None = Field(
        default=None,
        description="Escape hatch: verbatim Deps cell when it can't round-trip "
        "via deps list (ellipsis '…', P20b(deferred-nodoc)). Renderer prefers "
        "this if set. Writer-appended rows never set it.",
    )
    tracey_total: int = Field(
        default=0, description="Exit column — exit-criteria count"
    )
    tracey_covered: int = Field(
        default=0, description="Mrk column — tracey marker count"
    )
    crate: str = ""
    priority: int = Field(
        default=50, ge=1, le=100,
        description="Declared priority. Default 50 = 'no opinion' — impact "
        "decides among equals. Propagates BACKWARD through deps: if P0294=90 "
        "depends on P0280, P0280's effective priority is ≥90 (launching it "
        "is on the critical path to the high-prio target). Strict sort: "
        "priority beats impact.",
    )
    status: PlanStatus
    complexity: PlanComplexity | None = Field(
        default="MED", description="Ready column. None → '-' (P0122 RESERVED)."
    )
    note: str = Field(
        default="",
        description="PARTIAL annotation, e.g. '(2/37 — T26/T28 @ `ebce112`)'",
    )

    def _deps_cell(self) -> str:
        if self.deps_raw is not None:
            return self.deps_raw
        if not self.deps:
            return "-"
        return ",".join(f"P{d:04d}" for d in self.deps)

    def _status_cell(self, frontier: bool) -> str:
        # Bold = frontier (all deps DONE). DONE/RESERVED never bold.
        s = self.status
        if frontier and s in ("UNIMPL", "PARTIAL"):
            s = f"**{s}**"
        if self.note:
            s = f"{s} {self.note}"
        return s

    def render(self, frontier: bool, eff_prio: int = 50) -> str:
        cx = self.complexity if self.complexity else "-"
        # Blank at 50 (noise). Show declared; append ← if inherited via propagation.
        prio = ""
        if self.priority != 50:
            prio = str(self.priority)
        elif eff_prio != 50:
            prio = f"{eff_prio}←"
        return (
            f"| P{self.plan:04d} | {self.title} | {self._deps_cell()} "
            f"| {self.tracey_total} | {self.tracey_covered} | {self.crate} "
            f"| {prio} | {self._status_cell(frontier)} | {cx} |"
        )


# ─── PlanFile + CollisionRow ─────────────────────────────────────────────────
# Plan docs declare files they touch in a fenced ```json files block.

FileAction = Literal["NEW", "MODIFY", "DELETE", "RENAME"]


class PlanFile(BaseModel):
    # rio-build deltas from rix: crates/ → rio-*/ (crates at repo root),
    # + migrations/ (sqlx), + infra/ (helm/eks), + scripts/, - systemd/ - tests/ - benches/ - deny.toml
    path: str = Field(
        pattern=r"^(rio-[a-z-]+/|nix/|docs/|infra/|migrations/|scripts/|flake\.nix|\.claude/|Cargo|justfile|\.config/|codecov\.yml)"
    )
    action: FileAction = "MODIFY"
    note: str = ""


class CollisionRow(BaseModel):
    path: str
    plans: list[int] = Field(description="Plan numbers touching this path")
    count: int


# ─── Worktree ────────────────────────────────────────────────────────────────


class Worktree(BaseModel):
    path: Path
    branch: str | None  # None if detached HEAD
    head: str

    @property
    def plan_num(self) -> int | None:
        """Extract plan number from branch name like 'p142' → 142."""
        if self.branch and (m := re.fullmatch(r"p(\d+)", self.branch)):
            return int(m.group(1))
        return None


# ─── BuildReport ─────────────────────────────────────────────────────────────

BuildRole = Literal["impl", "verify", "merge", "writer"]


class BuildReport(BaseModel):
    target: str
    branch: str
    rc: int = Field(description="0 green; nonzero build or copy failed")
    log_bytes: int = Field(description="Build-log size at proc.wait()")
    log_path: str = Field(
        description="Always kept. /tmp/rio-dev/rio-{plan}-{role}-{iter}.log"
    )
    store_path: str | None = Field(
        default=None,
        description="Set when --copy succeeded. No result/ symlink -- this is the handle.",
    )
    log_tail: list[str] = Field(description="Last 80 lines if red. Empty if green.")
    role: BuildRole
    iter: int


# ─── Merger compound-op results (NEW — absorbed from bash blocks) ────────────


class Preflight(BaseModel):
    """onibus merge preflight BRANCH — was merger.md:45-50 bash."""
    worktree_exists: bool
    branch_exists: bool
    commits_ahead: int
    clean: bool
    reason: str = Field(description='"" if clean; else first failing check')


class ConvcoResult(BaseModel):
    """onibus merge convco-check RANGE — was merger.md:59-63 bash loop."""
    violations: list[str] = Field(
        description="Subject lines that don't match "
        r"^(feat|fix|perf|refactor|test|docs|chore)(\(...\))?: "
    )
    clean: bool


class RebaseResult(BaseModel):
    """onibus merge rebase-anchored BRANCH — was merger.md:70-81 bash.
    On conflict, the rebase is already aborted before this returns."""
    status: Literal["ok", "conflict", "no-op"]
    pre_rebase: str = Field(description="SHA before rebase — no more $pre_rebase shell var")
    moved: int = Field(description="rev-list --count pre_rebase..HEAD; 0 if no-op/conflict")
    conflict_files: list[str] = Field(default_factory=list)


class FfResult(BaseModel):
    """onibus merge ff-try BRANCH — was merger.md:88-93 bash.
    Does NOT include CI gate or rollback; those stay as discrete agent steps."""
    status: Literal["ok", "not-ff"]
    pre_merge: str = Field(description="Rollback anchor — agent reads from JSON")
    post_merge: str = Field(description="HEAD after; == pre_merge if not-ff")


class BehindWorktree(BaseModel):
    path: str
    branch: str
    behind: int


class BehindReport(BaseModel):
    """onibus merge behind-report — was merger.md:159-166 bash loop."""
    worktrees: list[BehindWorktree]


class DagFlipResult(BaseModel):
    """onibus merge dag-flip N — step 7.5 compound (set-status + amend + count-bump).

    Replaces the 7-command bash block in rio-impl-merger.md step 7.5. The
    bare `git commit --amend` there ran in the merger agent's bash-cwd,
    which isn't reliably the main worktree (P0401: amended to d1449fad
    but sprint-1 stayed at 4fc05cfe — the amend ran in a context where
    the branch-ref didn't follow HEAD). Python owns cwd now."""
    plan: int
    amend_sha: str = Field(
        description="post-amend HEAD (short), or 'already-done' if dag was "
        "pre-flipped. already-done covers two cases (P0417): "
        "(a) coord-fast-path-never-bumped → mc freshly incremented; "
        "(b) merger-crashed-post-bump-re-invoked → mc from prior MergeSha "
        "row (NOT re-bumped). Caller can't distinguish from this field "
        "alone — check mc against merge-count.txt if that matters."
    )
    mc: int = Field(description="merge-count after bump")
    unblocked: list[int] = Field(
        description="plans entering frontier because of this flip — "
        "Dag.unblocked_by(N) pre-flip"
    )
    queue_consumed: int = Field(
        default=0,
        description="merge-queue.jsonl rows removed for this plan"
    )


class BehindCheck(BaseModel):
    """onibus merge behind-check WORKTREE — validator's step-0 compound query.
    Was: `git rev-list --count HEAD..$TGT` + 3-dot file-intersection bash."""
    behind: int
    file_collision: list[str] = Field(
        description="3-dot intersection: files THIS worktree changed AND $TGT "
        "added since merge-base. Empty → rebase will be trivial (no conflicts)."
    )
    trivial_rebase: bool = Field(description="behind > 0 and file_collision empty")
    phantom_amend: bool = Field(
        default=False,
        description="behind==1 AND the oldest commit exclusive to our side "
        "differs from $TGT's tip only in dag.jsonl/merge-shas.jsonl (the "
        "merger's step-7.5 amend-files) AND carries the same commit message "
        "(amend --no-edit preserves it). This worktree rebased onto the "
        "pre-amend SHA during the ff→amend window; `git rebase $TGT` will "
        "auto-drop the patch-already-upstream commit. NOT a real collision "
        "despite trivial_rebase=false."
    )


# ─── CadenceReport ───────────────────────────────────────────────────────────


class CadenceWindow(BaseModel):
    due: bool
    range: str | None = Field(description="git range '$since..$TGT' if due, else None")


class CadenceReport(BaseModel):
    """onibus merge cadence — single source of truth for the 5/7 constants.
    Was: `c=$(cat merge-count.txt)` + `c % 5 == 0` + `git log -$((W+1))` bash
    duplicated in merger.md and dag-run/SKILL.md."""
    count: int
    consolidator: CadenceWindow  # mod 5
    bughunter: CadenceWindow     # mod 7


# ─── LockStatus ──────────────────────────────────────────────────────────────


class LockStatus(BaseModel):
    """onibus merge lock-status — promoted from dict. ff_landed computed
    (compare content.main_at_acquire vs current $TGT) so the three prose
    explanations in dag-tick/dag-run/merger collapse to one field read."""
    held: bool
    stale: bool
    content: dict | None
    ff_landed: bool | None = Field(
        default=None,
        description="Only meaningful if stale=True. True → merger died AFTER "
        "ff-merge (partial state: ff landed, cleanup+dag-flip didn't — finish "
        "from step 5). False → died BEFORE ff (just unlock)."
    )


# ─── DagValidation ───────────────────────────────────────────────────────────


class DagValidation(BaseModel):
    ok: bool
    errors: list[str] = Field(default_factory=list)
    warnings: list[str] = Field(default_factory=list)


# ─── ReconcileReport ─────────────────────────────────────────────────────────


class ReconcileReport(BaseModel):
    """onibus state reconcile — agents-running.jsonl vs worktree reality."""
    stale_rows: list[AgentRow] = Field(description="Row exists, worktree doesn't")
    orphan_worktrees: list[str] = Field(description="Worktree exists, no row")
    stuck_agents: list[str] = Field(
        default_factory=list,
        description="running row + worktree exists + ahead==0. Impl might be "
        "spinning. Was 'one extra check per suspect' manual rev-list in "
        "dag-status/SKILL.md.",
    )
    ok: bool


# ─── ExcusableVerdict ────────────────────────────────────────────────────────


class ExcusableVerdict(BaseModel):
    """onibus flake excusable LOG_PATH — was implementer.md:116 prose."""
    excusable: bool
    failing_tests: list[str]
    matched_flakes: list[str]
    reason: str


# ─── Atomicity / Rename (lifted from per-skill scripts) ──────────────────────


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


# ─── Collision live-check (lifted) ───────────────────────────────────────────


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


# ─── Tick / Stop (lifted) ────────────────────────────────────────────────────


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


# ─── TraceyCoverage ──────────────────────────────────────────────────────────


class TraceyMarkerHit(BaseModel):
    id: str
    impl_loc: str | None = Field(description="file:line of r[impl ...] in diff, or None")
    verify_loc: str | None = Field(description="file:line of r[verify ...] in diff, or None")


class TraceyCoverage(BaseModel):
    """onibus plan tracey-coverage BRANCH PLAN_DOC — validator step 4's
    PASS/PARTIAL boundary. Was: plan tracey-markers → diff grep r[impl] →
    diff grep r[verify] → manual 3-way table join. The sole gate for 'plan
    claimed coverage, impl forgot marker' (tracey-validate catches dangling
    refs, not missing ones)."""
    markers: list[TraceyMarkerHit]
    unmatched: list[str] = Field(
        description="Plan claims these, diff has neither r[impl] nor r[verify]. "
        "Non-empty → FAIL per validator.md:97 rule."
    )
    covered: int
    total: int


# ─── Forward-ref resolution ──────────────────────────────────────────────────
# MergerReport.cadence references CadenceReport which is defined below it.
# __future__ annotations makes the string-form work at class-body time;
# model_rebuild resolves it for pydantic's schema generation.
MergerReport.model_rebuild()
