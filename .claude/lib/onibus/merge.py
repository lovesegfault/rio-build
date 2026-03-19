"""Merge machinery — lock, atomicity check, placeholder rename.

Lock is kernel-atomic open(O_CREAT|O_EXCL), not flock() — the CLI exits
immediately so an fd-held lock would release. Staleness is time-lease
(acquired_at age > _LEASE_SECS), not PID-liveness: the CLI subprocess
that writes the lock exits immediately, so its PID is always dead by the
time anyone checks. Coordinator failed serialize-mergers discipline three
times (P119/P0201, P0085/nbr-redesign, P0118/P0210). Prose constraint →
mechanism.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

from onibus import INTEGRATION_BRANCH, STATE_DIR, WORK_DIR
from onibus.jsonl import atomic_write_text, read_jsonl, remove_jsonl, write_jsonl
from onibus.models import (
    AgentRow,
    AgentStatus,
    CadenceReport,
    CadenceWindow,
    LockStatus,
    MergeQueueRow,
)

# Module-local alias — tests monkeypatch this (rename_unassigned scanned main's
# docs dir regardless of which worktree invoked it; tests need to redirect that).
DOCS_DIR = WORK_DIR

# Single source of truth. Was bash arithmetic in merger.md + dag-run/SKILL.md.
_CADENCE = {"consolidator": 5, "bughunter": 7}
from onibus.git_ops import git
from onibus.models import (
    AtomicityVerdict,
    ChoreViolation,
    Rename,
    RenameReport,
)
from onibus.plan_doc import find_plan_doc, plan_doc_t_count


# ─── lock ────────────────────────────────────────────────────────────────────

_LOCK_FILE = STATE_DIR / "merger.lock"

# Staleness threshold. The merge itself is ~10min (rebase + ff + .#ci cache-hit
# re-validate); .#coverage-full is backgrounded so doesn't count against lease.
# PID-liveness was the wrong mechanism: the `onibus merge lock` subprocess exits
# immediately after writing the file (fire-and-forget CLI), so os.kill(pid, 0)
# was always ProcessLookupError → stale=True → POISONED on every merge.
_LEASE_SECS = 30 * 60


def _lock_age_secs(content: dict) -> float:
    acquired = datetime.fromisoformat(content["acquired_at"])
    if acquired.tzinfo is None:
        # Tolerate pre-T2 lock files with naive timestamps.
        acquired = acquired.replace(tzinfo=timezone.utc)
    return (datetime.now(timezone.utc) - acquired).total_seconds()


def lock(plan: str, agent_id: str) -> None:
    """Exit 0 with lock JSON on stdout; exit 4 with holder JSON on stderr if held."""
    content = {
        "agent_id": agent_id,
        "plan": plan,
        "acquired_at": datetime.now(timezone.utc).isoformat(),
        "main_at_acquire": subprocess.run(
            ["git", "rev-parse", "--short", "HEAD"],
            capture_output=True, text=True,
        ).stdout.strip(),
    }
    try:
        _LOCK_FILE.parent.mkdir(parents=True, exist_ok=True)
        with _LOCK_FILE.open("x") as f:
            json.dump(content, f)
        print(json.dumps(content))
        sys.exit(0)
    except FileExistsError:
        existing = json.loads(_LOCK_FILE.read_text())
        age_s = _lock_age_secs(existing)
        stale = age_s > _LEASE_SECS
        print(
            json.dumps({"error": "lock-held", "holder": existing, "age_secs": age_s, "stale": stale}),
            file=sys.stderr,
        )
        sys.exit(4)


def unlock() -> None:
    _LOCK_FILE.unlink(missing_ok=True)


def lock_status() -> LockStatus:
    """stale = lock age > _LEASE_SECS → merger crashed mid-run (or hung).
    ff_landed = comparison of content.main_at_acquire vs current $TGT —
    was prose-duplicated in dag-tick:23, dag-run:47, merger:41."""
    if not _LOCK_FILE.exists():
        return LockStatus(held=False, stale=False, content=None)
    content = json.loads(_LOCK_FILE.read_text())
    age_s = _lock_age_secs(content)
    stale = age_s > _LEASE_SECS
    ff_landed = None
    if stale:
        current = subprocess.run(
            ["git", "rev-parse", "--short", INTEGRATION_BRANCH],
            capture_output=True, text=True,
        ).stdout.strip()
        ff_landed = current != content.get("main_at_acquire")
    return LockStatus(held=True, stale=stale, content=content, ff_landed=ff_landed)


def count_bump(set_to: int | None = None) -> int:
    """Cadence counter: mod 5 → consolidator, mod 7 → bughunter."""
    count_file = STATE_DIR / "merge-count.txt"
    if set_to is not None:
        new = set_to
    else:
        cur = int(count_file.read_text().strip()) if count_file.exists() else 0
        new = cur + 1
    count_file.parent.mkdir(parents=True, exist_ok=True)
    count_file.write_text(f"{new}\n")
    return new


def _cadence_range(window: int) -> str | None:
    """git range for the last W commits (NOT merges — see T5).
    (W+1)th-from-tip is commit-before-window; `A..B` excludes A so
    `out[-1]..out[0]` is exactly W commits.

    End pinned to out[0] (tip SHA at git-log time), not INTEGRATION_BRANCH —
    the live ref moves between cadence-computation and cadence-agent execution.
    At mc=7 this sprint end=sprint-1 had moved +12 commits by the time bughunter
    ran; window grew 7→19 mid-run, auditing unreviewed code from merge N+1."""
    out = subprocess.run(
        ["git", "log", "--first-parent", "--format=%H", f"-{window + 1}", INTEGRATION_BRANCH],
        capture_output=True, text=True,
    ).stdout.strip().splitlines()
    if len(out) < window + 1:
        return None  # not enough history yet
    return f"{out[-1]}..{out[0]}"


def cadence() -> CadenceReport:
    """Which cadence agents are due and what git range to pass them. Computed
    from merge-count.txt + git log. Merger calls this AFTER count-bump."""
    count_file = STATE_DIR / "merge-count.txt"
    count = int(count_file.read_text().strip()) if count_file.exists() else 0
    windows = {}
    for name, w in _CADENCE.items():
        due = count > 0 and count % w == 0
        windows[name] = CadenceWindow(due=due, range=_cadence_range(w) if due else None)
    return CadenceReport(count=count, consolidator=windows["consolidator"], bughunter=windows["bughunter"])


def queue_consume(plan: str) -> int:
    """Remove a plan's row(s) from merge-queue.jsonl. Called post-merge so the
    queue doesn't accumulate forever. Returns count removed."""
    return remove_jsonl(STATE_DIR / "merge-queue.jsonl", MergeQueueRow, lambda r: r.plan == plan)


