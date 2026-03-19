# Plan 0306: Harness — onibus merge 3-dot, lock lease, planner dag-append isolation

Five coordinator/bughunter-surfaced harness bugs, all in `.claude/lib/onibus/`. None have tracey markers (harness tooling is out of spec scope). All four caused real workflow failures during the sprint-1 run; fixing them together avoids separate `.claude/` rebase cycles.

**T1 (`behind_check` 2-dot):** [`git_ops.py:181`](../../.claude/lib/onibus/git_ops.py) computes `theirs` with `git diff HEAD..TGT` (2-dot). For `git diff`, 2-dot compares the two tree states directly — so `theirs` includes files the worktree itself touched (they show up as "undo my change" in the HEAD→TGT delta). The intersection `mine & theirs` over-reports: every file the worktree touched appears in `theirs` too, so `file_collision` = `mine`, always. **Five validators** independently confirmed phantom collisions; all manually verified via `git diff $(git merge-base HEAD sprint-1)...sprint-1 --name-only` and found empty intersection. The comment at lines 172-175 correctly describes 3-dot semantics; the code at line 181 contradicts it.

**T2 (lock PID stale-check):** [`merge.py:54`](../../.claude/lib/onibus/merge.py) stores `os.getpid()` — the PID of the `onibus merge lock` subprocess, which exits immediately after writing the lock file (the CLI is fire-and-forget; the merger agent invokes it via `Bash` and the bash subprocess terminates). When `lock_status()` at [`merge.py:95`](../../.claude/lib/onibus/merge.py) later checks `os.kill(pid, 0)`, the PID is **always dead** → `stale=True` → coordinator sees `POISONED` and aborts. The docstring at lines 3-6 acknowledges "the CLI exits immediately" but stores the PID anyway.

**T3 (planner dag-append writes-to-main):** [`rio-planner.md:110`](../../.claude/agents/rio-planner.md) tells the planner to run `.claude/bin/onibus dag append`. The `/plan` skill launches the planner with `cwd="/root/src/rio-build/main"` ([`SKILL.md:73`](../../.claude/skills/plan/SKILL.md)). Relative `.claude/bin/onibus` from that cwd resolves to **main's** onibus → [`__init__.py:24`](../../.claude/lib/onibus/__init__.py) `REPO_ROOT = Path(__file__).resolve().parents[3]` → main's `dag.jsonl`. The docs-905036 planner appended rows to main's dag.jsonl, then copied main→worktree to get them into its commit — picking up a concurrent P0218 status flip that caused a rebase conflict. The fix is a protocol change in the agent prompt, not a code change: **append rows directly to the worktree's `.claude/dag.jsonl` file** (the planner already creates a worktree; it should write there).

**T4 (`_cadence_range` start-off-by-one + end-unpin):** [`merge.py:130`](../../.claude/lib/onibus/merge.py) `return f"{out[-1]}..{INTEGRATION_BRANCH}"` has **two bugs on one line**. (a) **Start:** `out[-1]` is the (window+1)th-from-tip commit, but `git A..B` excludes A — so the consolidator got `1ad980c9..sprint-1` and missed P0222/P0218/P0220 (the oldest commits in its window). (b) **End:** `INTEGRATION_BRANCH` is a live branch ref, not `out[0]` (the tip SHA at cadence-computation time). Empirically observed this sprint: cadence fired at merge-count=7 with end=`sprint-1`; by the time bughunter ran `git diff`, sprint-1 had moved +12 commits (Build CRD deletion landed). Audit window grew 7→19 mid-run, non-deterministic. **Risk if unfixed:** cadence agents audit code from merge N+1 that has NOT passed its own per-plan review yet.

