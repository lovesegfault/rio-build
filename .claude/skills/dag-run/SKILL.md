---
name: dag-run
description: Become the DAG runner. Loads the coordinator loop. The session stays interactive — user can intervene mid-run with corrections, requests, questions. Invoke once at session start; the loop runs until the frontier is exhausted or the user stops.
---

You are the DAG runner. First, clear any prior-stop sentinel and read the integration branch:

```bash
rm -f .claude/state/runner-stopped
TGT=$(.claude/bin/onibus integration-branch)  # e.g., "sprint-1" — the sprint's merge target
git rev-parse --abbrev-ref HEAD  # verify coordinator worktree is checked out to $TGT
```

The coordinator worktree (`/root/src/rio-build/main`) must have `$TGT` checked out — mergers run `git merge --ff-only` from here, so HEAD determines what advances. `main` stays at the last stable cut.

Loop:

1. **Frontier.** `.claude/bin/onibus dag launchable --parallel 10` → sorted by `(effective_priority desc, impact desc)`, collision-free. Priority propagates backward through deps — bump a blocked plan and its blockers inherit that priority on the frontier. `onibus dag set-priority <N> <1-100>` (default 50); bump flake-owners to ~90 when adding a known-flake. `--verbose` shows `(eff=N←)` for propagated priorities. Launch via `/implement <N>`. Throttle: ~10 parallel is a **ceiling**, not a batch size — keep slot count near ceiling continuously. Write each launch via:
   ```bash
   .claude/bin/onibus state agent-start impl P<N> --id <agent-id> --note "<brief>"
   ```
   Worktree path is derived. (The old hand-typed JSON form via `state agent-row` still works if you need a non-standard worktree.)

2. **Tick.** `/dag-tick` handles the mechanical reflex: impl-done → launch validator; verify-PASS → append merge-queue; followups-pending over threshold → launch writer; coverage regressions → append follow-up. Invoke after each completion notification, or `/loop 2m /dag-tick` to poll.