# ─── agent-row convenience ───────────────────────────────────────────────────


def agent_start(role: str, plan: str, agent_id: str | None = None, note: str = "") -> AgentRow:
    """Typed launch-row creation. Worktree path derived from plan number.
    Replaces hand-typed JSON in dag-run/dag-tick/implement — a typo there
    is silent corruption until reconcile runs."""
    m = re.fullmatch(r"P?(\d+)", plan)
    worktree = f"/root/src/rio-build/p{int(m.group(1))}" if m else None
    row = AgentRow(
        plan=plan if plan.startswith("P") else f"P{plan}",
        role=role,  # type: ignore[arg-type]
        agent_id=agent_id,
        worktree=worktree,
        status="running",
        note=note,
    )
    from onibus.jsonl import append_jsonl  # avoid top-level circular risk
    append_jsonl(STATE_DIR / "agents-running.jsonl", row)
    return row


def agent_mark(plan: str, role: str, status: AgentStatus) -> int:
    """Lifecycle transition. Rewrites matching row(s) in-place. Returns count
    updated. Replaces hand-editing the row to status=consumed."""
    path = STATE_DIR / "agents-running.jsonl"
    rows = read_jsonl(path, AgentRow)
    n = 0
    for r in rows:
        if r.plan == plan and r.role == role:
            r.status = status  # type: ignore[assignment]
            n += 1
    write_jsonl(path, rows)
    return n


# ─── atomicity (lifted from atomicity_check.py) ──────────────────────────────


def _plan_num_from_branch(branch: str) -> int | None:
    if m := re.fullmatch(r"p(\d+)", branch):
        return int(m.group(1))
    return None


def _commit_src_files(sha: str) -> list[str]:
    out = git("show", "--name-only", "--format=", sha)
    return [f for f in out.splitlines() if re.match(r"^rio-[a-z-]+/src/.*\.rs$", f)]


