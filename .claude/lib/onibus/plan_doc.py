"""Plan-doc parsing — fenced blocks, dep extraction, mechanical QA."""

from __future__ import annotations

import json
import re
from pathlib import Path

from onibus import INTEGRATION_BRANCH, PLAN_DOC_GLOB, REPO_ROOT
from onibus.git_ops import git_try
from onibus.models import PlanFile, TraceyCoverage, TraceyMarkerHit
from onibus.tracey import TRACEY_MARKER_RE, tracey_markers


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


# `r[impl gw.foo.bar]` / `r[verify gw.foo.bar]` — annotation form in code.
# Comment syntax (`//`, `#`) is ignored: diff lines start with `+` anyway.
_IMPL_ANNOT_RE = re.compile(r"r\[(impl|verify) ([a-z][a-z0-9.-]+)\]")


def tracey_coverage(branch: str, plan_doc: Path, *, worktree: Path | None = None) -> TraceyCoverage:
    """Validator step 4's PASS/PARTIAL gate. Set-subtract plan's claimed markers
    against r[impl]/r[verify] annotations in the branch diff. The sole gate for
    'plan claimed coverage, impl forgot marker' — tracey-validate catches
    dangling refs (marker→spec), not missing annotations (spec→marker)."""
    claimed = tracey_markers(plan_doc)

    # Walk diff added-lines with file+line tracking. -U0 keeps hunks tight.
    diff = git_try(
        "diff", "-U0", f"{INTEGRATION_BRANCH}..{branch}",
        cwd=worktree or REPO_ROOT,
    ) or ""

    annot: dict[str, dict[str, str]] = {}  # id → {kind: "file:line"}
    cur_file, cur_line = "", 0
    for ln in diff.splitlines():
        if ln.startswith("+++ b/"):
            cur_file = ln[6:]
        elif ln.startswith("@@"):
            # @@ -a,b +c,d @@ → c is new-side start line
            m = re.search(r"\+(\d+)", ln)
            cur_line = int(m.group(1)) if m else 0
        elif ln.startswith("+") and not ln.startswith("+++"):
            for kind, mid in _IMPL_ANNOT_RE.findall(ln):
                annot.setdefault(mid, {})[kind] = f"{cur_file}:{cur_line}"
            cur_line += 1

    hits: list[TraceyMarkerHit] = []
    unmatched: list[str] = []
    for mid in claimed:
        a = annot.get(mid, {})
        hits.append(TraceyMarkerHit(id=mid, impl_loc=a.get("impl"), verify_loc=a.get("verify")))
        if "impl" not in a and "verify" not in a:
            unmatched.append(mid)

    return TraceyCoverage(
        markers=hits, unmatched=unmatched,
        covered=len(claimed) - len(unmatched), total=len(claimed),
    )


# Fenced block: ```json deps\n{...}\n```. New docs use this; old docs fall
# back to grepping the **Depends on:** prose line. Fence wins if both exist.
_DEPS_FENCE_RE = re.compile(r"```json deps\n(.*?)\n```", re.DOTALL)
# Fenced block: ```json files\n[...]\n```. Structured file declarations —
# each entry is a PlanFile dict. New docs use this; old docs return
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
    returns a list of dicts matching PlanFile shape (path/action/note).
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

_PLAN_MARKER_RE = re.compile(r"r\[plan\.")


def qa_mechanical_check(doc: Path, dag_plans: set[int]) -> list[tuple[str, str]]:
    """Mechanical plan-doc precondition for /plan step 7a. Returns [(severity,
    issue)] — empty list means mechanical checks pass. Same pattern as
    atomicity_check: cheap skill-layer validation, don't spawn to abort.
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
    # r[plan.*] is corpus pollution — the old model leaking in. rio-build plan
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
    domain_markers = TRACEY_MARKER_RE.findall(text)
    if not domain_markers:
        issues.append(
            (
                "WARN",
                "zero r[domain.*] marker refs — OK for refactor/tooling plans; "
                "feature plans should reference the spec markers they implement",
            )
        )

    return issues
