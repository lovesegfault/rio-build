"""Typed agent-boundary contracts — pydantic models + JSONL primitives + CLI.

The COV_EXIT= bug (rix 8ce3e08): merger wrote COV_PID=, dag_tick grepped
COV_EXIT=, coverage regressions silently invisible. Root cause is
stringly-typed agent boundaries. Pipe-table columns are positionally matched;
schema lives in prose; producer/consumer drift silently.

Pydantic models ARE the contract: producer writes .model_dump_json(), consumer
reads .model_validate_json(). Same class both sides. Can't drift. Validation at
the boundary is the guarantee.

Scope: every file code reads becomes JSONL. Markdown pipe-tables survive ONLY
as display (verifier's ## Follow-ups in its human-read report — never parsed).

Ported from rix @ 76cac2e. rio-build deltas:
  - PlanFile.path validator: crates/ → rio-*/ + migrations/ + infra/ + scripts/
  - Comment refs: rix-* → rio-*
"""

from __future__ import annotations

import json
import re
import sys
import warnings
from datetime import datetime
from pathlib import Path
from typing import Literal, TypeVar
from typing import get_args

from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator

REPO_ROOT = Path(__file__).resolve().parents[2]
STATE_DIR = REPO_ROOT / ".claude" / "state"
KNOWN_FLAKES = REPO_ROOT / ".claude" / "known-flakes.jsonl"
DAG_JSONL = REPO_ROOT / ".claude" / "dag.jsonl"
WORK_DIR = REPO_ROOT / ".claude" / "work"
COLLISIONS_JSONL = REPO_ROOT / ".claude" / "collisions.jsonl"

T = TypeVar("T", bound=BaseModel)


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


def gate_is_clear(gate: Gate | None, dag_rows: list[PlanRow]) -> bool:
    """Check whether a merge-queue gate condition is satisfied.

    Caller supplies dag_rows (read_jsonl(DAG_JSONL, PlanRow)) — passed in
    rather than read here so the caller can read once for N rows, and so
    this is unit-testable without filesystem.
    """
    if gate is None:
        return True
    match gate.kind:
        case "plan_merged":
            assert gate.plan is not None, "plan_merged gate requires plan"
            return any(r.plan == gate.plan and r.status == "DONE" for r in dag_rows)
        case "ci_green":
            assert gate.log_path is not None, "ci_green gate requires log_path"
            p = Path(gate.log_path)
            return p.exists() and "status = Built" in p.read_text()
        case "manual":
            return False  # never auto-clears; coordinator removes explicitly


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
    blocker: str | None = Field(
        default=None,
        description="DEPRECATED free-text blocker. Use `gate` instead. "
        "Kept so old merge-queue.jsonl rows still parse.",
    )

    @model_validator(mode="after")
    def _warn_blocker_without_gate(self) -> MergeQueueRow:
        # Migration nudge: old callers setting blocker but not gate.
        if self.blocker is not None and self.gate is None:
            warnings.warn(
                f"MergeQueueRow.blocker is deprecated; use gate instead. "
                f"Free-text blocker={self.blocker!r} is not mechanically checkable.",
                DeprecationWarning,
                stacklevel=2,
            )
        return self


# ─── Followup ────────────────────────────────────────────────────────────────
# Severities from rio-impl-reviewer.md §5 (verifier narrowed to verdict-only;
# reviewer owns the smell catalog + followup sink now)

Severity = Literal["trivial", "test-gap", "doc-bug", "perf", "correctness", "feature"]

