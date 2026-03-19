---
name: validate-impl
description: Launch rio-impl-validator for a completed impl. Reads the impl report, extracts deviations + tracey claims into a scrutiny seed, launches the verifier. Usage — /validate-impl <impl-output-path> OR /validate-impl <plan-number> (looks up agents-running.jsonl)
---

## 1. Resolve the impl report

Given `$ARGUMENTS` as either a path or a plan number:

**Path** (starts with `/` or contains `.output`): read it directly.

**Plan number** (bare number like `120`): look up `.claude/state/agents-running.jsonl` for the `role=impl` row with that plan:

```bash
.claude/bin/onibus state agent-lookup P$ARGUMENTS impl
```

If `agent_id` is set, read `/tmp/claude-*/*/tasks/<agent_id>.output`. If `agent_id` is null (bootstrap): the impl data is in the worktree's git log and the `note` field; compose from there.

## 2. Extract scrutiny seed

From the impl report, pull:

**Commit hash** — the worktree HEAD. Goes in the verifier prompt.

**Deviations section** — grep for `## Deviations`. Each bullet becomes a scrutiny point: is the deviation spec-sanctioned (plan doc permits it) or a scope dodge (implementer avoided something hard)?

**Tracey coverage table** — any claim of "transitively covered" or "no direct `r[verify]`" or "covered by test X which also tests Y" → scrutiny point: trace the transitive path, prove the test actually reaches the code.

**Files changed** — compute directly (don't rely on the impl report): `git diff $(.claude/bin/onibus integration-branch)..p<N> --name-only` from the worktree. Compare against the plan doc's `## Files` section. Any file NOT in the doc's list → scrutiny point: why the extra file?

**Commit granularity** (batch plans) — `.claude/bin/onibus merge atomicity-check p<N>` → read `.mega_commit` (bool) and `.abort_reason`. Non-null abort_reason → note as a pre-merge blocker.

## 3. Compose and launch

```
Agent(
  subagent_type="rio-impl-validator",
  cwd="/root/src/rio-build/main",
  prompt=f"""
Plan {N} — {title}.

Worktree: /root/src/rio-build/p{N} at commit {hash}.

Implementer's tracey claims:
{tracey_table}

Scrutinize:
{deviation_bullets}
{transitive_coverage_bullets}
{extra_file_bullets}
{commit_granularity_note if batch plan}

Use /nixbuild .#ci for any independent CI runs.
"""
)
```

## 4. Record the verify launch

Append a `role=verify` row with the new agent_id:

```bash
.claude/bin/onibus state agent-row \
  '{"plan":"P<N>","role":"verify","agent_id":"<agent-id>","worktree":"/root/src/rio-build/p<N>","status":"running","note":"launched from impl <impl-commit>"}'
```
