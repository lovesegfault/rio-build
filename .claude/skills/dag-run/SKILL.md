---
name: dag-run
description: Become the DAG runner. Loads the coordinator loop. The session stays interactive — user can intervene mid-run with corrections, requests, questions. Invoke once at session start; the loop runs until the frontier is exhausted or the user stops.
---

You are the DAG runner. First, clear any prior-stop sentinel:

```bash
rm -f .claude/state/runner-stopped
```

Loop:

1. **Frontier.** `/dag-status` → ready plans. **Prioritize flake-fix plans** — any plan in `.claude/known-flakes.jsonl`'s `fix_owner` field goes first (`/dag-tick` reports them in `flake_fix_phases`); flakes tax every other agent's `.#ci` until fixed. Then launch the rest via `/implement <N>`, respecting the live collision check. Serialize chains per `.claude/collisions.jsonl`. Throttle: ~10 parallel agents — this is a **ceiling**, not a batch size. Keep the slot count near the ceiling continuously; don't launch-10-and-wait. Write each launch via:
   ```bash
   python3 .claude/lib/state.py agent-row \
     '{"plan":"P<N>","role":"impl","agent_id":"<id>","worktree":"/root/src/rio-build/p<N>","status":"running","note":"<brief>"}'
   ```

2. **Tick.** `/dag-tick` handles the mechanical reflex: impl-done → launch validator; verify-PASS → append merge-queue; followups-pending over threshold → launch writer; coverage regressions → append follow-up. Invoke after each completion notification, or `/loop 2m /dag-tick` to poll.

