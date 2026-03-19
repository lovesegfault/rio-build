---
name: fix-impl
description: Surgical-fix mode for verify FAILs. Relaunches rio-implementer in the existing ../p<N> worktree with only the FAIL/PARTIAL sections — patch-not-rebuild framing. No new agent type; same impl agent, different input shape. Usage — /fix-impl <path-to-verify-report> OR /fix-impl --inline
---

Given `$ARGUMENTS`:

## 1. Parse the plan number

**If `$ARGUMENTS` is a path**: read the report. Extract `<N>` from the VERDICT header or the worktree path. Verify reports reference the plan doc path, which has the number.

**If `$ARGUMENTS` is `--inline`**: the FAIL sections are in the conversation. Look for the plan number in the verdict header or the coordinator's framing.

If no plan number found: the report is malformed or it's not a verify report. Abort with "can't locate plan number — is this a verify report?".

## 2. Check the worktree exists

```bash
test -d /root/src/rio-build/p<N> && git -C /root/src/rio-build/p<N> rev-parse --is-inside-work-tree
```

If missing: the impl agent's worktree was cleaned up (merged, or the agent terminated and `/merge-impl` swept it). **Abort** — do not re-create. Report:

> Worktree `/root/src/rio-build/p<N>` gone. Either:
> - Branch `p<N>` still exists → `git worktree add ../p<N> p<N>` manually and re-run
> - Branch merged → the FAIL is against code now on main; this is a post-merge regression, not a fix-plan job — escalate

Fix mode is a *relaunch into existing state*. If the state is gone, it's not fix mode anymore.

## 3. Extract FAIL/PARTIAL sections

Verify reports use a fixed structure (see `rio-impl-validator.md` `## Report format`). Pull the actionable parts — `[ ]` unmet-criterion lines, unmatched tracey markers, smell flags — and drop the noise (`[x]` PASS lines, `matched:` lists):

```bash
# VERDICT line + unmet exit criteria + unmatched markers + smell flags
grep -E '^VERDICT:|^\s+\[ \]|^\s+unmatched:|^\s+.*:[0-9]+\s+' "$ARGUMENTS"
```

If `VERDICT: PASS` and no `[ ]`/`unmatched:`/smell lines: nothing to fix. Report "verdict PASS, no failures extracted" and stop.

## 4. Compose the fix-mode prompt

**Only the failures + framing.** Worktree protocol, tracey protocol, commit protocol, `.#ci` gate — all baked into `rio-implementer`. Do NOT repeat. The agent has a `### Fix mode` section in its system prompt; the framing cue triggers it.

```
Fix mode — plan <N>, worktree /root/src/rio-build/p<N> already exists.

rio-impl-validator reported:

<extracted FAIL/PARTIAL sections verbatim>

Patch, don't rebuild. Touch only what's listed above. PASS criteria are
already met — leave them alone. Re-run the specific failing checks
(cargo test <test-name>, grep for the missing marker) before the full
.#ci gate. Don't scope-creep into refactoring; if fixing reveals a
deeper problem, report it as a /plan follow-up.
```

## 5. Launch

```
Agent(
  subagent_type="rio-implementer",
  name="p<N>-fix",
  cwd="/root/src/rio-build/p<N>",
  run_in_background=true,
  prompt=<composed>
)
```

Note `cwd` is the **existing** worktree — no `git worktree add`.

## 6. Re-verify loop

After the fix agent reports complete, **re-launch `rio-impl-validator`** against the same worktree. The loop is verify → fix → verify until PASS. Do not `/merge-impl` off a fix-agent report; only merge off a verify PASS.

```bash
.claude/bin/onibus state agent-row \
  '{"plan":"P<N>","role":"impl","agent_id":"p<N>-fix","worktree":"/root/src/rio-build/p<N>","status":"running","note":"fix mode — re-verify on completion"}'
```
