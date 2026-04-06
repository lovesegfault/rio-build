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
    FastPathVerdict,
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
                    # no break — last-row-per-plan wins (post-rewind
                    # via --set-to may have multiple plan=N rows;
                    # latest is correct per _cadence_range's
                    # last-row-per-mc semantics)
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


def archive_agents() -> int:
    """Drop AgentRows whose worktree no longer exists. Returns rows dropped.

    agents-running.jsonl is append-mostly (agent_mark rewrites in place but
    never prunes). At mc~250 the file hit 966 rows — 50+ stale Pdocs- rows
    from mc=33-era, range-ID rows, consumed rows from long-gone worktrees.
    P0418's canonical_plan_id validator exposed them.

    The plan-doc sketch indexed by mc (drop consumed where mc < current-20),
    but AgentRow has no mc field. Simpler: a row whose worktree directory is
    gone is dead by definition — the agent that wrote it finished and the
    merger pruned the worktree. Rows with no worktree path (writer/qa agents
    use docs-<runid>, stored as plan not worktree) survive unless consumed.

    Run via `onibus state archive-agents` or wire into /dag-tick post-merge."""
    path = STATE_DIR / "agents-running.jsonl"
    rows = read_jsonl(path, AgentRow)

    def _keep(r: AgentRow) -> bool:
        if r.worktree:
            return Path(r.worktree).is_dir()
        # No worktree path (docs-<runid> writer/qa rows) — keep unless
        # consumed. Consumed-no-worktree is terminal state; nothing will
        # ever read it again.
        return r.status != "consumed"

    keep = [r for r in rows if _keep(r)]
    dropped = len(rows) - len(keep)
    if dropped:
        write_jsonl(path, keep)
    return dropped


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


# ─── clause-4 fast-path (P0479 hardening) ────────────────────────────────────
# Clause-4 let red tests through: "test-only diff = skip .#ci" merged a
# red test attr. 118-commit .#coverage-full break. Hardening: SKIP only
# on PROVEN hash-identity, not diff-category inference.

_QUEUE_HALTED = STATE_DIR / "queue-halted"
_LAST_GREEN_HASH = STATE_DIR / "last-green-ci-hash"


def halt_queue(reason: str) -> None:
    """Write the queue-halted sentinel. /dag-run checks this pre-dispatch
    and refuses to launch new work until cleared. Called when clause4_check
    finds red new-tests — the merge pipeline is provably broken, stacking
    more merges behind it wastes cycles."""
    ts = datetime.now(timezone.utc).isoformat()
    atomic_write_text(_QUEUE_HALTED, f"{ts}\n{reason}\n")


def queue_halted() -> str | None:
    """Read the queue-halted sentinel. Returns the reason (timestamp+detail)
    if halted, None if clear. /dag-run calls this pre-dispatch — nonempty
    return means DO NOT launch new /implement calls."""
    if _QUEUE_HALTED.exists():
        return _QUEUE_HALTED.read_text()
    return None


def clear_halt() -> bool:
    """Remove the sentinel. Coordinator calls this manually after fixing the
    root cause. Returns True if a sentinel was removed."""
    if _QUEUE_HALTED.exists():
        _QUEUE_HALTED.unlink()
        return True
    return False


# ─── coverage-full infra-break heuristic (P0484) ─────────────────────────────
# .#coverage-full is backgrounded (merger step 6) and writes CoverageResult
# to coverage-pending.jsonl. /dag-tick consumes these as test-gap followups
# — "write a test", not "undo the merge". That is correct for ONE scenario
# failing (genuine test regression). But when ≥3 scenarios fail, it is not a
# test-gap — the coverage pipeline itself is broken (profraw collection,
# llvm-cov export, lcov merge, or the instrumented build). The PSA break
# was all-scenarios-red and went 118 commits undetected because each red
# got filed as an individual test-gap. This heuristic writes queue-halted
# instead — same sentinel P0479 uses for clause-4 red-test detection.

# Matches both the VM test drv (scenario did not boot / test script failed)
# and the per-test lcov drv (profraw pipeline broke). nix/coverage.nix
# names the latter rio-cov-<scenario>. Captures scenario name from either.
_COV_SCENARIO_FAIL_RE = re.compile(
    r"^error: Cannot build '/nix/store/[a-z0-9]+-"
    r"(?:vm-test-run|rio-cov)-([\w-]+)\.drv'",
    re.MULTILINE,
)

# ≥3 scenarios red = infrastructure break. 1-2 could be genuine test
# regressions in specific scenarios; 3+ means the common substrate
# (instrumented build, profraw tarball machinery, lcov pipeline) is broken.
_COV_INFRA_THRESHOLD = 3