3. **Validator outcome** (what `/dag-tick` hands back to you):
   - `PASS` → already queued by `/dag-tick`, and `rio-impl-reviewer` already spawned (advisory — smells+followups to sink, doesn't block merge). Your call on merge order.
   - `BEHIND` → re-launch validator directly. The impl's rebase is proactive now (step 0 of its gate) — if the validator saw BEHIND, the impl's `.#ci` ran stale; bounce it back to impl with "re-run your verification gate."
   - `FAIL` → `/fix-impl <report>` → re-launch validator.
   - `PARTIAL` → **your judgment.** Fix-then-merge (surgical, same worktree) vs accept-with-followups (doc-bug, spec-was-wrong). Neither `/dag-tick` nor any agent decides this.

4. **Merge.** Head of `.claude/state/merge-queue.jsonl` → check `gate`. If `gate` is `None` or the gate condition is satisfied (`gate_is_clear()` — `plan_merged` checks dag.jsonl status, `ci_green` greps the log, `manual` never auto-clears), `/merge-impl <branch>`. Otherwise skip to the next unguarded row. One at a time. Read via:
   ```bash
   python3 .claude/lib/state.py merge-queue-gates
   ```
   One JSON line per row: `{"plan": ..., "gate": ..., "clear": true|false}`.
   - **Atomicity precondition** (`/merge-impl` step 0b, before merger spawn): `mega-commit` / `chore-touches-src` → back to impl agent. Merger never ran.
   - **Merger outcome** (parse the ```json `MergerReport` fence; match on `report.status` / `report.abort_reason`):
     - `report.status == "merged"` → merger already committed the DAG delta (step 7.5) AND bumped `merge-count.txt` via `state.py merge-count-bump`. Coverage is backgrounded; `/dag-tick` picks up regressions as follow-ups. `report.behind_worktrees` is informational — impls self-rebase at their gate, you don't broadcast.
     - `report.status == "merged"` and `report.stale_verify_commits_moved > 3` → your judgment: accept (most merges) or re-verify on main retroactively.
     - `report.abort_reason == "ci-failed"` → merger rolled back. `rio-ci-fixer` on a throwaway worktree with `report.failure_detail` (log tail).
     - `report.abort_reason` in {`"rebase-conflict"`, `"non-convco-commits"`} → back to impl agent.
     - `report.abort_reason == "lock-held"` → **you launched two mergers.** Coordinator discipline failure. The second merger aborted at step 0 without touching anything; the first is still running. Wait for it. If `failure_detail` shows `holder_alive: false` (crashed merger), inspect `main` against the lock's `main_at_acquire`, then `python3 .claude/lib/state.py merge-unlock` before relaunching.

   **merge-count:** merger owns the bump at step 7.5 via `state.py merge-count-bump`. Do NOT pre-bump via bookkeeper when inferring a merge from downstream evidence — wait for the notification or state-check. The merger's bump is authoritative. (Prior double-bump: coordinator pre-bumped on inferred merge, then merger's authoritative bump landed → off-by-one.)

   **merger lock:** check `python3 .claude/lib/state.py merge-lock-status` before any direct-to-main commit. `{"held": false}` → safe. `{"held": true, "stale": false}` → merger in flight, WAIT (will ff-advance main underneath you). `{"held": true, "stale": true}` → merger died mid-run — compare `.content.main_at_acquire` against current main to determine if ff landed, clear with `merge-unlock`, handle partial state (finish-from-step-5: ff landed but cleanup + dag-flip didn't). (Prose-only serialization failed three times before the lockfile.)

   **After each merge — re-check the frontier immediately.** Plans whose deps
   just cleared launch NOW, not after the queue drains. Mergers are serialized by
   `.claude/state/merger.lock` (one `.#ci` at a time); impls are parallel (own
   worktrees, own index). Saturate impl capacity continuously — a merge that
   clears a dep is a launch trigger, not a checkpoint to wait at.

   Common failure mode: treating the merge queue as a "phase gate" and holding
   new impls until it empties. The queue is a FIFO for serial merger work; it
   does not gate parallel impl work. Check `dag.jsonl` frontier after EVERY
   status change (merge, PARTIAL, new deps satisfied), not just at loop start.

5. **Mid-run requests — two kinds, routed by "does it commit?":**
   - **Steering a running agent** ("that P0120 agent is stuck, tell it to try X", "I disagree with P0109's approach, redirect it", "P0142's output is wrong, stop it") → `SendMessage` to that agent directly. No commit, no plan, no race. The agent_id is in `.claude/state/agents-running.jsonl`.
   - **Changing files on disk** ("fix the flags", "create an agent", "add a known-flake entry") → `/plan --inline '<json-array>'`. Writer creates a plan doc (tooling-batch kind for `.claude/`/`CLAUDE.md`/`flake.nix`), you schedule it. **Never spawn a generic worker that commits to main** — that's the `b4cd717` race.
     - Known-flake entries: **do not add the entry yourself.** Pass the flake details through `/plan --inline` — embed the KnownFlake fields (minus `fix_owner`) in the followup `description`:
       ```
       /plan --inline '[{"severity":"test-gap","proposed_plan":"P-new","description":"flake-fix: <test>. KnownFlake: {\"test\":\"<name>\",\"symptom\":\"<brief>\",\"root_cause\":\"<brief>\",\"fix_description\":\"<what>\",\"retry\":\"Once\"}"}]'
       ```
       Writer renders a `## Known-flake entry` section in the fix-plan doc (filling `fix_owner` with its placeholder; `/merge-impl` rewrites to the real number). The fix-plan impl reads that section, adds the entry via relative `python3 .claude/lib/state.py known-flake` in its worktree, commits it alongside the fix — entry and fix merge atomically. **No live bridge**: the entry is on main only after the fix merges, by which time the fix is in. Flake fixes should be fast (and the `fix_owner: P<N>` validator already forces the plan to exist first). If bridging matters — slow fix, other agents keep hitting it — coordinator can manually `state.py known-flake` + commit before the fix-plan, but that's the exception, not the rule.

   If unsure which kind: ask "will this `git commit` anything?" Steering doesn't; file changes do.

6. **Regen.** After ~10 merges: refresh `.claude/collisions.jsonl` via `state.py collisions-regen` — catches newly-hot files the incremental appends missed.

6.5. **Cadence agents.** Counter bumped by merger at step 7.5 (reported in the merge notification). Check `c=$(cat .claude/state/merge-count.txt)`:
   - `c % 5 == 0` → spawn `rio-impl-consolidator` with the last 5 merges — duplication/weak-abstraction review. Writes followups with `origin: "consolidator"` (`severity: "feature"`; `CONSOLIDATION:` prefix is cosmetic).
   - `c % 7 == 0` → spawn `rio-impl-bughunter` with the last 7 merges — cross-plan bug patterns (smell accumulation, error-path coverage). Writes followups with `origin: "bughunter"` (`severity: "correctness"|"test-gap"`; `BUGHUNT:` prefix is cosmetic).
   ```bash
   # compute range for window W: (W+1)th-from-tip is the commit before the window
   since=$(git log --first-parent --format='%H' -$((W+1)) main | tail -1)
   # launch with: merge-count $c, range $since..main
   ```
   5 and 7 are coprime — both fire at merge 35, 70, … which is fine (one tick spawns both). Both write to `followups-pending.jsonl` with `discovered_from: None`. Both write a `severity: "trivial"` null-result marker when nothing found — cadence is visible either way. **Cadence proposals don't auto-flush via `/dag-tick`** — rows with `origin` in {`consolidator`, `bughunter`} are filtered out of the >15-row threshold and the P-new trigger (a consolidator `P-new` would otherwise insta-flush). They wait in the sink for coordinator review: promote via `/plan` when judged worthwhile; drop from sink if declined.

State lives in `.claude/state/` (gitignored scratch). Judgment stays with you.
