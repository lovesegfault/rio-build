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

from onibus import DAG_JSONL, INTEGRATION_BRANCH, REPO_ROOT, STATE_DIR, WORK_DIR
from onibus.jsonl import append_jsonl, atomic_write_text, read_jsonl, remove_jsonl, write_jsonl
from onibus.models import (
    AgentRow,
    AgentStatus,
    CadenceReport,
    CadenceWindow,
    LockStatus,
    MergeQueueRow,
    MergeSha,
    canonical_plan_id,
)

# Module-local alias — tests monkeypatch this (rename_unassigned scanned main's
# docs dir regardless of which worktree invoked it; tests need to redirect that).
DOCS_DIR = WORK_DIR

# Single source of truth. Was bash arithmetic in merger.md + dag-run/SKILL.md.
_CADENCE = {"consolidator": 5, "bughunter": 7}
from onibus.git_ops import git, git_try
from onibus.models import (
    AtomicityVerdict,
    ChoreViolation,
    DagFlipResult,
    Rename,
    RenameReport,
)
from onibus.plan_doc import find_plan_doc, plan_doc_t_count


# ─── lock ────────────────────────────────────────────────────────────────────

_LOCK_FILE = STATE_DIR / "merger.lock"

# Staleness threshold. The merge itself is ~10min typically (rebase + ff
# + .#ci cache-hit re-validate); .#coverage-full is backgrounded so
# doesn't count against lease. 30min was the old value — P0216 under
# TCG (cold-cache, every VM drv rebuilt on broken builders) ran ~30min
# end-to-end. 45min gives headroom. Failure mode is safe (stale=True →
# coordinator pings human, not auto-steal) but spurious pings waste time.
# PID-liveness was the wrong mechanism: the `onibus merge lock`
# subprocess exits immediately after writing (fire-and-forget CLI), so
# os.kill(pid, 0) was always ProcessLookupError → stale=True → POISONED
# on every merge.
_LEASE_SECS = 45 * 60


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
            cwd=REPO_ROOT,
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
            cwd=REPO_ROOT,
            capture_output=True, text=True,
        ).stdout.strip()
        ff_landed = current != content.get("main_at_acquire")
    return LockStatus(held=True, stale=stale, content=content, ff_landed=ff_landed)


def count_bump(set_to: int | None = None, *, plan: int | None = None) -> int:
    """Cadence counter: mod 5 → consolidator, mod 7 → bughunter.

    Also records the integration-branch tip SHA at this merge-count to
    merge-shas.jsonl — _cadence_range() indexes by merge-count, not
    commit-count. Merges are fast-forward (no merge commit), so each plan
    lands as N first-parent commits; counting commits structurally misses
    multi-commit plans. At mc=14 this sprint: 7 plans since mc=7 spanned
    33 commits, but commit-indexed `git log -8` returned a 7-commit
    window — bughunter audited a rio-*/src diff of literally zero lines.

    `plan` kwarg (P0417): if provided, recorded in the MergeSha row
    alongside mc+sha+ts. dag_flip passes this so the already-done path
    can check "did a prior dag_flip for this plan already bump?"
    (case-(b) re-invocation → skip; case-(a) coord-fast-path → bump).
    `--set-to` manual rewinds leave plan=None (no single plan owns a
    rewind)."""
    count_file = STATE_DIR / "merge-count.txt"
    sha_file = STATE_DIR / "merge-shas.jsonl"
    if set_to is not None:
        new = set_to
    else:
        cur = int(count_file.read_text().strip()) if count_file.exists() else 0
        new = cur + 1
    count_file.parent.mkdir(parents=True, exist_ok=True)
    # Record tip at THIS merge-count FIRST — P0417's already-done scan
    # checks merge-shas.jsonl for plan==plan_num before bumping. If
    # count-file is written first and we crash before MergeSha, the scan
    # sees no row → case-(a) → double-bump. Writing MergeSha first makes
    # the crash window a cadence-gap (cadence agents skipped one tick)
    # not a permanent off-by-one. _cadence_range dedupes by
    # last-row-per-mc (dict comprehension), so duplicate-mc rows are
    # harmless. tip=="" (bare rev-parse fail outside repo) → skip
    # MergeSha but still bump count — same cadence-gap degradation.
    # (P0420 swapped the order from count-file-first.)
    #
    # Explicit cwd — if the merger's bash-cwd is outside the repo
    # entirely (e.g., a removed-worktree directory), a bare rev-parse
    # fails → tip="" → mc→sha map gaps → _cadence_range returns None →
    # cadence agents silently not spawned.
    tip = subprocess.run(
        ["git", "rev-parse", INTEGRATION_BRANCH],
        cwd=REPO_ROOT,
        capture_output=True, text=True,
    ).stdout.strip()
    if tip:
        append_jsonl(sha_file, MergeSha(
            mc=new, sha=tip, ts=datetime.now(timezone.utc), plan=plan,
        ))
    count_file.write_text(f"{new}\n")
    return new