def coverage_full_red(log_path: str) -> tuple[int, list[str]]:
    """Count distinct scenario failures in a .#coverage-full build log.
    Returns (count, sorted_scenario_names). Called by build.coverage()
    post-red to decide between test-gap followup (coverage-pending.jsonl
    → /dag-tick) and queue-halted (block new /implement dispatch)."""
    try:
        log = Path(log_path).read_text()
    except (FileNotFoundError, OSError):
        return 0, []
    scenarios = sorted(set(_COV_SCENARIO_FAIL_RE.findall(log)))
    return len(scenarios), scenarios


def coverage_maybe_halt(log_path: str) -> bool:
    """Call after a red .#coverage-full build. Writes queue-halted if the
    failure is infrastructure-class (≥_COV_INFRA_THRESHOLD scenarios).
    Returns True if halted. The CoverageResult row still goes to
    coverage-pending.jsonl either way — this is ADDITIVE (halt + followup,
    not halt instead-of followup)."""
    n, scenarios = coverage_full_red(log_path)
    if n >= _COV_INFRA_THRESHOLD:
        preview = ", ".join(scenarios[:5]) + ("..." if n > 5 else "")
        halt_queue(
            f"coverage-full-red: {n} scenarios failed ({preview}) — "
            f"infrastructure-class break, not test-gap. Fix the coverage "
            f"pipeline before dispatching more merges."
        )
        return True
    return False

# Files that provably don't affect .#ci derivation hash. .claude/ is
# excluded via fileset.difference (P0304-T29); docs/ and *.md are
# docs-only. Anything under rio-*/ or nix/ CAN change the hash.
_PURE_DOCS_RE = re.compile(
    r"^(\.claude/|docs/|CLAUDE\.md$|README\.md$|\.github/|.*\.md$)"
)


def _ci_drv_hash() -> str | None:
    """nix eval .#ci.drvPath — the hash-identity proof. None on eval failure
    (flake locked to a broken rev, nix not in PATH, etc)."""
    try:
        out = subprocess.run(
            ["nix", "eval", "--raw", ".#ci.drvPath"],
            capture_output=True, text=True, check=True, cwd=REPO_ROOT,
        )
        return out.stdout.strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None


def _diff_files(base: str, head: str = "HEAD") -> list[str]:
    return [f for f in git("diff", "--name-only", f"{base}...{head}").splitlines() if f]


def _is_pure_docs(files: list[str]) -> bool:
    """Fallback when nix eval unavailable: diff touches ZERO files that
    could feed .#ci. Stricter than old "test-only" category — test files
    ARE derivation inputs."""
    return bool(files) and all(_PURE_DOCS_RE.match(f) for f in files)


def record_green_ci_hash() -> str | None:
    """Merger calls this post-green-.#ci. Next clause4_check compares
    against this. Returns the hash written (or None if eval failed)."""
    h = _ci_drv_hash()
    if h:
        atomic_write_text(_LAST_GREEN_HASH, h)
    return h


# Matches `#[test]` / `#[tokio::test]` / `#[rstest]` attrs followed by a
# fn decl on the next (or same-after-newlines) line. Captures the fn name.
_TEST_ATTR_RE = re.compile(
    r"#\[(?:tokio::)?(?:rs)?test(?:\([^)]*\))?\]\s*(?:#\[[^\]]*\]\s*)*"
    r"(?:pub\s+)?(?:async\s+)?fn\s+(\w+)",
    re.MULTILINE,
)


def _extract_new_test_names(base: str, head: str = "HEAD") -> list[str]:
    """Test fn names ADDED (not modified) by base..head. Parses the unified
    diff for +-prefixed #[test] attrs. Cheap — just regex over diff output."""
    diff = git("diff", f"{base}...{head}", "--unified=3", "--", "*.rs")
    # Keep only added lines (strip the leading '+'), drop the '+++' file
    # headers, re-join so the multi-line regex sees attr+fn together.
    added = "\n".join(
        ln[1:] for ln in diff.splitlines()
        if ln.startswith("+") and not ln.startswith("+++")
    )
    return _TEST_ATTR_RE.findall(added)


def _run_new_tests(names: list[str]) -> tuple[int, str]:
    """cargo nextest run <names> — targeted, seconds not minutes. Returns
    (rc, last-20-lines). nix develop wrapper so deps (fuse3 etc) resolve."""
    cmd = ["nix", "develop", ".#stable", "-c", "cargo", "nextest", "run"] + names
    proc = subprocess.run(cmd, capture_output=True, text=True, cwd=REPO_ROOT)
    tail = "\n".join((proc.stdout + proc.stderr).splitlines()[-20:])
    return proc.returncode, tail