**T5 (`_cadence_range` commit-count ≠ merge-count — DISTINCT from T4):** T4 pins both ends but **still counts commits**. [`merge.py:125`](../../.claude/lib/onibus/merge.py) uses `git log --first-parent -{window+1}` which walks `window+1` **commits**; but `count_bump()` at [`merge.py:109`](../../.claude/lib/onibus/merge.py) bumps once per **plan merge**. Merges are fast-forward (no merge commit), so each plan lands as N first-parent commits. Empirically observed at merge-count=14: 7 plans merged since mc=7, but the coordinator-passed range `9f6eb110..sprint-1` was only **7 commits** = tail of P0243 + 2 docs commits. Missed P0294 (-2821 LoC), P0290, P0276, P0215 **entirely** — those are 33 commits behind tip. Bughunter audited a `rio-*/src` diff of **literally zero lines**. T4's `out[-1]^..out[0]` fix doesn't help: `out` is still `git log -{w+1}` output, still commit-indexed. **Risk if unfixed:** cadence agents structurally miss multi-commit plans; the more thorough the plan (more commits), the more likely it escapes audit.

## Tasks

### T1 — `fix(harness):` behind_check theirs-side 2-dot → 3-dot

MODIFY [`.claude/lib/onibus/git_ops.py`](../../.claude/lib/onibus/git_ops.py) at `:181`:

```python
# Before (WRONG — 2-dot includes my changes in the "undo" direction):
theirs = set((git_try("diff", f"HEAD..{INTEGRATION_BRANCH}", "--name-only", cwd=worktree) or "").splitlines())

# After (3-dot — merge-base → TGT, what TGT changed since fork):
theirs = set((git_try("diff", f"HEAD...{INTEGRATION_BRANCH}", "--name-only", cwd=worktree) or "").splitlines())
```

One character. The docstring at lines 172-175 is already correct ("3-dot (TGT...HEAD) = merge-base to HEAD"); the code just didn't match it on the `theirs` side.

**Test:** In [`.claude/lib/onibus/tests/`](../../.claude/lib/onibus/tests/) (find or create `test_git_ops.py`), add a fixture that creates a repo with a fork point, commits `file_a` on branch A and `file_b` on branch B (no overlap), and asserts `behind_check(worktree_A).file_collision == []`. Before the fix, it returns `["file_a"]` (phantom self-collision).

### T2 — `fix(harness):` lock stale-check via agent_id, not PID

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py):

At `:54`, replace `"pid": os.getpid()` with a lease timestamp. PID-liveness was the wrong mechanism — the right question is "has the merger agent heartbeated recently?", but agents don't heartbeat. Simpler: **time-based lease**. Store `acquired_at` (already there) and define stale = `now - acquired_at > LEASE_SECS` where `LEASE_SECS` covers the longest observed merge (CI run is the long pole; ~25 min for `.#coverage-full` backgrounded but the merge itself is ~10 min).

```python
# At module top, after _LOCK_FILE:
_LEASE_SECS = 30 * 60  # merger ff + post-ff CI-cache-hit re-validate; generous

# In lock(), :53-62, replace content dict — drop "pid", keep agent_id as
# the identity for debugging:
content = {
    "agent_id": agent_id,
    "plan": plan,
    "acquired_at": datetime.now(timezone.utc).isoformat(),
    "main_at_acquire": subprocess.run(...).stdout.strip(),
}
# Drop the try/except FileExistsError→os.kill(pid,0) path at :70-75;
# replace with:
except FileExistsError:
    existing = json.loads(_LOCK_FILE.read_text())
    acquired = datetime.fromisoformat(existing["acquired_at"])
    age_s = (datetime.now(timezone.utc) - acquired).total_seconds()
    stale = age_s > _LEASE_SECS
    print(json.dumps({"error": "lock-held", "holder": existing, "age_secs": age_s, "stale": stale}), file=sys.stderr)
    sys.exit(4)

# In lock_status(), :88-106, replace the os.kill(pid,0) check:
def lock_status() -> LockStatus:
    if not _LOCK_FILE.exists():
        return LockStatus(held=False, stale=False, content=None)
    content = json.loads(_LOCK_FILE.read_text())
    acquired = datetime.fromisoformat(content["acquired_at"])
    age_s = (datetime.now(timezone.utc) - acquired).total_seconds()
    stale = age_s > _LEASE_SECS
    ff_landed = None
    if stale:
        current = subprocess.run(...).stdout.strip()
        ff_landed = current != content.get("main_at_acquire")
    return LockStatus(held=True, stale=stale, content=content, ff_landed=ff_landed)
```

