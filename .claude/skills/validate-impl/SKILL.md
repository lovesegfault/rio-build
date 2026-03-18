---
name: validate-impl
description: Launch rio-impl-validator for a completed impl. Reads the impl report, extracts deviations + tracey claims into a scrutiny seed, launches the verifier. Usage — /validate-impl <impl-output-path> OR /validate-impl <plan-number> (looks up agents-running.jsonl)
---

## 1. Resolve the impl report

Given `$ARGUMENTS` as either a path or a plan number:

**Path** (starts with `/` or contains `.output`): read it directly.

**Plan number** (bare number like `120`): look up `.claude/state/agents-running.jsonl` for the `role=impl` row with that plan:

```bash
python3 -c "
import sys
sys.path.insert(0, '.claude/lib')
from state import AgentRow, read_jsonl, STATE_DIR
for a in read_jsonl(STATE_DIR / 'agents-running.jsonl', AgentRow):
    if a.role == 'impl' and a.plan == 'P$ARGUMENTS':
        print(a.model_dump_json())
"
```

If `agent_id` is set, read `/tmp/claude-*/*/tasks/<agent_id>.output`. If `agent_id` is null (bootstrap): the impl data is in the worktree's git log and the `note` field; compose from there.

## 2. Extract scrutiny seed

From the impl report, pull:

**Commit hash** — the worktree HEAD. Goes in the verifier prompt.

**Deviations section** — grep for `## Deviations` or `### Deviations`. Each bullet becomes a scrutiny point: is the deviation spec-sanctioned (plan doc permits it) or a scope dodge (implementer avoided something hard)?

**Tracey coverage table** — any claim of "transitively covered" or "no direct `r[verify]`" or "covered by test X which also tests Y" → scrutiny point: trace the transitive path, prove the test actually reaches the code.

**Files changed** — compare against the plan doc's `## Files` section. Any file NOT in the doc's list → scrutiny point: why the extra file?

**Commit granularity** (batch plans) — if the plan doc has `### T<N>` headers, count them. Compare to commit count. `t_count >= 3 && c_count == 1` is a mega-commit smell; note it.

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

Use /nbr .#ci for any independent CI runs.
"""
)
```

## 4. Record the verify launch

Append a `role=verify` row with the new agent_id:

```bash
python3 .claude/lib/state.py agent-row \
  '{"plan":"P<N>","role":"verify","agent_id":"<agent-id>","worktree":"/root/src/rio-build/p<N>","status":"running","note":"launched from impl <impl-commit>"}'
```