# FollowupOrigin is the typed twin of source_plan — replaces CADENCE_PREFIXES
# string-sniffing. `reviewer` is inferred when the CLI positional is P<N>; the
# rest are passed literally. None = legacy row pre-this-change.
FollowupOrigin = Literal[
    "reviewer", "consolidator", "bughunter", "coverage", "inline", "coordinator"
]
_FOLLOWUP_ORIGINS = frozenset(get_args(FollowupOrigin))

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
    origin: FollowupOrigin | None = Field(
        default=None,
        description="Typed producer identity. None = legacy pre-origin row. "
        "dag_tick filters cadence rows on this (consolidator|bughunter), not "
        "on description prefix. CLI sets it from the positional arg.",
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
_P_NUM_RE = re.compile(r"^P(\d+)$")


class KnownFlake(BaseModel):
    test: str
    symptom: str
    root_cause: str
    fix_owner: str = Field(
        description="Concrete P<N> or P<N> T<M> — validator enforces"
    )
    fix_description: str = Field(
        description="What the fix does (was prose after fix_owner)"
    )
    retry: Retry

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
    cleanup: str = "ok"


# ─── CoverageResult ──────────────────────────────────────────────────────────


class CoverageResult(BaseModel):
    branch: str
    exit_code: int
    log_path: str
    merged_at: str = Field(description="main hash at merge time")


# ─── PlanRow ────────────────────────────────────────────────────────────────────
# dag.jsonl is the DAG. `dag-render` emits a markdown pipe-table to stdout
# for human review (`python3 state.py dag-render | less`). No file splice —
# DAG.md is gone; the prose sections it carried (collision matrix, serialization
# chains) were apologizing for drift that no longer exists now that dag.jsonl
# + collisions.jsonl are the machine-readable truth.

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

    def render(self, frontier: bool) -> str:
        cx = self.complexity if self.complexity else "-"
        return (
            f"| P{self.plan:04d} | {self.title} | {self._deps_cell()} "
            f"| {self.tracey_total} | {self.tracey_covered} | {self.crate} "
            f"| {self._status_cell(frontier)} | {cx} |"
        )


def _compute_frontier(rows: list[PlanRow]) -> set[int]:
    # Plan is in frontier if UNIMPL/PARTIAL and ALL integer deps are DONE.
    # deps_raw-only rows (P0152's P20b) are conservatively excluded — unknown dep state.
    done = {r.plan for r in rows if r.status == "DONE"}
    return {
        r.plan
        for r in rows
        if r.status in ("UNIMPL", "PARTIAL")
        and r.deps_raw is None
        and all(d in done for d in r.deps)
    }


def dag_render_table(rows: list[PlanRow]) -> str:
    """Render sorted rows as the plan-table (header + separator + data rows)."""
    rows = sorted(rows, key=lambda r: r.plan)
    frontier = _compute_frontier(rows)
    lines = [
        "| P# | Title | Deps | Exit | Mrk | Crates | Status | Ready |",
        "|---|---|---|---|---|---|---|---|",
    ]
    lines.extend(r.render(r.plan in frontier) for r in rows)
    return "\n".join(lines)


# ─── PlanFile + collisions ───────────────────────────────────────────────────
# Plan docs declare files they touch in a fenced ```json files block.
# collisions-regen reads all blocks, builds a path→plans index, emits hot
# files (count > 1) to collisions.jsonl. Replaces the hand-maintained DAG.md
# collision matrix with a derived artifact.

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


# ─── JSONL primitives ────────────────────────────────────────────────────────


def read_jsonl(path: Path, model: type[T]) -> list[T]:
    if not path.exists():
        return []
    return [
        model.model_validate_json(line)
        for line in path.read_text().splitlines()
        if line.strip() and not line.startswith("#")
    ]
    # '#' skip lets us put a header comment in known-flakes.jsonl


def append_jsonl(path: Path, row: BaseModel) -> None:
    # Single-line append is atomic for < PIPE_BUF (4096). Rows are ~200 bytes.
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a") as f:
        f.write(row.model_dump_json() + "\n")


def consume_jsonl(path: Path, model: type[T]) -> list[T]:
    # Read-all-then-truncate. For coverage-pending (process-once semantics).
    rows = read_jsonl(path, model)
    if path.exists():
        path.write_text("")
    return rows


def remove_jsonl(path: Path, model: type[T], pred) -> int:
    # Read, filter, rewrite. For known-flake deletion. Returns count removed.
    rows = read_jsonl(path, model)
    keep = [r for r in rows if not pred(r)]
    header: list[str] = []
    if path.exists():
        header = [ln for ln in path.read_text().splitlines() if ln.startswith("#")]
    body = header + [r.model_dump_json() for r in keep]
    path.write_text("\n".join(body) + ("\n" if body else ""))
    return len(rows) - len(keep)


def _warn_if_cwd_elsewhere() -> None:
    # Absolute-path footgun: `python3 /abs/main/.claude/lib/state.py` from a
    # worktree → REPO_ROOT=main (parents[2] of __file__), edit lands in main
    # uncommitted. Relative invocation (`python3 .claude/lib/state.py`) fixes it.
    cwd = Path.cwd().resolve()
    if not cwd.is_relative_to(REPO_ROOT):
        sys.stderr.write(
            f"warning: editing {KNOWN_FLAKES} from cwd={cwd} — REPO_ROOT is "
            f"{REPO_ROOT}. If in a worktree, invoke via relative path so the "
            f"edit lands there.\n"
        )


# ─── CLI ─────────────────────────────────────────────────────────────────────

_HELP = """\
state.py — typed agent-boundary contracts + JSONL state CLI

Subcommands:
  schema <Model>            JSON Schema for any model to stdout
  dag-render                Render dag.jsonl as pipe-table (frontier bold)
  dag-append '<json>'       Append a PlanRow (rio-planner)
  dag-set-status N STATUS   Flip plan N to STATUS [--note '...']
  dag-deps N                Plan N's deps + each dep's status as JSON
  dag-markers               Join UNIMPL ❤ Tracey refs w/ piped tracey-uncovered
                              tracey query uncovered | state.py dag-markers
  collisions-regen          Derive collisions.jsonl from plan docs' json fences
  agent-row '<json>'        Append AgentRow to agents-running.jsonl
  merge-queue '<json>'      Append MergeQueueRow
  coverage BR EC LOG SHA    Append CoverageResult (merger backgrounded subshell)
  followup P<N>|<origin> '<json>'   Append Followup (reviewer/cadence)
  known-flake '<json>'      Append KnownFlake
  known-flake-remove TEST   Remove rows where test=TEST
  known-flake-exists TEST   exit 0 if TEST in bridge table, 1 otherwise

Models: AgentRow MergeQueueRow Gate Followup KnownFlake MergerReport
        CoverageResult PlanRow PlanFile CollisionRow
"""

if __name__ == "__main__":
    if len(sys.argv) < 2 or sys.argv[1] in ("-h", "--help", "help"):
        print(_HELP)
        sys.exit(0)
    match sys.argv[1]:
        case "coverage":
            # merger subshell: python3 state.py coverage <branch> <ec> <log> <merged_at>
            _, _, branch, ec, log, merged_at = sys.argv
            append_jsonl(
                STATE_DIR / "coverage-pending.jsonl",
                CoverageResult(
                    branch=branch,
                    exit_code=int(ec),
                    log_path=log,
                    merged_at=merged_at,
                ),
            )

        case "followup":
            # reviewer / cadence agents: one call per followup, JSON-arg form
            # python3 state.py followup <origin-or-P<N>> '{"severity":"trivial",...}'
            #
            # Positional arg routing:
            #   P<N>            → discovered_from=N, origin="reviewer", source_plan=P<N>
            #   FollowupOrigin  → discovered_from=None, origin=<that>, source_plan=<that>
            #   anything else   → error (typed boundary — no free-text positionals)
            source_arg, row_json = sys.argv[2], sys.argv[3]
            data = json.loads(row_json)
            data["source_plan"] = source_arg
            data["timestamp"] = datetime.now().isoformat()
            m = _P_NUM_RE.match(source_arg)
            if m:
                data.setdefault("discovered_from", int(m.group(1)))
                data.setdefault("origin", "reviewer")
            elif source_arg in _FOLLOWUP_ORIGINS:
                data.setdefault("discovered_from", None)
                data.setdefault("origin", source_arg)
            else:
                print(
                    f"followup positional must be P<N> or one of "
                    f"{sorted(_FOLLOWUP_ORIGINS)}, got {source_arg!r}",
                    file=sys.stderr,
                )
                sys.exit(2)
            append_jsonl(
                STATE_DIR / "followups-pending.jsonl",
                Followup.model_validate(data),
            )

        case "agent-row":
            append_jsonl(
                STATE_DIR / "agents-running.jsonl",
                AgentRow.model_validate_json(sys.argv[2]),
            )

        case "merge-queue":
            append_jsonl(
                STATE_DIR / "merge-queue.jsonl",
                MergeQueueRow.model_validate_json(sys.argv[2]),
            )

        case "known-flake":
            _warn_if_cwd_elsewhere()
            append_jsonl(KNOWN_FLAKES, KnownFlake.model_validate_json(sys.argv[2]))

        case "known-flake-remove":
            # rio-ci-flake-fixer step 6: python3 state.py known-flake-remove <test_name>
            _warn_if_cwd_elsewhere()
            n = remove_jsonl(KNOWN_FLAKES, KnownFlake, lambda f: f.test == sys.argv[2])
            print(f"removed {n} row(s) for test {sys.argv[2]!r}")

        case "known-flake-exists":
            # rio-implementer verification gate: python3 state.py known-flake-exists <test>
            # exit 0 if <test> is in the bridge table (retry allowed), 1 otherwise.
            # Typed path — replaces `grep '"test":"<name>"' .claude/known-flakes.jsonl`.
            rows = read_jsonl(KNOWN_FLAKES, KnownFlake)
            sys.exit(0 if any(f.test == sys.argv[2] for f in rows) else 1)

        case "schema":
            # python3 state.py schema AgentRow → JSON Schema to stdout
            print(json.dumps(globals()[sys.argv[2]].model_json_schema(), indent=2))

        case "dag-set-status":
            # python3 state.py dag-set-status 143 DONE
            # python3 state.py dag-set-status 143 PARTIAL --note '(2/37 — T26/T28)'
            plan_num = int(sys.argv[2])
            new_status: PlanStatus = sys.argv[3]  # type: ignore[assignment]
            note = (
                sys.argv[sys.argv.index("--note") + 1] if "--note" in sys.argv else ""
            )
            rows = read_jsonl(DAG_JSONL, PlanRow)
            found = False
            for r in rows:
                if r.plan == plan_num:
                    r.status = new_status  # validate_assignment=True → pydantic checks the Literal
                    r.note = note
                    found = True
                    break
            if not found:
                print(f"no row for plan {plan_num}", file=sys.stderr)
                sys.exit(1)
            # Rewrite whole file — rows are small, serialization is atomic-enough
            # under /merge-impl (only one merger at a time).
            DAG_JSONL.write_text(
                "".join(
                    r.model_dump_json() + "\n"
                    for r in sorted(rows, key=lambda r: r.plan)
                )
            )
            print(f"P{plan_num:04d} → {new_status}" + (f" {note}" if note else ""))

        case "dag-render":
            # Render plan table to stdout. `python3 state.py dag-render | less`.
            # No file splice — dag.jsonl IS the DAG; this is display-only.
            rows = read_jsonl(DAG_JSONL, PlanRow)
            print(dag_render_table(rows))
            print(
                f"\n# {len(rows)} rows; frontier = {sorted(_compute_frontier(rows))}",
                file=sys.stderr,
            )

        case "collisions-regen":
            # Derive .claude/collisions.jsonl from plan docs' ```json files blocks.
            # Scaffold: most docs don't have the fenced block yet — they fall
            # back to plan_doc_src_files() grep and are logged as unmigrated.
            from collections import Counter, defaultdict

            # Late import: avoid circular (_lib imports nothing from state).
            import importlib.util

            _spec = importlib.util.spec_from_file_location(
                "_lib", REPO_ROOT / ".claude" / "lib" / "_lib.py"
            )
            _lib = importlib.util.module_from_spec(_spec)
            _spec.loader.exec_module(_lib)

            plan_re = re.compile(r"^plan-(\d{4})-")
            path_counter: Counter = Counter()
            path_plans: defaultdict = defaultdict(list)
            unmigrated = []
            for doc in sorted(WORK_DIR.glob("plan-*.md")):
                m = plan_re.match(doc.name)
                if not m:
                    continue
                n = int(m.group(1))
                files = _lib.plan_doc_files(doc)
                if files is None:
                    # Old-format doc — fall back to grep.
                    unmigrated.append(n)
                    paths = _lib.plan_doc_src_files(doc)
                else:
                    paths = [PlanFile.model_validate(f).path for f in files]
                for p in paths:
                    path_counter[p] += 1
                    path_plans[p].append(n)
            hot = [
                CollisionRow(path=p, plans=sorted(path_plans[p]), count=c)
                for p, c in path_counter.items()
                if c > 1
            ]
            hot.sort(key=lambda r: (-r.count, r.path))
            COLLISIONS_JSONL.write_text(
                "".join(r.model_dump_json() + "\n" for r in hot)
            )
            print(f"wrote {len(hot)} hot files to {COLLISIONS_JSONL}")
            print(
                f"unmigrated (grep fallback): {len(unmigrated)} docs", file=sys.stderr
            )
            if "--verbose" in sys.argv:
                for n in unmigrated:
                    print(f"  P{n:04d}", file=sys.stderr)

        case "dag-deps":
            # python3 state.py dag-deps 142 → deps list + each dep's status
            # Replaces grep-for-status in plan/SKILL.md step 3.
            plan_num = int(sys.argv[2])
            rows = read_jsonl(DAG_JSONL, PlanRow)
            by_num = {r.plan: r for r in rows}
            r = by_num.get(plan_num)
            if r is None:
                print(f"no row for plan {plan_num}", file=sys.stderr)
                sys.exit(1)
            out = {
                "plan": r.plan,
                "status": r.status,
                "deps": [
                    {"plan": d, "status": by_num[d].status if d in by_num else "?"}
                    for d in r.deps
                ],
                "deps_raw": r.deps_raw,
                "all_deps_done": all(
                    d in by_num and by_num[d].status == "DONE" for d in r.deps
                ),
            }
            print(json.dumps(out, indent=2))

        case "dag-append":
            # python3 state.py dag-append '{"plan":924999901,"title":"...","status":"UNIMPL",...}'
            # For rio-planner — adds a new row, validates it.
            append_jsonl(DAG_JSONL, PlanRow.model_validate_json(sys.argv[2]))

        case "dag-markers":
            # Planning-gap detector: join UNIMPL plans' ## Tracey refs with
            # tracey-uncovered. Surfaces "marker uncovered AND unclaimed" (gap).
            #
            #   tracey query uncovered | python3 state.py dag-markers
            #
            # stdin: tracey output (anything containing r[domain.*] tokens —
            # format-agnostic, we just grep it). If stdin is a tty (no pipe),
            # skip the uncovered-join and just print the claim map.
            #
            # Output: JSON with three keys:
            #   claimed_uncovered   — scheduled work (good)
            #   unclaimed_uncovered — PLANNING GAP (no plan claims this)
            #   claimed_covered     — stale claim or covered-elsewhere (info)
            from collections import defaultdict

            # Strict form for plan docs: r[domain.area.detail] — requires ≥2
            # dots (rules out file paths like `worker.md`, `store.toml`).
            _DOC_RE = re.compile(
                r"r\[((?:gw|sched|store|worker|ctrl|obs|sec|proto)\.[a-z][a-z0-9-]*\.[a-z0-9.-]+)\]"
            )
            # Lenient form for stdin: tracey output may be bare IDs or bracketed.
            _STDIN_RE = re.compile(
                r"\b((?:gw|sched|store|worker|ctrl|obs|sec|proto)\.[a-z][a-z0-9-]*\.[a-z0-9.-]+)\b"
            )
            # Build claim map: marker -> [plan numbers]
            claims: defaultdict = defaultdict(list)
            unimpl = {r.plan for r in read_jsonl(DAG_JSONL, PlanRow)
                      if r.status in ("UNIMPL", "PARTIAL")}
            plan_re = re.compile(r"^plan-(\d{4})-")
            for doc in sorted(WORK_DIR.glob("plan-*.md")):
                m = plan_re.match(doc.name)
                if not m or int(m.group(1)) not in unimpl:
                    continue
                n = int(m.group(1))
                for marker in set(_DOC_RE.findall(doc.read_text())):
                    claims[marker].append(n)

            # Read tracey uncovered from stdin (pipe); skip if tty
            uncovered: set[str] = set()
            if not sys.stdin.isatty():
                uncovered = set(_STDIN_RE.findall(sys.stdin.read()))

            claimed = set(claims)
            out = {
                "claimed_uncovered": {
                    m: sorted(claims[m]) for m in sorted(claimed & uncovered)
                },
                "unclaimed_uncovered": sorted(uncovered - claimed),
                "claimed_covered": {
                    m: sorted(claims[m]) for m in sorted(claimed - uncovered)
                } if uncovered else "<no tracey input>",
                "_summary": {
                    "unimpl_plans": len(unimpl),
                    "markers_claimed": len(claimed),
                    "tracey_uncovered": len(uncovered),
                    "planning_gaps": len(uncovered - claimed),
                },
            }
            print(json.dumps(out, indent=2))

        case "merge-count-bump":
            # Cadence counter for consolidator (mod 5) / bughunter (mod 7).
            #   python3 state.py merge-count-bump     → read, +1, write, emit
            #   python3 state.py merge-count-bump 34  → set directly (drift correction)
            # Merger's read-only charter is classifier-enforced — raw `echo N >`
            # gets flagged as self-modification. state.py is the controlled
            # boundary (same mechanical-edit-via-bash exception as dag-set-status).
            count_file = STATE_DIR / "merge-count.txt"
            if len(sys.argv) > 2:
                new = int(sys.argv[2])
            else:
                cur = int(count_file.read_text().strip()) if count_file.exists() else 0
                new = cur + 1
            count_file.parent.mkdir(parents=True, exist_ok=True)
            count_file.write_text(f"{new}\n")
            print(new)

        case cmd:
            print(f"unknown subcommand: {cmd!r}", file=sys.stderr)
            sys.exit(2)