MODIFY [`.claude/lib/onibus/models.py`](../../.claude/lib/onibus/models.py) — `LockStatus` model: if it has a `holder_alive` or `pid` field, remove it. Check at impl time.

**Test:** `test_lock_stale_after_lease()` — write a lock file with `acquired_at` = now - 31min, assert `lock_status().stale == True`. `test_lock_fresh()` — now - 1min → `stale == False`.

### T3 — `fix(harness):` planner protocol — dag rows to worktree, not via onibus

MODIFY [`.claude/agents/rio-planner.md`](../../.claude/agents/rio-planner.md) at `:107-121` (the "dag.jsonl integration" section):

Replace the `onibus dag append` bash block with direct-append guidance:

````markdown
### 1. Plan Table row (dag.jsonl append — YOUR WORKTREE, NOT MAIN)

**Do NOT use `onibus dag append`** — when your cwd resets to main (agent
threads reset cwd between bash calls), relative `.claude/bin/onibus`
resolves to main's onibus → writes to main's `dag.jsonl`. This caused
docs-905036's rebase conflict (picked up a concurrent P0218 flip).

Instead, append directly to your worktree's file. From inside your
worktree (`/root/src/rio-build/docs-<runid>`):

```bash
# Append one line per new plan. Keep the schema exact (see tail -1 for reference).
python3 -c '
import json
row = {"plan": <NNN>, "title": "<title>", "deps": [<d1>,<d2>], "deps_raw": None,
       "tracey_total": <exit-crit-count>, "tracey_covered": <marker-count>,
       "crate": "<csv>", "priority": 50, "status": "UNIMPL",
       "complexity": "<LOW|MEDIUM|HIGH>", "note": "<one-line>"}
print(json.dumps(row))
' >> /root/src/rio-build/docs-<runid>/.claude/dag.jsonl
```

Validate with `onibus dag validate` — it reads from cwd's `REPO_ROOT`
which IS correct when invoked from the worktree.
````

Also update the `Verification gate` step 3 in `rio-planner.md` — it currently says `git diff main -- .claude/dag.jsonl` which compares against `main` the branch; change to `git diff $(cat .claude/integration-branch) -- .claude/dag.jsonl` to compare against the actual integration target.

**Also** MODIFY [`.claude/skills/plan/SKILL.md`](../../.claude/skills/plan/SKILL.md) at `:81-87` — step 6 truncates `followups-pending.jsonl` **before** the planner reads it (race: the prompt says "read the full sink" but the sink is already empty). Either:
- **Option A (preferred):** Move truncation to step 7 (after planner commits and QA passes — if QA fails, the followups are still available for the next run)
- **Option B:** Write a backup before truncation: `cp .claude/state/followups-pending.jsonl /tmp/followups-backup-<runid>.jsonl && : > .claude/state/followups-pending.jsonl`

Option A is cleaner. The truncation point should be "after the rows are durably captured in a commit," not "after the prompt is composed."

### T4 — `fix(harness):` _cadence_range start-parent + pinned-end

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) at `:130`:

```python
# Before (WRONG — two bugs):
#   (a) out[-1] as start: git A..B excludes A, so the oldest commit
#       in the window is missed
#   (b) INTEGRATION_BRANCH as end: live ref moves between cadence
#       computation and cadence-agent execution → window grows
return f"{out[-1]}..{INTEGRATION_BRANCH}"

# After:
#   (a) out[-1]^ : parent of the (window+1)th commit → git range now
#       includes out[-1] itself
#   (b) out[0] : the tip SHA at git-log time → pinned, deterministic
return f"{out[-1]}^..{out[0]}"
```

