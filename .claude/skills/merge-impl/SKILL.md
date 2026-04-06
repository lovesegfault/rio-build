---
name: merge-impl
description: Merge a completed plan branch into main. Runs atomicity check first (cheap, skill-layer — don't spawn the merger just to abort at step 2). Then launches rio-impl-merger (read-only — rolls back on CI failure, doesn't patch). Usage — /merge-impl <branch-name>
---

Given branch name `$ARGUMENTS` (e.g., `p142`):

## 0a. Assign placeholders (docs branches)

```bash
.claude/bin/onibus merge rename-unassigned $ARGUMENTS
```

Output is `RenameReport` JSON. If `mapping` is empty: no placeholders on this branch (impl branches, or docs branches with only P-batch appends) — no-op. If non-empty: the script already sed'd every `9<runid><NN>` → `NNN`, renamed files, committed `docs: assign plan numbers …`. The branch now has no placeholders. Idempotent; safe to re-run.

## 0b. Atomicity precondition

```bash
.claude/bin/onibus merge atomicity-check $ARGUMENTS
```

Output is JSON (`AtomicityVerdict` — `--schema` for the contract). Read `abort_reason`:

- `null` → proceed to step 1
- `"mega-commit"` → `t_count >= 3 && c_count == 1` (batch collapsed). **Don't launch merger.** Report back: impl agent needs `git reset --soft $(.claude/bin/onibus integration-branch)` in the worktree, re-commit per-T-item using the convco types the writer embeds in T-headers.
- `"chore-touches-src"` → `chore_violations` has each sha + the src files. **Don't launch merger.** Report back: impl agent needs `git rebase -i` and reword to `fix:`/`refactor:`/`perf:`.

The JSON has the full file lists — include it in your report so the impl agent has what they need without re-running.

## 1. Launch merger

```
Agent(
  subagent_type="rio-impl-merger",
  name="merge-$ARGUMENTS",
  cwd="/root/src/rio-build/main",
  run_in_background=false,
  prompt="Branch: $ARGUMENTS"
)
```

Foreground — merges are serialization points.

## 2. Act on the report

The merger emits a fenced ```json `MergerReport` block (`state.MergerReport` — `.claude/bin/onibus schema MergerReport`). Parse it; match on `report.status` and `report.abort_reason`.

### `report.status == "merged"`

**`report.dag_delta_commit`** — merger step 7.5 amended the dag.jsonl flip into the plan's last commit (== `report.hash`). No separate log noise. `"already-done"` if the row was already DONE pre-merge.

**`report.cov_log`** — coverage is backgrounded (non-gating). Merger step 6 writes a `CoverageResult` row to `coverage-pending.jsonl` via onibus; `/dag-tick` consumes it (surfaces in `TickReport.coverage_regressions`) and appends a follow-up if `exit_code != 0`.

**clause4 fast-path** — if `report.failure_detail` contains `"clause-4 SKIP"`, the merger skipped `.#ci` entirely (drv-hash identical to last-green — nothing observable changed). This is expected for docs-only / `.claude/`-only merges post-P0304-T29 `fileset.difference`. The merger still calls `onibus merge record-green` to refresh the baseline.

**`report.behind_worktrees`** — informational. Impls self-rebase at their verification-gate step 0; you don't broadcast.

**`report.stale_verify_commits_moved > 3`** — branch rebased >3 commits at merge time, verify PASS examined older code. `.#ci` passed so syntactic/test regressions are ruled out. Your judgment: accept (most merges), or re-verify on the integration branch retroactively if the rebase magnitude worries you.

### `report.status == "aborted"` — match on `report.abort_reason`

| `abort_reason` | Action |
|---|---|
| `"clause4-halt"` | `onibus merge clause4-check` found new `#[test]` attrs that are RED. Merge rolled back; `queue-halted` sentinel written. **Do NOT relaunch** — the queue is halted. Root-cause (the new test is either a real regression the impl missed, or a broken test) then `onibus merge clear-halt`. `report.failure_detail` has the test names + nextest tail. |
| `"ci-failed"` | Merge rolled back; integration branch at pre-merge hash. `report.failure_detail` is a log tail — `rio-ci-fixer`'s expected input. Either launch ci-fixer against a throwaway worktree, or send the branch back to impl with the log tail. |
| `"rebase-conflict"` | `report.failure_detail` has the conflicting files. Send back to impl: resolve in the worktree, commit, re-run `/merge-impl`. |
| `"non-convco-commits"` | `.pre-commit-config.yaml` symlink was missing; commits bypassed convco. Impl agent needs `git rebase -i $(.claude/bin/onibus integration-branch)` to reword. |
| `"ff-rejected"` | Investigate — `ff-rejected` after clean rebase means the integration branch moved mid-merge (race?). |
| `"worktree-missing"` \| `"already-merged"` | Investigate — `already-merged` means someone got there first. |
