"""Shared helpers for DAG-orchestration scripts.

Pydantic models are the output contracts — each script has a model, emits
`.model_dump_json()` on stdout, and `--schema` prints the JSON Schema.
Skills/agents parse the JSON; the schema is the doc.

Ported from rix @ 76cac2e. rio-build deltas:
  - diff_src_files / plan_doc_src_files: crates/ → rio-*/ regex
  - qa_mechanical_check: r[plan.*] is POLLUTION (FAIL); domain markers WARN
    (rio-build tracey is domain-indexed — plan docs *reference* spec markers,
    they don't *define* them)
"""

from __future__ import annotations

import re
import json
import subprocess
import sys
from pathlib import Path

from pydantic import BaseModel

REPO_ROOT = Path(__file__).resolve().parents[2]
STATE_DIR = REPO_ROOT / ".claude" / "state"
PLAN_DOC_GLOB = ".claude/work/plan-{n:04d}-*.md"


# ─── git plumbing ────────────────────────────────────────────────────────────


def git(*args: str, cwd: Path | None = None) -> str:
    """Run git, return stdout stripped. Non-zero → CalledProcessError."""
    out = subprocess.run(
        ["git", *args],
        cwd=cwd or REPO_ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    return out.stdout.strip()


def git_try(*args: str, cwd: Path | None = None) -> str | None:
    """Like git() but returns None on non-zero instead of raising."""
    try:
        return git(*args, cwd=cwd)
    except subprocess.CalledProcessError:
        return None


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


def list_worktrees() -> list[Worktree]:
    """Parse `git worktree list --porcelain`."""
    out = git("worktree", "list", "--porcelain")
    wts = []
    cur: dict = {}
    for line in out.splitlines():
        if not line:
            if cur:
                wts.append(cur)
                cur = {}
            continue
        key, _, val = line.partition(" ")
        cur[key] = val
    if cur:
        wts.append(cur)
    return [
        Worktree(
            path=Path(w["worktree"]),
            branch=w.get("branch", "").removeprefix("refs/heads/") or None,
            head=w["HEAD"],
        )
        for w in wts
    ]


def plan_worktrees() -> list[Worktree]:
    """Worktrees on a `pNNN` branch, excluding main."""
    return [w for w in list_worktrees() if w.plan_num is not None]


def diff_src_files(wt: Worktree) -> list[str]:
    """Files matching rio-*/src/*.rs changed on this worktree vs main."""
    out = git_try("diff", "--name-only", "main..HEAD", cwd=wt.path) or ""
    return sorted(f for f in out.splitlines() if re.match(r"^rio-[a-z-]+/src/.*\.rs$", f))


# ─── plan doc parsing ────────────────────────────────────────────────────────


def find_plan_doc(plan_num: int) -> Path | None:
    """Locate plan-NNNN-*.md. 4-digit zero-pad is standardized — one glob."""
    hits = list(REPO_ROOT.glob(PLAN_DOC_GLOB.format(n=plan_num)))
    return hits[0] if hits else None


def plan_doc_src_files(doc: Path) -> list[str]:
    """Grep whole doc for rio-*/src/*.rs paths. Best-effort — many docs
    use bare names in tree-style ## Files sections; T-item prose tends to
    have full paths."""
    pat = re.compile(r"rio-[a-z-]+/src/[a-zA-Z0-9_/.-]+\.rs")
    return sorted(set(pat.findall(doc.read_text())))


def plan_doc_t_count(doc: Path) -> int:
    """Count ### T<N> headers — batch plans have >1."""
    return len(re.findall(r"^### T\d", doc.read_text(), re.MULTILINE))


# Fenced block: ```json deps\n{...}\n```. New docs use this; old docs fall
# back to grepping the **Depends on:** prose line. Fence wins if both exist.
_DEPS_FENCE_RE = re.compile(r"```json deps\n(.*?)\n```", re.DOTALL)
# Fenced block: ```json files\n[...]\n```. Structured file declarations —
# each entry is a state.PlanFile dict. New docs use this; old docs return
# None and callers fall back to plan_doc_src_files() grep.
_FILES_FENCE_RE = re.compile(r"```json files\n(.*?)\n```", re.DOTALL)
# Prose fallback: capture the paragraph starting **Depends on:** up to the
# next blank line, extract P<int> tokens from it.
_DEPS_PROSE_RE = re.compile(
    r"^\*\*Depends on:\*\*(.*?)(?:\n\s*\n|\n\*\*|\Z)", re.DOTALL | re.MULTILINE
)
_P_TOKEN_RE = re.compile(r"\bP(\d+)\b")


def plan_doc_deps(doc: Path) -> dict:
    """Read dependency spec. Prefers ```json deps fence; falls back to
    grepping **Depends on:** prose. Returns {"deps": [int,...],
    "soft_deps": [...], "note": str, "source": "fence"|"prose"|"none"}."""
    text = doc.read_text()
    m = _DEPS_FENCE_RE.search(text)
    if m:
        data = json.loads(m.group(1))
        data.setdefault("soft_deps", [])
        data.setdefault("note", "")
        data["source"] = "fence"
        return data
    # Fallback: grep prose. Same regex variants old tooling used.
    m = _DEPS_PROSE_RE.search(text)
    if m:
        deps = [int(n) for n in _P_TOKEN_RE.findall(m.group(1))]
        return {"deps": deps, "soft_deps": [], "note": "", "source": "prose"}
    return {"deps": [], "soft_deps": [], "note": "", "source": "none"}


def plan_doc_files(doc: Path) -> list[dict] | None:
    """Read fenced ```json files block. Returns None if absent (old-format
    doc — caller falls back to plan_doc_src_files() grep). When present,
    returns a list of dicts matching state.PlanFile shape (path/action/note).
    An empty list is explicit intent (doc touches no files), not a parse
    failure — distinct from None."""
    text = doc.read_text()
    m = _FILES_FENCE_RE.search(text)
    if m:
        return json.loads(m.group(1))
    return None


# ─── Tracey marker checks (rio-build: domain-indexed, NOT plan-indexed) ──────
#
# rio-build's tracey corpus lives in docs/src/components/*.md with markers like
# r[gw.opcode.wopQueryPathInfo] — domain.area.detail. Plan docs REFERENCE these
# in a ## Tracey section; they don't DEFINE new r[plan.*] markers.
#
# The QA check inverts rix's semantics:
#   - r[plan.*] presence is POLLUTION → FAIL (rix corpus leak)
#   - Zero domain-marker refs → WARN (refactor plans legitimately cite zero;
#     167/167 already covered; .#ci's tracey-validate catches dangling refs)
#
# Backfill plans (status=DONE at write time) skip both checks — they're
# archaeology, not forward plans, and the clusterer doesn't route through /plan.

_DOMAIN_MARKER_RE = re.compile(
    r"r\[(gw|sched|store|worker|ctrl|obs|sec|proto)\.[a-z][a-z0-9.-]+\]"
)
_PLAN_MARKER_RE = re.compile(r"r\[plan\.")


def qa_mechanical_check(doc: Path, dag_plans: set[int]) -> list[tuple[str, str]]:
    """Mechanical plan-doc precondition for /plan step 7a. Returns [(severity,
    issue)] — empty list means mechanical checks pass. Same pattern as
    atomicity_check.py: cheap skill-layer validation, don't spawn to abort.
    Judgment checks (criterion concreteness, prose sufficiency) are the
    rio-plan-reviewer agent's responsibility (step 7b, only if this passes).

    severity ∈ {FAIL, WARN}. Any FAIL → bounce to planner, don't spawn reviewer.

    Checks:
      - json files fence parses as list[PlanFile]  (FAIL on ValidationError)
      - json deps fence entries are int           (FAIL on non-int)
      - each dep exists in dag_plans              (FAIL on miss; 9-digit placeholders skipped)
      - NO r[plan.*] markers present              (FAIL — pollution; rio tracey is domain-indexed)
      - at least one r[domain.*] marker ref       (WARN — refactor plans may cite zero)
    """
    # Late import to avoid circular (state.py doesn't import _lib at module level).
    from state import PlanFile  # noqa: PLC0415

    issues: list[tuple[str, str]] = []
    text = doc.read_text()

    # Fence: json files
    files = plan_doc_files(doc)
    if files is None:
        issues.append(("WARN", "no ```json files fence (grep fallback)"))
    else:
        for i, f in enumerate(files):
            try:
                PlanFile.model_validate(f)
            except Exception as e:  # noqa: BLE001 — pydantic ValidationError or TypeError
                issues.append(("FAIL", f"json files[{i}]: {e}"))

    # Fence: json deps
    deps = plan_doc_deps(doc)
    if deps["source"] == "fence":
        for d in deps["deps"]:
            if not isinstance(d, int):
                issues.append(("FAIL", f"json deps entry {d!r} not int"))
            elif d >= 900_000_000:
                # 9-digit placeholder — sibling doc in same worktree; /merge-impl rewrites.
                pass
            elif d not in dag_plans:
                issues.append(("FAIL", f"dep P{d:04d} not in dag.jsonl"))
    elif deps["source"] == "none":
        issues.append(("WARN", "no deps declaration"))

    # Tracey markers — rio-build semantics (domain-indexed)
    #
    # r[plan.*] is corpus pollution — the rix model leaking in. rio-build plan
    # docs never define tracey markers; they reference domain markers that
    # live in docs/src/components/*.md. Any r[plan.*] occurrence in a plan
    # doc is a port bug.
    plan_markers = _PLAN_MARKER_RE.findall(text)
    if plan_markers:
        issues.append(
            (
                "FAIL",
                f"{len(plan_markers)} r[plan.*] marker(s) found — rio-build tracey is "
                "domain-indexed (r[gw.*], r[sched.*], etc.). Plan docs REFERENCE "
                "domain markers in ## Tracey, they don't DEFINE r[plan.*].",
            )
        )

    # Domain marker refs — WARN not FAIL. Refactor/tooling plans legitimately
    # cite zero. tracey-validate in .#ci catches dangling refs independently.
    domain_markers = _DOMAIN_MARKER_RE.findall(text)
    if not domain_markers:
        issues.append(
            (
                "WARN",
                "zero r[domain.*] marker refs — OK for refactor/tooling plans; "
                "feature plans should reference the spec markers they implement",
            )
        )

    return issues


# ─── CLI glue ────────────────────────────────────────────────────────────────


def emit(model: BaseModel, schema_flag: bool = False) -> None:
    """Print model as JSON, or its schema if schema_flag. Always to stdout."""
    if schema_flag:
        print(json.dumps(type(model).model_json_schema(), indent=2))
    else:
        print(model.model_dump_json(indent=2))


def cli_schema_or_run(model_cls: type[BaseModel], run_fn):
    """Standard CLI pattern: --schema prints the output schema, otherwise run."""
    if "--schema" in sys.argv:
        print(json.dumps(model_cls.model_json_schema(), indent=2))
        sys.exit(0)
    result = run_fn()
    print(result.model_dump_json(indent=2))
