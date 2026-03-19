---
name: dag-tick
description: One pass of the DAG runner's mechanical reflex — launch verifiers for done impls, queue merges for PASS verdicts, flush followups batch if over threshold. Idempotent; safe to /loop. Does NOT make judgment calls (stale_verify acceptance, PARTIAL interpretation, merge ordering) — those stay with the coordinator.
---

## 0. Sentinel

```bash
test -e .claude/state/runner-stopped && { echo "Runner stopped (sentinel present) — no-op. Resume with /dag-run."; exit 0; }
```

`/dag-stop` wrote this. If it's there, skip everything below.

## 0b. Stale merger lock

```bash
lock_status=$(python3 .claude/lib/state.py merge-lock-status)
if echo "$lock_status" | jq -e '.stale' > /dev/null; then
    echo "POISONED LOCK: $(echo "$lock_status" | jq -c .content)" >&2
fi
```

`{"held":true,"stale":true}` means a merger crashed mid-run (PID dead, lock still on disk). Surface to coordinator — compare `.content.main_at_acquire` against current `git rev-parse --short main`: if same → merger died before ff, just `python3 .claude/lib/state.py merge-unlock`; if different → ff landed, partial state needs finish-from-step-5 (ff landed but cleanup + dag-flip didn't — complete steps 7-8 manually). Don't clear it yourself — coordinator call.

## Run the scan

```bash
python3 .claude/skills/dag-tick/dag_tick.py
```

Output is JSON (`TickReport` — `--schema` for the contract). Idempotent: running twice with no state change → second run is all-zeros.

## Act on each field

**`impls_needing_verify`** — for each: launch `/validate-impl <plan>`. The `note` field is the scrutiny-seed hint from agents-running (handoff notes or impl-report summary). Append a `role=verify` row:
```bash
python3 .claude/lib/state.py agent-row \
  '{"plan":"P<N>","role":"verify","agent_id":"<id>","worktree":"/root/src/rio-build/p<N>","status":"running","note":"launched from impl <hash>"}'
```

**`verify_pass`** — for each: append to merge-queue AND spawn reviewer:
```bash
python3 .claude/lib/state.py merge-queue \
  '{"plan":"P<N>","worktree":"/root/src/rio-build/p<N>","verdict":"PASS","commit":"<hash>"}'
```
Then `/review-impl <N>` — post-PASS code-quality pass (smells, test-gaps, convention). Advisory: reviewer writes to followups sink; findings land as plan docs later, don't block the merge. Append `role=review` row.

Do NOT launch merger — merge ordering is a coordinator judgment call. Update the verify row `status=consumed` so the next tick doesn't re-append.

**`verify_needs_judgment`** — PARTIAL/FAIL/BEHIND. Report them; coordinator decides fix-then-merge vs accept-with-followups vs bounce-to-impl. You do not act.

**`followups_should_flush`** — if `true`: invoke `/plan` (default mode reads `followups-pending.jsonl`). Append `role=writer` row. `/plan` step 6 truncates the sink after launching the writer. **Cadence proposals excluded from the trigger:** rows with `origin` in {`consolidator`, `bughunter`} (from `rio-impl-consolidator`/`rio-impl-bughunter`) accumulate in the sink but don't count toward the >15 threshold and don't fire the P-new trigger. They're advisory — coordinator reviews and manually promotes (or drops from sink) when ready. `followups_cadence_count` tells you how many are waiting.

**`coverage_regressions`** — for each: append a `test-gap` followup. The positional is `coverage` (a `state.FollowupOrigin` — sets `origin="coverage"`, `discovered_from=None`); the branch name goes in `description`/`deps`:
```bash
python3 .claude/lib/state.py followup coverage \
  '{"severity":"test-gap","description":"coverage regression post-<branch> merge — see <log_path>","proposed_plan":"P-batch-tests","deps":"<branch>"}'
```

**`flake_fix_phases`** — pass to the coordinator for frontier-launch prioritization (step 1 of `/dag-run`).

## Report

Summarize what you acted on. If the scan was all-zeros: the loop is idle, nothing to do this tick.
