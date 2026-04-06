---
name: dag-stop
description: Stop the DAG runner — write the sentinel, cancel the tick cron, snapshot in-flight state for handoff. Running agents are NOT killed (they finish in background; results land in worktrees + state files). Resume with /dag-run.
---

## 1. Write the sentinel

```bash
touch .claude/state/runner-stopped
```

`/dag-tick` checks this at step 0 and no-ops. Belt-and-suspenders against a stale cron or a manual tick after stop.

## 2. Cancel the tick cron

`CronList` → look for any scheduled `/dag-tick` (or `/loop` wrapping it). `CronDelete` each. If there are none, note that in your report — the runner was tick-on-notification, not looped.

## 3. Snapshot

```bash
.claude/bin/onibus stop
```

Output is `StopSnapshot` JSON (`--schema` for the contract). Pure read — no side effects.

## 4. Report

Render the snapshot as a handoff. The shape:

```
Runner stopped (main @ <sha>).

In-flight agents (continuing in background):
  <plan> <role> → <worktree>  — <note>
  ...
  [or: none]

Merge queue (waiting):
  <plan> <verdict> @ <commit>
  ...
  [or: empty]

Followups sinked: <total> rows (<pnew> P-new)
  [if pnew > 0: these would flush on next tick — consider /plan now]

Resume: /dag-run
```

Running agents will complete and update `agents-running.jsonl`. Their worktrees stay. `/dag-run` step 0 clears the sentinel and picks up where this left off.

## Hard stop (rare)

If you actually need to kill running agents — something is deeply wrong, or the user is abandoning the wave entirely — do steps 1–3, then for each `in_flight` row with a non-null `agent_id`: `TaskStop(agent_id)`. Worktrees may have uncommitted changes; say so. Default is graceful (don't kill); only hard-stop if the user asks.