def clause4_check(base: str) -> FastPathVerdict:
    """Clause-4 fast-path gate. base is the last-known-green ref (typically
    INTEGRATION_BRANCH pre-merge or FfResult.pre_merge).

    SKIP     → .#ci drv-hash provably identical to last green. Skip gate.
    RUN_FULL → hash changed OR can't prove identity. Run .#ci.
    HALT     → (T2 adds this) new tests found and red. Not reachable in T1.

    The old "test-only = skip" was wrong: adding a test changes the drv
    hash (new test = new derivation input). Hash-identity is the ONLY
    valid skip proof."""
    ci_hash = _ci_drv_hash()
    last_green = _LAST_GREEN_HASH.read_text().strip() if _LAST_GREEN_HASH.exists() else None

    # Clause-4: fast-path only when .#ci drv-hash unchanged vs last green.
    # "test-only diff" is NOT sufficient — adding a test changes the hash.
    if ci_hash and last_green and ci_hash == last_green:
        return FastPathVerdict(
            decision="SKIP", ci_hash=ci_hash, last_green_hash=last_green,
            reason="clause-4: hash-identical to last green — skip .#ci",
        )

    # Fallback: nix eval failed/unavailable → pure-docs category check.
    # "Pure docs" = touches ZERO files under rio-*/ or nix/ — stricter
    # than the old "test-only" (which let rio-*/tests/ through).
    if ci_hash is None:
        files = _diff_files(base)
        if _is_pure_docs(files):
            return FastPathVerdict(
                decision="SKIP", ci_hash=None, last_green_hash=last_green,
                reason=f"clause-4: nix eval unavailable, pure-docs fallback "
                f"({len(files)} files, all .claude/|docs/|*.md)",
            )

    # Hash changed → something observable changed. Before falling through
    # to RUN_FULL, check if the diff ADDED test attrs — those must be green
    # even for a fast-path candidate. Targeted nextest run (~seconds).
    new_tests = _extract_new_test_names(base)
    if new_tests:
        rc, tail = _run_new_tests(new_tests)
        if rc != 0:
            halt_queue(f"new test(s) red: {', '.join(new_tests)}\n{tail}")
            return FastPathVerdict(
                decision="HALT", ci_hash=ci_hash, last_green_hash=last_green,
                new_tests=new_tests,
                reason=f"clause-4: {len(new_tests)} new test(s) RED — "
                f"queue halted (fix before merge)",
            )
        # New tests green — still RUN_FULL (hash changed), but note them.
        return FastPathVerdict(
            decision="RUN_FULL", ci_hash=ci_hash, last_green_hash=last_green,
            new_tests=new_tests,
            reason=f"clause-4: {len(new_tests)} new test(s) green, "
            f"drv-hash changed — run full .#ci",
        )

    # Hash changed, no new tests → run the gate.
    return FastPathVerdict(
        decision="RUN_FULL", ci_hash=ci_hash, last_green_hash=last_green,
        reason="clause-4: drv-hash changed vs last-green — run full .#ci",
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
    # Compute all mappings FIRST (no writes), check for collisions,
    # THEN write. Previously the batch-doc rewrite loop flushed before
    # the collision check, leaving the worktree dirty on assert-fail.
    result: dict[str, dict[str, int]] = {}
    for rel, phs in found.items():
        start = _max_existing_t(worktree, rel, tgt) + 1
        result[rel] = {ph: start + i for i, ph in enumerate(phs)}

    # Slurp placeholder-doc contents once so the collision check can
    # test whether a colliding key is actually referenced (vs a
    # harmless per-doc NN-sequencing collision no placeholder reads).
    ph_texts = {rel: (worktree / rel).read_text() for rel in placeholder_docs}
    all_t: dict[str, int] = {}
    for m in result.values():
        # Two batch docs sharing a T-placeholder key with DIFFERENT
        # assignments → cross-doc T-ref in a placeholder doc would get
        # wrong-rewrite (last .update wins). Per-doc NN-sequencing means
        # key-sharing is legitimate (both docs start at <runid>01); only
        # a DISAGREEING collision that a placeholder doc ACTUALLY
        # REFERENCES is a bug. Check: collision key appears in any
        # placeholder doc's text.
        dup = {
            k for k in set(m) & set(all_t)
            if m[k] != all_t[k]
            and any(f"T{k}" in t for t in ph_texts.values())
        }
        assert not dup, (
            f"T-placeholder collision across batch docs (disagreeing "
            f"assignments, placeholder doc would wrong-rewrite): {dup}"
        )
        all_t.update(m)

    # Collision check passed — now write batch-doc rewrites.
    for rel, mapping in result.items():
        p = worktree / rel
        text = p.read_text()
        new = text
        for ph, assigned in mapping.items():
            # Replace T<placeholder> → T<assigned> everywhere in this doc
            # (headers, cross-refs, prose). No padding — T163 not T0163.
            new = new.replace(f"T{ph}", f"T{assigned}")
        if new != text:
            atomic_write_text(p, new)
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
