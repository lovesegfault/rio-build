# Plan 427: Chain-merger — one long-lived merger processes FIFO queue

coord harness feature. Today each merge spawns a fresh `rio-impl-merger` agent via [`/merge-impl`](../../.claude/skills/merge-impl/SKILL.md) — lock-acquire, preflight, rebase, ff, `.#ci`, dag-flip, unlock, exit. The coordinator then spawns another for the next queue entry. Per-merge agent-launch overhead is ~30-60s (subagent spawn, context load, shell init) plus the per-merge lock round-trip.

At 161 merges this sprint (mc=79→240, per `merge-count.txt`), that's ~80-160min of wall-clock spent on launch-overhead alone. The rix chain-merger-3 run processed 23 merges with 0 rollbacks and 0 coordinator-roundtrips in a single agent session — the agent holds the lock across merges and loops over the queue.

**Design delta from per-merge:** the merger becomes a daemon-shaped loop. It acquires the lock ONCE, processes the queue head, dag-flips, and instead of exiting reads the NEXT queue row. On empty queue it polls (not exits) for ~N cycles before releasing. On CI-fail it still rolls back the failing plan but continues to the next row (the coordinator still sees the abort-report per plan). Lock lease must be bumped per iteration to avoid stale detection.

**Risks:** (a) a merger crash mid-loop leaves the lock held — same recovery as today (`lock-status` → `ff_landed` check → finish/unlock), but now up to ~25 merges might be in-flight state to reconcile. Mitigate: per-iteration `onibus merge lock-renew` with the CURRENT plan written to lock JSON, so `lock-status` always shows which plan the merger was processing when it died. (b) the coordinator loses visibility between merges — mitigate: merger writes a per-iteration `MergerReport` to `merge-reports.jsonl` that `/dag-tick` tails. (c) a stuck `.#ci` on plan N blocks plans N+1..N+k — same as today's FIFO but now explicit. Split at ~25 merges per session to bound crash blast-radius.

## Tasks

### T1 — `feat(harness):` onibus merge lock-renew — bump lease, rewrite lock JSON

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) — add `lock_renew(plan: str)` after `lock()` at `~:97`:

```python
def lock_renew(plan: str) -> None:
    """Renew the lease + update the plan field — chain-merger per-iteration.

    Does NOT acquire — fails if the lock isn't held. Atomically rewrites
    _LOCK_FILE with a fresh acquired_at and the NEW plan string. The
    agent_id and main_at_acquire stay from the original lock() call
    (chain-merger doesn't re-acquire, it renews in place)."""
    if not _LOCK_FILE.exists():
        raise SystemExit("lock-renew: no lock held — use `lock` to acquire first")
    existing = json.loads(_LOCK_FILE.read_text())
    existing["plan"] = plan
    existing["acquired_at"] = datetime.now(timezone.utc).isoformat()
    existing["renewed_count"] = existing.get("renewed_count", 0) + 1
    atomic_write_text(_LOCK_FILE, json.dumps(existing))
    print(json.dumps(existing))
```

`renewed_count` gives the coordinator visibility into how many iterations the chain-merger has run when it checks `lock-status`. MODIFY [`.claude/lib/onibus/cli.py`](../../.claude/lib/onibus/cli.py) to wire the `onibus merge lock-renew <plan>` subcommand.

### T2 — `feat(harness):` onibus merge queue-next — pop-and-mark next mergeable row

MODIFY same file — add `queue_next(max_batch: int = 25) -> MergeQueueRow | None` after `queue_consume` at `~:250`:

```python
def queue_next(max_batch: int) -> MergeQueueRow | None:
    """Return the next gate-clear queue row, or None if queue empty OR
    chain-merger has processed max_batch iterations.

    The per-agent iteration count is derived from the lock's renewed_count
    — once >= max_batch, return None to signal a clean session boundary.
    The coordinator then spawns a fresh chain-merger for the rest."""
    lock = lock_status()
    if lock.held and (lock.content or {}).get("renewed_count", 0) >= max_batch:
        return None
    rows = read_jsonl(_MERGE_QUEUE, MergeQueueRow)
    for row in rows:
        if gate_is_clear(row):
            return row
    return None
```

### T3 — `feat(harness):` rio-impl-merger — chain-mode loop in agent prompt

MODIFY [`.claude/agents/rio-impl-merger.md`](../../.claude/agents/rio-impl-merger.md) — add a `## Chain mode` section after the existing `## Protocol`:

````markdown
## Chain mode (when `--chain` is in the prompt)

Instead of processing one branch and exiting, loop over the merge queue:

```bash
cd /root/src/rio-build/main
.claude/bin/onibus merge lock "chain-<agent_id>" "<agent_id>"  # once
while true; do
  row=$(.claude/bin/onibus merge queue-next 25)
  [ -z "$row" ] && break  # queue empty OR session budget hit
  branch=$(jq -r .branch <<<"$row")
  .claude/bin/onibus merge lock-renew "$branch"

  # Steps 1-7.5 from ## Protocol, for $branch. Append each step's
  # JSON result to /tmp/rio-dev/chain-merge-<agent_id>.jsonl
  # (per-iteration MergerReport the coordinator tails).

  # On step-5 CI fail: roll back that plan, write abort-report row,
  # CONTINUE to next queue entry. Do NOT exit the loop.
  # On step-3 rebase-conflict or step-2 convco fail: write abort-report
  # row, queue entry stays (coordinator will re-gate it), continue.
done
.claude/bin/onibus merge unlock
# Write final ChainReport JSON: {iterations, merged, aborted, last_mc}.
```

**Symbolic-ref HEAD assertion** before each ff at step 4: `git symbolic-ref HEAD` MUST be `refs/heads/$TGT`. A chain-merger that processes plan A (which touches branch-ref state), then plan B, can drift if A's cleanup left HEAD detached. Assert on every iteration, not just once.

**Poll-not-exit on empty queue:** after `queue-next` returns empty once, poll 3× at 30s intervals before breaking — gives a validator-in-flight time to land a new PASS→queue row. Coordinator spawns a new chain-merger only when this one fully exits.

**Session budget:** `25` merges per chain-merger session. Beyond that, memory/context/log-size concerns; a fresh session is cleaner than a single ever-growing one. The budget is in `queue-next` so the coordinator doesn't need to count.
````

The existing single-branch mode stays — chain mode is opt-in via the prompt's `--chain` marker. Coordinator's `/dag-run` step 4 chooses which.

### T4 — `feat(harness):` /merge-impl --chain — skill-layer wrapper for chain mode

MODIFY [`.claude/skills/merge-impl/SKILL.md`](../../.claude/skills/merge-impl/SKILL.md) — add a `## Chain mode` section:

````markdown
## Chain mode (`/merge-impl --chain`)

Skip step 0a/0b (per-branch checks) and launch the merger in chain-loop mode:

```
Agent(
  subagent_type="rio-impl-merger",
  name="chain-merge",
  cwd="/root/src/rio-build/main",
  run_in_background=true,
  prompt="--chain\nSession budget: 25\nReport sink: /tmp/rio-dev/chain-merge-<id>.jsonl"
)
```

The merger loops over `merge queue-next` until empty or 25 iterations. Background — the coordinator proceeds with impl launches, tails the report-sink for per-plan outcomes. `run_in_background=true` means the task-notification fires on session END, not per iteration; the coordinator must read the sink file.

**When to use chain mode vs single:** chain when the queue has ≥3 gate-clear rows and the frontier is actively producing more. Single when one urgent merge needs to land immediately and the coordinator wants synchronous feedback.
````

### T5 — `feat(harness):` /dag-run — chain-merger spawn logic + sink tailing

MODIFY [`.claude/skills/dag-run/SKILL.md`](../../.claude/skills/dag-run/SKILL.md) at step 4 `**Merge.**`:

```markdown
4. **Merge.** If `merge-queue.jsonl` has ≥3 gate-clear rows AND no merger running: `/merge-impl --chain` (backgrounded). Otherwise: head of queue → `/merge-impl <branch>` (foregrounded, single). Chain-merger writes per-iteration `MergerReport` rows to `/tmp/rio-dev/chain-merge-<id>.jsonl`; tail it for per-plan outcomes:

   ```bash
   tail -f /tmp/rio-dev/chain-merge-*.jsonl | while read row; do
     # Each row is a MergerReport — match on .status/.abort_reason
     # same as the single-merge table below.
   done
   ```

   Chain-merger holds the lock across iterations (renewed per iter via `lock-renew`). `lock-status` shows the CURRENT plan being processed + `renewed_count`. On chain-merger task-notification (session end), read the final `ChainReport` for totals.
```

### T6 — `test(harness):` chain-merger regression tests

MODIFY [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py) — add after the existing `lock`/`count_bump` tests:

```python
def test_lock_renew_bumps_timestamp_and_plan(tmp_state_dir):
    """lock_renew updates acquired_at + plan, keeps agent_id, increments
    renewed_count. Fails if no lock held."""
    lock("p100", "agent-X")
    before = json.loads(_LOCK_FILE.read_text())
    time.sleep(0.1)
    lock_renew("p101")
    after = json.loads(_LOCK_FILE.read_text())
    assert after["plan"] == "p101"
    assert after["agent_id"] == before["agent_id"]  # preserved
    assert after["acquired_at"] > before["acquired_at"]
    assert after["renewed_count"] == 1

def test_queue_next_respects_session_budget(tmp_state_dir):
    """queue_next returns None once renewed_count >= max_batch, even
    with rows still in the queue."""
    # Seed 5 gate-clear rows.
    # lock + renew 3×.
    lock("chain-test", "agent-Y")
    for _ in range(3):
        lock_renew("chain-test")
    assert queue_next(max_batch=3) is None
    assert queue_next(max_batch=10) is not None  # budget allows
```

