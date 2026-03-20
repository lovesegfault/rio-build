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
from onibus.jsonl import append_jsonl, atomic_write_text, read_jsonl, remove_jsonl, write_jsonl
from onibus.models import (
    AgentRow,
    AgentStatus,
    CadenceReport,
    CadenceWindow,
    LockStatus,
    MergeQueueRow,
    MergeSha,
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
    """Cadence counter: mod 5 → consolidator, mod 7 → bughunter.

    Also records the integration-branch tip SHA at this merge-count to
    merge-shas.jsonl — _cadence_range() indexes by merge-count, not
    commit-count. Merges are fast-forward (no merge commit), so each plan
    lands as N first-parent commits; counting commits structurally misses
    multi-commit plans. At mc=14 this sprint: 7 plans since mc=7 spanned
    33 commits, but commit-indexed `git log -8` returned a 7-commit
    window — bughunter audited a rio-*/src diff of literally zero lines."""
    count_file = STATE_DIR / "merge-count.txt"
    sha_file = STATE_DIR / "merge-shas.jsonl"
    if set_to is not None:
        new = set_to
    else:
        cur = int(count_file.read_text().strip()) if count_file.exists() else 0
        new = cur + 1
    count_file.parent.mkdir(parents=True, exist_ok=True)
    count_file.write_text(f"{new}\n")
    # Record tip at THIS merge-count. Append-only; _cadence_range reads the
    # last row with mc == (current - window). git failure (no integration
    # branch in this cwd) is silent — count still bumps, sha just not recorded.
    tip = subprocess.run(
        ["git", "rev-parse", INTEGRATION_BRANCH],
        capture_output=True, text=True,
    ).stdout.strip()
    if tip:
        append_jsonl(sha_file, MergeSha(
            mc=new, sha=tip, ts=datetime.now(timezone.utc),
        ))
    return new


def _cadence_range(window: int) -> str | None:
    """git range for the last `window` PLAN MERGES (not commits).

    Reads merge-shas.jsonl: start = SHA recorded at (current_mc - window),
    end = SHA at current_mc. Both pinned. No commit-counting — count_bump()
    records the tip after each ff-merge, so the map indexes by merge, and
    each range covers however many commits that plan contained.

    `A..B` excludes A — correct here: by_mc[start_mc] is the tip AFTER merge
    start_mc landed, which is the boundary we want (audit everything merged in
    (start_mc, current_mc]).

    Returns None until `window` merges accumulate post-P0306 (merge-shas.jsonl
    has no rows for pre-fix history). One cadence cycle lost; simpler than
    backfill. merge-shas.jsonl is append-only in state/ (gitignored) — survives
    across sessions, not across fresh clones."""
    sha_file = STATE_DIR / "merge-shas.jsonl"
    count_file = STATE_DIR / "merge-count.txt"
    if not sha_file.exists() or not count_file.exists():
        return None
    current_mc = int(count_file.read_text().strip())
    start_mc = current_mc - window
    if start_mc < 0:
        return None
    # Last row per mc wins (handles set_to re-writes — e.g., `count-bump --set-to N`
    # after a reset can re-record the same mc with a different tip).
    by_mc: dict[int, str] = {r.mc: r.sha for r in read_jsonl(sha_file, MergeSha)}
    if start_mc not in by_mc or current_mc not in by_mc:
        return None  # gap in the map (pre-P0306 history, or start_mc=0 never recorded)
    return f"{by_mc[start_mc]}..{by_mc[current_mc]}"


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
# T-placeholder: T9<runid><NN> — same 9-digit scheme as P-placeholders but
# with a T prefix to disambiguate. Per-batch-doc sequences (not global) so
# P0304's T959435401 and P0311's T959435401 are distinct placeholders.
_T_PLACEHOLDER_RE = re.compile(r"\bT(9\d{8})\b")
# Real T-header: ### T<N> — where N is ≤4 digits. Cap excludes placeholders.
_T_HEADER_RE = re.compile(r"^### T(\d{1,4}) —", re.M)


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


def _find_t_placeholders(worktree: Path, tgt: str) -> dict[str, list[str]]:
    """Scan batch-target docs this branch touched; return rel_path → ordered
    unique T-placeholder tokens (the 9-digit part, sans T prefix).

    Uses three-dot diff (TGT...HEAD) to find touched files — pre-ff, so this
    sees the writer's committed appends. NOT the P0325 concern: that was the
    P-rewrite's `touched` set going empty post-ff (because three-dot diff is
    empty when docs-tip == TGT-tip). T-scan runs BEFORE ff, and filters to
    batch-target docs (4-digit plan-0NNN- prefix) which are distinct from
    P-placeholder filenames (9-digit), so the two scans are disjoint by
    construction."""
    touched = git("diff", "--name-only", f"{tgt}...HEAD", cwd=worktree).splitlines()
    result: dict[str, list[str]] = {}
    for rel in touched:
        # Batch-target docs only: real 4-digit plan numbers (P0304/P0311/P0295).
        # P-placeholder filenames (plan-9ddddddNN-*.md) won't match this.
        if not re.match(r"\.claude/work/plan-0\d{3}-", rel):
            continue
        p = worktree / rel
        if not p.exists():
            continue
        text = p.read_text()
        phs = _T_PLACEHOLDER_RE.findall(text)
        if phs:
            # Dedup preserving insertion order — same placeholder referenced
            # in header + body cross-ref should map to one assignment.
            result[rel] = list(dict.fromkeys(phs))
    return result


def _max_existing_t(worktree: Path, rel_path: str, tgt: str) -> int:
    """Highest ### T<N> — header in the TGT-branch version of the doc.

    Reads TGT (not working tree) so a rebased branch's own placeholder
    headers (T9ddddddNN) don't pollute the max. Also defended by the
    \\d{1,4} cap in _T_HEADER_RE."""
    try:
        tgt_text = git("show", f"{tgt}:{rel_path}", cwd=worktree)
    except subprocess.CalledProcessError:
        # Batch doc doesn't exist on TGT — new in this branch. Start at T1.
        return 0
    matches = _T_HEADER_RE.findall(tgt_text)
    return max((int(m) for m in matches), default=0)


def _rewrite_t_placeholders(worktree: Path, tgt: str) -> dict[str, dict[str, int]]:
    """For each touched batch doc, assign real T-numbers to placeholders and
    rewrite in-place. Returns rel_path → {placeholder: assigned_t}.

    Per-doc sequence: each doc's assignments start from that doc's own
    max-existing-T on TGT, not a global counter."""
    found = _find_t_placeholders(worktree, tgt)
    result: dict[str, dict[str, int]] = {}
    for rel, phs in found.items():
        start = _max_existing_t(worktree, rel, tgt) + 1
        mapping = {ph: start + i for i, ph in enumerate(phs)}
        p = worktree / rel
        text = p.read_text()
        new = text
        for ph, assigned in mapping.items():
            # Replace T<placeholder> → T<assigned> everywhere in this doc
            # (headers, cross-refs, prose). No padding — T163 not T0163.
            new = new.replace(f"T{ph}", f"T{assigned}")
        if new != text:
            atomic_write_text(p, new)
        result[rel] = mapping
    return result


def _rewrite_and_rename(worktree: Path, mapping: list[Rename]) -> None:
    # Rewrite set derived from mapping, not git diff. Three-dot diff
    # (INTEGRATION_BRANCH...HEAD) is empty when the docs branch has
    # already been ff-merged — docs-tip == sprint-1-tip → no commits
    # unique to HEAD → rewrite loop silently skipped every .md file
    # while git-mv (below, mapping-driven) still ran. Manifested
    # 2× (docs-926870 @ 55f1d050, docs-928654 @ 8cb27862) before
    # bughunter-mc35 scratch-repo'd it.
    #
    # The mapping already has everything needed: placeholder + slug →
    # filename. The plan .md file is where the placeholder lives in
    # prose (P924999901, [P924999901](plan-924999901-...)). dag.jsonl
    # carries it as {"plan": 924999901, ...}. Rewrite both; no other
    # file type carries placeholders (by construction — the planner
    # only writes to .claude/work/plan-*.md and dag.jsonl).
    touched: list[str] = [
        f".claude/work/plan-{r.placeholder}-{r.slug}.md" for r in mapping
    ]
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

    # Fail loud if this branch has already been ff-merged. T1 makes the
    # rewrite logic call-order-robust anyway, but this catches a caller
    # who's about to do something confused: if HEAD is already an ancestor
    # of INTEGRATION_BRANCH, there's nothing left to merge AFTER rename.
    # Either the caller already merged (wrong order — rename commits a
    # NEW commit on top, diverging the branch) or they're calling rename
    # on a branch that was never a docs branch.
    rc = subprocess.run(
        ["git", "merge-base", "--is-ancestor", "HEAD", INTEGRATION_BRANCH],
        cwd=worktree,
    ).returncode
    if rc == 0:
        raise SystemExit(
            f"rename-unassigned: {branch!r} HEAD is already an ancestor of "
            f"{INTEGRATION_BRANCH!r}. If you ff-merged before renaming, the "
            f"rename commit will DIVERGE the branch. Rename BEFORE merge, "
            f"or skip rename (placeholders are already live on "
            f"{INTEGRATION_BRANCH} — fix them there manually)."
        )

    placeholders = _find_placeholders(worktree)
    mapping: list[Rename] = []
    if placeholders:
        start = _next_real()
        mapping = [
            Rename(placeholder=ph, assigned=start + i, slug=slug)
            for i, (ph, slug) in enumerate(placeholders)
        ]
        _rewrite_and_rename(worktree, mapping)

    # T-placeholder rewrite — separate pass over batch-target docs. Runs
    # after P-rewrite but before commit so both land atomically. Scan is
    # pre-ff three-dot diff (see _find_t_placeholders for P0325 distinction).
    t_map = _rewrite_t_placeholders(worktree, INTEGRATION_BRANCH)

    if not mapping and not t_map:
        return RenameReport(branch=branch, mapping=[], commit=None)

    git("add", "-u", cwd=worktree)
    parts = []
    if mapping:
        lo, hi = mapping[0].assigned, mapping[-1].assigned
        label = f"P{lo:04d}" if lo == hi else f"P{lo:04d}-P{hi:04d}"
        parts.append(f"plan numbers {label}")
    if t_map:
        n_t = sum(len(m) for m in t_map.values())
        parts.append(f"{n_t} T-number{'s' if n_t != 1 else ''} in {len(t_map)} batch doc{'s' if len(t_map) != 1 else ''}")
    git("commit", "-m", f"docs: assign {' + '.join(parts)}", cwd=worktree)
    return RenameReport(
        branch=branch, mapping=mapping,
        commit=git("rev-parse", "--short", "HEAD", cwd=worktree),
    )
