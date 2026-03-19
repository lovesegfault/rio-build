---
name: review-impl
description: Launch rio-impl-reviewer for a PASS-verified plan. Post-PASS code-quality pass — smells, test-gaps, convention mismatches. Advisory; writes to followups sink, doesn't block merge. Usage — /review-impl <plan-number> [<validator-report-path>]
---

## When to invoke

After `rio-impl-validator` returns PASS. `/dag-tick` does this automatically (verify-PASS → append merge-queue AND spawn reviewer). Manual invocation for a one-off deeper look.

**Don't invoke on FAIL/PARTIAL/BEHIND** — no point reviewing code that's about to change.

## 1. Resolve the worktree

```bash
row=$(.claude/bin/onibus state agent-lookup P$1 verify)
worktree=$(jq -r '.worktree // empty' <<<"$row")
echo "${worktree:-/root/src/rio-build/p$1}"
```

## 2. Optional: extract validator prose notes

If `$2` (validator report path) is given: grep for prose after the `Recommendation:` line — anything the validator noted but didn't act on (it's narrowed to verdict-only now; smells go in prose hints).

## 3. Launch

```
Agent(
  subagent_type="rio-impl-reviewer",
  name="review-p<N>",
  cwd="/root/src/rio-build/main",
  run_in_background=true,
  prompt=f"""
Plan P<NNNN> — post-PASS review.

Worktree: <worktree-path>

<validator prose notes, if any>

Smell catalog, convention check, test-gap analysis. Write findings to
followups-pending.jsonl via onibus state followup P<NNNN>. Advisory — the
merge is already queued.
"""
)
```

## 4. Record

```bash
.claude/bin/onibus state agent-row \
  '{"plan":"P<N>","role":"review","agent_id":"<id>","worktree":"<worktree>","status":"running","note":"post-PASS review"}'
```