def dag_flip(plan_num: int) -> DagFlipResult:
    """Step 7.5 compound: set-status DONE + amend into tip + count-bump.

    Explicit cwd=REPO_ROOT on EVERY git invocation — the merger agent's
    bash-cwd is not reliable (step 7 worktree-remove may leave it in a
    deleted directory, or it may be a plan worktree where HEAD ≠ $TGT).
    P0401's merger amended to d1449fad but sprint-1 stayed at 4fc05cfe —
    the amend ran in a context where the branch-ref didn't follow HEAD.

    Verifies the integration branch is checked out in REPO_ROOT before
    amending — fail loud if not (merger should ALWAYS have $TGT checked
    out in main; if not, something upstream went wrong).

    Also folds in queue-consume (state-file edit, cwd-agnostic but one
    less moving part for the merger) — the merger.md step 7.5 block had
    7 bash commands; this collapses them to one CLI call."""
    from onibus.dag import Dag, set_status

    # Sanity: REPO_ROOT has the integration branch checked out. The amend
    # below moves whatever branch HEAD is — MUST be sprint-N.
    cur_branch = git("rev-parse", "--abbrev-ref", "HEAD", cwd=REPO_ROOT)
    if cur_branch != INTEGRATION_BRANCH:
        raise SystemExit(
            f"dag-flip: {REPO_ROOT} has {cur_branch!r} checked out, "
            f"expected {INTEGRATION_BRANCH!r}. The amend would move the "
            f"wrong branch. Check step-4 ff-try and step-7 cleanup ordering."
        )

    # Compute unblocked BEFORE the flip — Dag.unblocked_by is a
    # hypothetical ("if N→DONE, what enters frontier?"). After set_status
    # writes DONE, the would-be-unblocked are already IN the frontier so
    # the diff reads empty.
    unblocked = Dag.load().unblocked_by(plan_num)
    set_status(plan_num, "DONE")
    consumed = queue_consume(canonical_plan_id(plan_num))

    dag_rel = str(DAG_JSONL.relative_to(REPO_ROOT))
    git("add", dag_rel, cwd=REPO_ROOT)
    # Verify stage is non-empty. If dag.jsonl was already DONE for this
    # plan, set_status wrote an identical tree → nothing staged → amend
    # would be a no-op rewrite (harmless but surprising; coordinator
    # fast-path or re-invoked merger).
    staged = git_try("diff", "--cached", "--name-only", cwd=REPO_ROOT) or ""
    if not staged.strip():
        # already-done: dag row was pre-flipped. Two cases:
        #   (a) coord-fast-path — ff + set_status DONE directly, never ran
        #       dag_flip → merge-shas.jsonl has no row for this plan →
        #       bump (merge happened, mc never incremented).
        #   (b) merger crashed AFTER count_bump → re-invoked → row exists
        #       → skip (bumping again = mc off-by-one forever; the exact
        #       P0221-class double-bump P0414 was meant to prevent).
        # Distinguish by scanning merge-shas.jsonl for plan==plan_num.
        sha_file = STATE_DIR / "merge-shas.jsonl"
        prior_mc: int | None = None
        if sha_file.exists():
            for row in read_jsonl(sha_file, MergeSha):
                if row.plan == plan_num:
                    prior_mc = row.mc
                    break
        if prior_mc is not None:
            # case (b): already bumped at prior run
            return DagFlipResult(
                plan=plan_num, amend_sha="already-done",
                mc=prior_mc, unblocked=unblocked, queue_consumed=consumed,
            )
        # case (a): never bumped — bump now
        return DagFlipResult(
            plan=plan_num, amend_sha="already-done",
            mc=count_bump(plan=plan_num), unblocked=unblocked,
            queue_consumed=consumed,
        )

    git("commit", "--amend", "--no-edit", cwd=REPO_ROOT)
    amend_sha = git("rev-parse", "--short", "HEAD", cwd=REPO_ROOT)

    # Post-amend sanity: the integration branch ref moved with HEAD.
    # `git commit --amend` with a branch checked out DOES move the branch;
    # this check proves we weren't in detached-HEAD (which would leave
    # the branch behind — the P0401 failure mode).
    tgt_sha = git("rev-parse", INTEGRATION_BRANCH, cwd=REPO_ROOT)
    head_sha = git("rev-parse", "HEAD", cwd=REPO_ROOT)
    ref_forced = False
    if tgt_sha != head_sha:
        # Belt-and-suspenders: force-update the ref. The cur_branch
        # precondition above should make this unreachable. If it fires,
        # something about git's amend semantics under this worktree
        # layout is surprising — the update-ref recovers regardless.
        print(
            f"dag_flip: UNEXPECTED — {INTEGRATION_BRANCH!r} at {tgt_sha[:8]} "
            f"but HEAD at {head_sha[:8]} post-amend. Force-updating ref. "
            f"The cur_branch precondition should prevent this; investigate "
            f"worktree layout.",
            file=sys.stderr,
        )
        git("update-ref", f"refs/heads/{INTEGRATION_BRANCH}", head_sha,
            cwd=REPO_ROOT)
        ref_forced = True

    # count-bump MUST run AFTER amend — it records rev-parse
    # INTEGRATION_BRANCH in merge-shas.jsonl. Pre-amend that SHA is
    # orphaned (reflog-only). This ordering was the P0319 fix; dag_flip
    # keeps it correct by construction. P0417 passes plan so the
    # already-done re-invocation check can find this row. P0420
    # reordered count_bump internally (MergeSha row BEFORE count-file)
    # so crash-between-writes degrades to a cadence-gap not double-bump.
    mc = count_bump(plan=plan_num)
    return DagFlipResult(
        plan=plan_num, amend_sha=amend_sha, mc=mc,
        unblocked=unblocked, queue_consumed=consumed,
        ref_forced=ref_forced,
    )


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
    plan_c = canonical_plan_id(plan)
    return remove_jsonl(STATE_DIR / "merge-queue.jsonl", MergeQueueRow, lambda r: r.plan == plan_c)