One line. `out[-1]^` handles the start-exclusion (git `..` excludes the left side; `^` moves to the parent so the oldest window-commit is included). `out[0]` is the tip SHA from `git log --first-parent -{window+1}` at `:125` — immutable, so the cadence agent audits exactly the commits that existed when `cadence()` was called.

**Test:** In `test_merge.py` (T2's file), add `test_cadence_range_pins_both_ends()`:
```python
def test_cadence_range_pins_both_ends(tmp_repo_with_history):
    # Fixture: repo with 10 linear commits on integration branch.
    # _cadence_range(5) should return "<sha6>^..<sha10>" where shaN
    # is the Nth-from-tip. Assert:
    #   - range string ends with out[0] (a SHA, not a branch name)
    #   - range string starts with out[-1] + "^"
    #   - git rev-list <range> returns exactly 5 commits
    rng = _cadence_range(5)
    assert not rng.endswith(INTEGRATION_BRANCH)  # not live ref
    assert "^.." in rng                          # parent-of-start
    # rev-list count proves both bounds correct at once:
    count = subprocess.run(
        ["git", "rev-list", "--count", rng], ...
    ).stdout.strip()
    assert count == "5"
```

### T5 — `fix(harness):` _cadence_range — record mc→SHA at count_bump, index by merge-count

**Root cause:** `_cadence_range(window)` answers "what are the last `window` commits?" but the caller wants "what are the last `window` **plan-merges**?" These are the same number only when every plan is exactly one commit. Fast-forward merges of multi-commit plans break the 1:1.

**Fix:** Record the integration-branch tip SHA **at each `count_bump()`** alongside the count. `_cadence_range(window)` then looks up `mc_current - window` in that map and uses its SHA as the start. No commit-counting.

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py):

**At `count_bump()` (`:109-119`):** alongside `merge-count.txt`, maintain a `merge-shas.jsonl` append-only map of `{mc: int, sha: str, ts: str}`:

```python
def count_bump(set_to: int | None = None) -> int:
    """Cadence counter: mod 5 → consolidator, mod 7 → bughunter.
    Also records the integration-branch tip SHA at this mc —
    _cadence_range() indexes by mc, not commit-count."""
    count_file = STATE_DIR / "merge-count.txt"
    sha_file = STATE_DIR / "merge-shas.jsonl"
    if set_to is not None:
        new = set_to
    else:
        cur = int(count_file.read_text().strip()) if count_file.exists() else 0
        new = cur + 1
    count_file.parent.mkdir(parents=True, exist_ok=True)
    count_file.write_text(f"{new}\n")
    # Record tip at THIS merge-count. Append-only; _cadence_range reads
    # the last row with mc == (current - window).
    tip = subprocess.run(
        ["git", "rev-parse", INTEGRATION_BRANCH],
        capture_output=True, text=True, check=True,
    ).stdout.strip()
    with sha_file.open("a") as f:
        f.write(json.dumps({"mc": new, "sha": tip, "ts": datetime.now(timezone.utc).isoformat()}) + "\n")
    return new
```

**At `_cadence_range()` (`:122-130`):** replace `git log --first-parent -{w+1}` with a lookup:

```python
def _cadence_range(window: int) -> str | None:
    """git range for the last `window` PLAN MERGES (not commits).
    Reads merge-shas.jsonl: start = SHA at (current_mc - window),
    end = SHA at current_mc. Both pinned; no commit-counting."""
    sha_file = STATE_DIR / "merge-shas.jsonl"
    count_file = STATE_DIR / "merge-count.txt"
    if not sha_file.exists() or not count_file.exists():
        return None
    current_mc = int(count_file.read_text().strip())
    start_mc = current_mc - window
    if start_mc < 0:
        return None  # not enough history yet
    # Last row per mc wins (handles set_to re-writes).
    by_mc: dict[int, str] = {}
    for line in sha_file.read_text().splitlines():
        row = json.loads(line)
        by_mc[row["mc"]] = row["sha"]
    if start_mc not in by_mc or current_mc not in by_mc:
        return None  # gap in the map (pre-T5 history)
    return f"{by_mc[start_mc]}..{by_mc[current_mc]}"
```