## Exit criteria

- `/nbr .#ci` green
- `nix develop -c pytest .claude/lib/test_scripts.py -k 'lock_renew or queue_next'` → ≥2 passed
- `grep 'def lock_renew\|def queue_next' .claude/lib/onibus/merge.py` → 2 hits
- `grep 'lock-renew\|queue-next' .claude/lib/onibus/cli.py` → ≥2 hits (subcommands wired)
- `grep '## Chain mode' .claude/agents/rio-impl-merger.md .claude/skills/merge-impl/SKILL.md` → 2 hits
- `grep 'chain.*merger\|--chain' .claude/skills/dag-run/SKILL.md` → ≥2 hits (spawn logic + sink tailing)
- `grep 'renewed_count' .claude/lib/onibus/merge.py` → ≥3 hits (lock_renew writes + queue_next reads + lock_status surfaces)
- Manual smoke (non-gating): coordinator spawns `/merge-impl --chain` with 5+ queued plans → single agent processes all, `chain-merge-*.jsonl` has one MergerReport row per plan, lock held throughout with `renewed_count` incrementing

## Tracey

No markers. Harness tooling — `.claude/` is outside the spec'd component surface.

## Files

```json files
[
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T1: +lock_renew() after :97; T2: +queue_next() after queue_consume ~:250"},
  {"path": ".claude/lib/onibus/cli.py", "action": "MODIFY", "note": "T1+T2: wire onibus merge lock-renew + onibus merge queue-next subcommands"},
  {"path": ".claude/agents/rio-impl-merger.md", "action": "MODIFY", "note": "T3: +## Chain mode section after ## Protocol — loop over queue-next, per-iter lock-renew, symbolic-ref assert, poll-not-exit"},
  {"path": ".claude/skills/merge-impl/SKILL.md", "action": "MODIFY", "note": "T4: +## Chain mode section — --chain flag, background launch, sink-file pointer"},
  {"path": ".claude/skills/dag-run/SKILL.md", "action": "MODIFY", "note": "T5: step-4 merge logic — ≥3 gate-clear → --chain; sink-tailing for per-iter reports"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T6: +test_lock_renew_bumps_* + test_queue_next_respects_session_budget"}
]
```

```
.claude/
├── lib/
│   ├── onibus/merge.py      # T1+T2: lock_renew + queue_next
│   ├── onibus/cli.py        # T1+T2: subcommand wiring
│   └── test_scripts.py      # T6: regression tests
├── agents/rio-impl-merger.md   # T3: chain loop
└── skills/
    ├── merge-impl/SKILL.md  # T4: --chain flag
    └── dag-run/SKILL.md     # T5: spawn logic
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [417, 418, 420, 421, 304], "note": "discovered_from=coord (rix chain-merger-3 §1). No hard-deps — builds on existing lock() + queue_consume() + dag_flip() in merge.py. Soft-dep P0417 (DONE — already-done double-bump fix; chain-merger's dag_flip calls use the same already-done path). Soft-dep P0418 (rename-canonicalization — chain-merger step-0a calls rename-unassigned per docs-branch; P0418's fixes apply). Soft-dep P0420 (count_bump TOCTOU — chain-merger's per-iter count_bump calls the fixed-order code). Soft-dep P0421 (merge.py rename-cluster extraction — if P0421 lands first, T1/T2 add to the post-split file; the lock/queue fns are NOT in the extracted cluster so stay in merge.py). Soft-dep P0304-T17/T203/T204 (merge.py _LEASE_SECS bump + subprocess→git() + scan-break — all merge.py, non-overlapping fns). merge.py is HOT (count~8 active plans); lock_renew/queue_next are NEW fns so textual conflict is limited to one _MERGE_QUEUE import potentially duplicated."}
```

**Depends on:** none — additive to existing lock/queue mechanics.

**Conflicts with:** [P0421](plan-0421-merge-py-rename-cluster-extract.md) extracts `:368-604` to `plan_assign.py` — T1's `lock_renew` lands at `~:97` (before the extraction), T2's `queue_next` depends on where `queue_consume` lives (stays in merge.py per P0421's scope). [P0304-T203/T204](plan-0304-trivial-batch-p0222-harness.md) touch `:76,:114,:154` + the already-done scan loop; T1/T2 add NEW fns — non-overlapping hunks.