# ─── agent-row convenience ───────────────────────────────────────────────────


def agent_start(role: str, plan: str, agent_id: str | None = None, note: str = "") -> AgentRow:
    """Typed launch-row creation. Worktree path derived from plan number.
    Replaces hand-typed JSON in dag-run/dag-tick/implement — a typo there
    is silent corruption until reconcile runs."""
    m = re.fullmatch(r"P?(\d+)", plan)
    worktree = f"/root/src/rio-build/p{int(m.group(1))}" if m else None
    row = AgentRow(
        # The AgentRow field_validator normalizes on construct anyway, but
        # explicit canonicalization at the call site is clearer.
        plan=canonical_plan_id(plan),
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
    # r.plan is already normalized by the field_validator on load; this
    # normalizes the CALLER's arg so "414"/"P414"/"P0414" all match.
    plan_c = canonical_plan_id(plan)
    n = 0
    for r in rows:
        if r.plan == plan_c and r.role == role:
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
# Regex is deliberately permissive (9-11 digits): the CANONICAL form is
# 9-digit per rio-planner.md Step-1, but docs-993168 + docs-654701
# emitted 11-digit T<P-placeholder><seq> tokens. Writer bugs shouldn't
# strand tokens — catch them defensively here.
_T_PLACEHOLDER_RE = re.compile(r"\bT(9\d{8,10})\b")
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


def _touched_batch_docs(worktree: Path, tgt: str) -> list[str]:
    """Rel paths of batch-target docs (real 4-digit plan-0NNN-*.md) that
    this branch modified relative to TGT.

    Uses three-dot diff (TGT...HEAD) — pre-ff, so this sees the writer's
    committed appends. NOT the P0325 concern: that was the P-rewrite's
    `touched` set going empty post-ff (because three-dot diff is empty when
    docs-tip == TGT-tip). This scan runs BEFORE ff, and filters to 4-digit
    plan-0NNN- prefix which is disjoint from P-placeholder filenames
    (9-digit plan-9ddddddNN-) by construction.

    Shared by both the T-placeholder rewrite (P0401-T1) and the
    P-placeholder cross-ref rewrite in batch docs (P0304-T30: writer
    appends "see [P994957501](plan-994957501-foo.md)" to P0304's Tasks;
    the P-rewrite pass at _rewrite_and_rename only iterates the new
    plan-9ddddddNN-*.md files + dag.jsonl, so the batch-doc cross-ref
    stays stale). 7 of 12 recent docs-merges needed coordinator
    manual-sed for this."""
    touched = git("diff", "--name-only", f"{tgt}...HEAD", cwd=worktree).splitlines()
    return [
        rel for rel in touched
        # \d{4} (not 0\d{3}): plan-1NNN+ headroom. Still disjoint from
        # 9-digit placeholder filenames by length.
        if re.match(r"\.claude/work/plan-\d{4}-", rel)
        and (worktree / rel).exists()
    ]


def _find_t_placeholders(worktree: Path, batch_docs: list[str]) -> dict[str, list[str]]:
    """rel_path → ordered unique T-placeholder tokens (the 9-digit part)."""
    result: dict[str, list[str]] = {}
    for rel in batch_docs:
        text = (worktree / rel).read_text()
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


def _rewrite_t_placeholders(
    worktree: Path, tgt: str, batch_docs: list[str],
    placeholder_docs: list[str],
) -> dict[str, dict[str, int]]:
    """For each touched batch doc, assign real T-numbers to placeholders and
    rewrite in-place. Returns rel_path → {placeholder: assigned_t}.

    Per-doc sequence: each doc's assignments start from that doc's own
    max-existing-T on TGT, not a global counter.

    placeholder_docs (P0418-T4): plan-9ddddddNN-*.md from the same
    writer run. These are scanned for T-tokens and rewritten USING the
    batch_docs' mappings (a T-ref in a placeholder doc points at a
    batch-doc task, so the assignment is whatever that batch doc got).
    Without this, a "see P0304-T912345601" cross-ref in a new standalone
    plan doc would survive as a dead pointer."""
    found = _find_t_placeholders(worktree, batch_docs)
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

    # Second pass: rewrite T-refs inside placeholder docs using the
    # assignments just computed. One T-token can only point at one
    # batch doc (writer emits T<runid><NN> where <runid><NN> is
    # unique within the writer run), so union the mappings.
    all_t: dict[str, int] = {}
    for m in result.values():
        all_t.update(m)
    for rel in placeholder_docs:
        p = worktree / rel
        try:
            text = p.read_text()
        except FileNotFoundError:
            continue
        new = text
        for ph, assigned in all_t.items():
            new = new.replace(f"T{ph}", f"T{assigned}")
        if new != text:
            atomic_write_text(p, new)
    return result


def _rewrite_and_rename(
    worktree: Path, mapping: list[Rename], batch_docs: list[str]
) -> None:
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
    # carries it as {"plan": 924999901, ...}. Rewrite both.
    #
    # batch_docs (P0304-T30 fix): batch-target docs (P0304/P0295/P0311)
    # CROSS-REFERENCE P-placeholders when the writer appends a task that
    # links to a same-run standalone plan — "see [P994957501](...)". The
    # mapping-only iteration above misses these. 7/12 recent docs-merges
    # required coordinator manual-sed before this fix.
    touched: list[str] = [
        f".claude/work/plan-{r.placeholder}-{r.slug}.md" for r in mapping
    ]
    touched.append(".claude/dag.jsonl")
    touched.extend(batch_docs)

    # Defensive glob pass for batch-append targets: the writer may have
    # written a cross-ref to a plan doc it ALSO modified, but a glob scan
    # catches cases where a stale placeholder slipped into a doc outside
    # the three-dot diff (e.g., manual coordinator edit). O(files × phs)
    # — ~300 files × ≤5 placeholders, negligible.
    placeholders = [r.placeholder for r in mapping]
    for p in (worktree / ".claude/work").glob("plan-*.md"):
        rel = str(p.relative_to(worktree))
        if rel in touched:
            continue  # already covered
        try:
            text = p.read_text()
        except FileNotFoundError:
            continue
        if any(ph in text for ph in placeholders):
            touched.append(rel)

    for rel in touched:
        p = worktree / rel
        try:
            text = p.read_text()
        except FileNotFoundError:
            continue
        new = text
        is_jsonl = rel.endswith(".jsonl")
        for r in mapping:
            padded = f"{r.assigned:04d}"
            bare = str(r.assigned)
            if is_jsonl:
                # dag.jsonl: bare integer, no padding.
                new = new.replace(r.placeholder, bare)
            else:
                # .md: anchored replaces. P-prefix and plan-prefix protect
                # against T-substring collision (gap B: pre-P0418 bare
                # replace would corrupt T959435401 → T0305 in placeholder
                # docs, which the T-rewrite pass doesn't cover). The
                # JSON-context regex handles the deps-fence bare-int case
                # (gap C) — a zero-padded integer in `json deps` breaks
                # json.loads.
                new = new.replace(f"P{r.placeholder}", f"P{padded}")
                new = new.replace(f"plan-{r.placeholder}", f"plan-{padded}")
                # json-fence integer: immediately after `[`, `,`, `:`, or
                # whitespace with word-boundary after. Unpadded. This also
                # catches prose headers ("# Plan 924999901") — the token
                # becomes bare-int, not padded, but the exact form there
                # doesn't matter.
                new = re.sub(
                    rf"(?<=[\[,:\s]){re.escape(r.placeholder)}\b",
                    bare,
                    new,
                )
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

    # Enumerate touched batch-target docs once — shared by T-placeholder
    # rewrite (P0401-T1) and P-cross-ref rewrite (P0304-T30). Also
    # enumerate placeholder plan-docs up front so _rewrite_t_placeholders
    # can cross-scan them (P0418-T4).
    batch_docs = _touched_batch_docs(worktree, INTEGRATION_BRANCH)
    placeholders = _find_placeholders(worktree)
    placeholder_docs = [
        f".claude/work/plan-{ph}-{slug}.md" for ph, slug in placeholders
    ]

    # T-placeholder rewrite FIRST. Writer uses 9<runid><NN> for BOTH
    # P-placeholders and T-placeholders (per-doc sequences both start at
    # 01), so the same 9-digit token can appear as T959435401 AND
    # P959435401 in the same batch doc. Post-P0418 the P-rewrite below is
    # prefix-anchored (P-prefix / plan-prefix / json-context), so this
    # ordering is belt-and-suspenders — T-rewrite-first would be
    # load-bearing only if a bare-int placeholder in .md ever coincided
    # with a T-context, which anchoring excludes by construction. Scan is
    # pre-ff three-dot diff — see _touched_batch_docs for P0325 distinction.
    t_map = _rewrite_t_placeholders(
        worktree, INTEGRATION_BRANCH, batch_docs, placeholder_docs,
    )
    mapping: list[Rename] = []
    if placeholders:
        start = _next_real()
        mapping = [
            Rename(placeholder=ph, assigned=start + i, slug=slug)
            for i, (ph, slug) in enumerate(placeholders)
        ]
        _rewrite_and_rename(worktree, mapping, batch_docs)

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