Note: `A..B` excludes `A` but that's **correct** here — `by_mc[start_mc]` is the tip *after* merge `start_mc` landed, which is exactly the boundary we want (audit everything merged in `(start_mc, current_mc]`). No `^` needed — T4's `^` was compensating for commit-walk off-by-one, which no longer exists.

**Bootstrap for pre-T5 history:** `merge-shas.jsonl` won't have rows for mc<current. Either (a) `_cadence_range` returns `None` until `window` merges accumulate post-T5 (simplest — one cadence cycle lost), or (b) backfill via `onibus merge count-bump --set-to N` × historical N with manually-identified SHAs. Option (a) is fine; document it in the docstring.

**Delete T4's `^` fix if both land together** — T5 replaces the whole function body. If T4 lands first (separate commit), T5 overwrites it. Either order is fine.

**Test:** `test_cadence_range_indexes_by_merge_count()`:
```python
def test_cadence_range_indexes_by_merge_count(tmp_repo, tmp_state_dir):
    # Fixture: integration branch with 3 "plan merges":
    #   mc=1: 1 commit (plan A)
    #   mc=2: 5 commits (plan B — multi-commit, the bug trigger)
    #   mc=3: 2 commits (plan C)
    # Total 8 commits. _cadence_range(2) should span mc=2..3 = 7 commits
    # (plan B's 5 + plan C's 2), NOT the last 3 commits (which would
    # miss most of plan B).
    for mc, n_commits in [(1, 1), (2, 5), (3, 2)]:
        for _ in range(n_commits):
            _commit(tmp_repo)  # helper: empty commit on integration branch
        count_bump()  # records tip SHA at this mc
    rng = _cadence_range(2)
    assert rng is not None
    count = subprocess.run(
        ["git", "rev-list", "--count", rng], cwd=tmp_repo,
        capture_output=True, text=True, check=True,
    ).stdout.strip()
    assert count == "7", (
        f"expected 7 commits (plan B's 5 + plan C's 2); got {count}. "
        "Pre-T5 commit-counting would return 3 (last window+1 commits)."
    )
```

## Exit criteria