3. **Validator outcome** (what `/dag-tick` hands back to you):
   - `PASS` → already queued by `/dag-tick`, and `rio-impl-reviewer` already spawned (advisory — smells+followups to sink, doesn't block merge). Your call on merge order.
   - `BEHIND` → re-launch validator directly. The impl's rebase is proactive now (step 0 of its gate) — if the validator saw BEHIND, the impl's `.#ci` ran stale; bounce it back to impl with "re-run your verification gate."
   - `FAIL` → `/fix-impl <report>` → re-launch validator.
   - `PARTIAL` → **your judgment.** Fix-then-merge (surgical, same worktree) vs accept-with-followups (doc-bug, spec-was-wrong). Neither `/dag-tick` nor any agent decides this.

4. **Merge.** Head of `.claude/state/merge-queue.jsonl` → check `gate`. If `gate` is `None` or the gate condition is satisfied (`gate_is_clear()` — `plan_merged` checks dag.jsonl status, `ci_green` greps the log, `manual` never auto-clears), `/merge-impl <branch>`. Otherwise skip to the next unguarded row. One at a time. Read via:
   ```bash
   .claude/bin/onibus merge queue-gates
   ```
   One JSON line per row: `{"plan": ..., "gate": ..., "clear": true|false}`.
   - **Atomicity precondition** (`/merge-impl` step 0b, before merger spawn): `mega-commit` / `chore-touches-src` → back to impl agent. Merger never ran.
   - **Merger outcome** (parse the ```json `MergerReport` fence; match on `report.status` / `report.abort_reason`):
     - `report.status == "merged"` → merger already committed the DAG delta (step 7.5) AND bumped `merge-count.txt` via `onibus merge count-bump`. Coverage is backgrounded; `/dag-tick` picks up regressions as follow-ups. `report.behind_worktrees` is informational — impls self-rebase at their gate, you don't broadcast.
     - `report.status == "merged"` and `report.stale_verify_commits_moved > 3` → your judgment: accept (most merges) or re-verify on `$TGT` retroactively.
     - `report.abort_reason == "ci-failed"` → merger rolled back. `rio-ci-fixer` on a throwaway worktree with `report.failure_detail` (log tail).
     - `report.abort_reason` in {`"rebase-conflict"`, `"non-convco-commits"`} → back to impl agent.
     - `report.abort_reason == "lock-held"` → **you launched two mergers.** Coordinator discipline failure. The second merger aborted at step 0 without touching anything; the first is still running. Wait for it. If `failure_detail` shows `holder_alive: false`, check `.claude/bin/onibus merge lock-status` — read `ff_landed` to know if you need to finish-from-step-5, then `merge unlock`.

   **merge-count:** merger owns the bump at step 7.5. Do NOT pre-bump on inferred merges — double-bump risk when notification lags.

   **merger lock:** check `.claude/bin/onibus merge lock-status` before any direct-to-`$TGT` commit. `held: false` → safe. `held: true, stale: false` → merger in flight, WAIT. `held: true, stale: true` → merger died — read `ff_landed`: `false` → just `merge unlock`; `true` → ff landed, finish steps 7-8 manually then unlock.

   **After each merge — `report.unblocked` tells you which plans just entered frontier.** Launch them immediately. The merge queue is a FIFO for serial merger work; it does not gate parallel impl work. Mergers are serialized (one `.#ci`); impls are parallel.

5. **Mid-run requests — two kinds, routed by "does it commit?":**
   - **Steering a running agent** ("that P0120 agent is stuck, tell it to try X", "I disagree with P0109's approach, redirect it", "P0142's output is wrong, stop it") → `SendMessage` to that agent directly. No commit, no plan, no race. The agent_id is in `.claude/state/agents-running.jsonl`.
   - **Changing files on disk** ("fix the flags", "create an agent", "add a known-flake entry") → `/plan --inline '<json-array>'`. Writer creates a plan doc (tooling-batch kind for `.claude/`/`CLAUDE.md`/`flake.nix`), you schedule it. **Never spawn a generic worker that commits to `$TGT`** — that's the `b4cd717` race.
     - Known-flake entries: **do not add the entry yourself.** Pass the flake details through `/plan --inline` — embed the KnownFlake fields (minus `fix_owner`) in the followup `description`:
       ```
       /plan --inline '[{"severity":"test-gap","proposed_plan":"P-new","description":"flake-fix: <test>. KnownFlake: {\"test\":\"<name>\",\"symptom\":\"<brief>\",\"root_cause\":\"<brief>\",\"fix_description\":\"<what>\",\"retry\":\"Once\"}"}]'
       ```
       Writer renders a `## Known-flake entry` section in the fix-plan doc (filling `fix_owner` with its placeholder; `/merge-impl` rewrites to the real number). The fix-plan impl reads that section, adds the entry via relative `.claude/bin/onibus flake add` in its worktree, commits it alongside the fix — entry and fix merge atomically. **No live bridge**: the entry is on `$TGT` only after the fix merges, by which time the fix is in. Flake fixes should be fast (and the `fix_owner: P<N>` validator already forces the plan to exist first). If bridging matters — slow fix, other agents keep hitting it — coordinator can manually `onibus flake add` + commit before the fix-plan, but that's the exception, not the rule.

   If unsure which kind: ask "will this `git commit` anything?" Steering doesn't; file changes do.

6. **Cadence agents.** `report.cadence` from the merge notification has `{consolidator.due, consolidator.range, bughunter.due, bughunter.range}`:
   - `consolidator.due: true` → spawn `rio-impl-consolidator` with `consolidator.range`. Duplication/weak-abstraction review. Writes followups with `origin: "consolidator"`.
   - `bughunter.due: true` → spawn `rio-impl-bughunter` with `bughunter.range`. Cross-plan bug patterns. Writes followups with `origin: "bughunter"`.

   Both can be due at once (coprime — merge 35, 70, …). Both write `discovered_from: None` + a `severity: "trivial"` null-result marker when nothing found. **Cadence proposals don't auto-flush via `/dag-tick`** — rows with `origin` in {`consolidator`, `bughunter`} are filtered out of the flush trigger. They wait for coordinator review: promote via `/plan` or drop from sink.

State lives in `.claude/state/` (gitignored scratch). Judgment stays with you.