def atomicity_check(branch: str) -> AtomicityVerdict:
    """Two gates: mega-commit (batch plan collapsed), chore-touches-src."""
    plan_num = _plan_num_from_branch(branch)
    t_count = 0
    if plan_num is not None:
        doc = find_plan_doc(plan_num)
        if doc:
            t_count = plan_doc_t_count(doc)

    log = git("log", "--format=%H %s", f"{INTEGRATION_BRANCH}..{branch}")
    commits = [line.split(" ", 1) for line in log.splitlines() if line]
    c_count = len(commits)

    chore_violations = []
    for sha, subj in commits:
        if not subj.startswith("chore"):
            continue
        src = _commit_src_files(sha)
        if src:
            chore_violations.append(
                ChoreViolation(sha=sha, subject=subj, src_files=src)
            )

    mega = t_count >= 3 and c_count == 1
    abort = (
        "chore-touches-src" if chore_violations else ("mega-commit" if mega else None)
    )
    return AtomicityVerdict(
        branch=branch, t_count=t_count, c_count=c_count,
        mega_commit=mega, chore_violations=chore_violations, abort_reason=abort,
    )


# ─── rename-unassigned (lifted from rename_unassigned.py) ────────────────────
# 9-digit placeholders → real plan numbers. Serial by construction (one merger).

_PLACEHOLDER_RE = re.compile(r"^plan-(9\d{8})-(.+)\.md$")
_REAL_RE = re.compile(r"^plan-(\d{1,4})-")


def _worktree_for(branch: str) -> Path:
    out = git("worktree", "list", "--porcelain")
    for block in out.split("\n\n"):
        lines = {k: v for k, _, v in (ln.partition(" ") for ln in block.splitlines())}
        if lines.get("branch") == f"refs/heads/{branch}":
            return Path(lines["worktree"])
    raise SystemExit(f"no worktree for branch {branch!r}")


def _find_placeholders(worktree: Path) -> list[tuple[str, str]]:
    docs = worktree / ".claude" / "work"
    found = []
    for name in sorted(git("ls-files", str(docs), cwd=worktree).splitlines()):
        m = _PLACEHOLDER_RE.match(name.rsplit("/", 1)[-1])
        if m:
            found.append((m.group(1), m.group(2)))
    return found


def _next_real() -> int:
    # Scan MAIN's docs — a docs branch forked before P0188 merged would
    # otherwise allocate 188 again from its stale view.
    nums = [
        int(m.group(1))
        for p in DOCS_DIR.glob("plan-*.md")
        if (m := _REAL_RE.match(p.name))
    ]
    return max(nums, default=0) + 1


def _rewrite_and_rename(worktree: Path, mapping: list[Rename]) -> None:
    touched = [
        f for f in git(
            "diff", "--name-only", f"{INTEGRATION_BRANCH}...HEAD", cwd=worktree
        ).splitlines()
        if f.endswith(".md")
    ]
    # dag.jsonl carries the placeholder as {"plan":924999901,...} — same literal
    # replace. Include even if diff missed it.
    if ".claude/dag.jsonl" not in touched:
        touched.append(".claude/dag.jsonl")

    for rel in touched:
        p = worktree / rel
        try:
            text = p.read_text()
        except FileNotFoundError:
            continue
        new = text
        is_jsonl = rel.endswith(".jsonl")
        for r in mapping:
            repl = str(r.assigned) if is_jsonl else f"{r.assigned:04d}"
            new = new.replace(r.placeholder, repl)
        if new != text:
            atomic_write_text(p, new)

    for r in mapping:
        git(
            "mv",
            f".claude/work/plan-{r.placeholder}-{r.slug}.md",
            f".claude/work/plan-{r.assigned:04d}-{r.slug}.md",
            cwd=worktree,
        )


def rename_unassigned(branch: str) -> RenameReport:
    worktree = _worktree_for(branch)
    placeholders = _find_placeholders(worktree)
    if not placeholders:
        return RenameReport(branch=branch, mapping=[], commit=None)

    start = _next_real()
    mapping = [
        Rename(placeholder=ph, assigned=start + i, slug=slug)
        for i, (ph, slug) in enumerate(placeholders)
    ]
    _rewrite_and_rename(worktree, mapping)
    git("add", "-u", cwd=worktree)
    lo, hi = mapping[0].assigned, mapping[-1].assigned
    label = f"P{lo:04d}" if lo == hi else f"P{lo:04d}-P{hi:04d}"
    git("commit", "-m", f"docs: assign plan numbers {label}", cwd=worktree)
    return RenameReport(
        branch=branch, mapping=mapping,
        commit=git("rev-parse", "--short", "HEAD", cwd=worktree),
    )