- `/nbr .#ci` green (or: `cd .claude/lib/onibus && python3 -m pytest tests/ -v` if onibus tests aren't in the nix check — verify at dispatch)
- `test_git_ops.py::test_behind_check_no_phantom_self_collision` — fork with non-overlapping file sets → `file_collision == []`
- `test_merge.py::test_lock_stale_after_lease` — lock aged 31min → `stale == True`
- `test_merge.py::test_lock_fresh` — lock aged 1min → `stale == False`
- `grep 'onibus dag append' .claude/agents/rio-planner.md` → 0 hits in bash-fence context (may survive in "Do NOT use" prose)
- `grep 'HEAD\.\.[^.]' .claude/lib/onibus/git_ops.py` → only `rev-list` usages (rev-list 2-dot is correct; diff 2-dot was the bug)
- `test_merge.py::test_cadence_range_pins_both_ends` — `_cadence_range(5)` on a 10-commit repo → range ends with SHA (not branch name), contains `^..`, `git rev-list --count` returns exactly 5 (**superseded by T5's test if both land together** — T5 rewrites the function body)
- `grep 'INTEGRATION_BRANCH' .claude/lib/onibus/merge.py` — still appears in `_cadence_range` at `:125` (the `git log` arg) but NOT at `:130` (the return) — T4 pins the end (**or: T5 removes `git log` from `_cadence_range` entirely**)
- `test_merge.py::test_cadence_range_indexes_by_merge_count` — 3 merges of 1/5/2 commits each → `_cadence_range(2)` spans 7 commits (plan B + plan C), proving mc-index not commit-index (T5)
- `test -f .claude/state/merge-shas.jsonl` after one `count_bump()` call (T5: file created)
- `grep 'git log.*first-parent.*-{' .claude/lib/onibus/merge.py` — 0 hits in `_cadence_range` body (T5: commit-counting removed)

## Tracey

No marker changes. Harness tooling is not spec-covered (no `harness.*` domain in `TRACEY_DOMAINS` at [`tracey.py:16`](../../.claude/lib/onibus/tracey.py)). These are workflow-correctness fixes, not product behavior.

## Files

```json files
[
  {"path": ".claude/lib/onibus/git_ops.py", "action": "MODIFY", "note": "T1: theirs 2-dot→3-dot at :181"},
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T2: PID→time-lease at :54,:70-75,:88-106 (_LEASE_SECS=30*60); T4: _cadence_range out[-1]^..out[0] at :130; T5: count_bump() records mc→SHA to merge-shas.jsonl at :109-119, _cadence_range() indexes by mc at :122-130 (replaces T4 if landing together)"},
  {"path": ".claude/lib/onibus/models.py", "action": "MODIFY", "note": "T2: LockStatus drop pid/holder_alive fields if present"},
  {"path": ".claude/agents/rio-planner.md", "action": "MODIFY", "note": "T3: replace onibus-dag-append block with worktree-direct-append at :107-121"},
  {"path": ".claude/skills/plan/SKILL.md", "action": "MODIFY", "note": "T3: move followups truncation from step 6 to step 7 (post-QA)"},
  {"path": ".claude/lib/onibus/tests/test_git_ops.py", "action": "MODIFY", "note": "T1: phantom-collision test (NEW if file doesn't exist)"},
  {"path": ".claude/lib/onibus/tests/test_merge.py", "action": "MODIFY", "note": "T2: lease-stale tests; T4: cadence-range pin test; T5: cadence-range mc-index test (NEW if file doesn't exist)"}
]
```

```
.claude/lib/onibus/
├── git_ops.py                    # T1: :181 one-char fix (.. → ...)
├── merge.py                      # T2: PID → time-lease  T4/T5: cadence mc-index
├── models.py                     # T2: LockStatus field cleanup
└── tests/
    ├── test_git_ops.py           # T1: phantom-collision test
    └── test_merge.py             # T2: lease-stale tests
.claude/agents/rio-planner.md     # T3: dag-append protocol
.claude/skills/plan/SKILL.md      # T3: truncation timing
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "Harness-only. No rio-*/ code touched. T1 is one character. T2 replaces PID-liveness with time-lease (LockStatus schema tightens). T3 is agent-prompt protocol fix — NOTE agent .md prompts are SESSION-CACHED so this only takes effect after coordinator session restart. T4 is one-line cadence-range fix (out[-1]^..out[0]). T5 replaces commit-counting with mc→SHA map — T4's fix still counts COMMITS; T5 indexes by MERGE-COUNT. Empirically at mc=14: cadence agents audited rio-*/src diff of ZERO lines because 7-commit window missed P0294/P0290/P0276/P0215 (33 commits behind tip). T5 supersedes T4's body if landing together. Land early so validators stop phantom-blocking, planners stop clobbering main's dag.jsonl, cadence agents get deterministic AND correctly-sized windows."}
```

**Depends on:** none. Pure tooling.
**Conflicts with:** [`models.py`](../../.claude/lib/onibus/models.py) is also touched by [P0304](plan-0304-trivial-batch-p0222-harness.md) T1 (PlanFile regex at `:322`) — different section of the file, trivial merge. [`rio-planner.md`](../../.claude/agents/rio-planner.md) is agent-prompt territory — session-cached, so land during a coordinator pause. No other UNIMPL plan touches `git_ops.py` or `merge.py`.
