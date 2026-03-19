---
name: implement
description: Launch a rio-build plan implementation agent. Reads the plan doc, extracts file refs and spec pointers, composes a minimal prompt, and launches rio-implementer in a named worktree. Usage — /implement <N>
---

Given plan number `$ARGUMENTS`:

## 1. Read the plan doc

```bash
ls .claude/work/plan-$(printf '%04d' $ARGUMENTS)-*.md
```

Use Glob to find the exact filename. Read it in full.

## 2. Extract plan-specific refs

From the plan doc, extract:

- **File paths** — primary surgery sites:
  ```bash
  grep -oE 'rio-[a-z-]+/src/[a-z_/]+\.rs(:[0-9]+)?' <plan-doc> | sort -u
  ```
- **Component spec refs** — `docs/src/components/*.md` pointers (rio-build's behavior authority):
  ```bash
  grep -oE 'docs/src/components/[a-z-]+\.md(#[a-z-]+)?' <plan-doc> | sort -u
  ```
- **Exit criteria** — bullets under `## Exit criteria` (the done-definition)
- **Tracey markers** — domain markers referenced (NOT `r[plan.*]` — rio-build is domain-indexed):
  ```bash
  python3 .claude/lib/state.py tracey-markers <plan-doc>
  ```

## 3. Live collision check against running worktrees

The static dag.jsonl matrix is computed at regen time and only covers hot files above a threshold. Plans can collide on files the matrix doesn't track.

```bash
python3 .claude/skills/implement/collision_check.py $ARGUMENTS
```

Output is JSON (`CollisionReport` — `--schema` for the contract). Running-worktree side uses `git diff --name-only main..HEAD` (ground truth); this-plan side greps the doc (best-effort).

- `source: "none"` → doc has no fence and grep found nothing; check was a no-op for this plan. Report that — not the same as "no collisions".
- `collisions: [...]` → each has `branch` + `files`. Include in the prompt.

Also read collisions.jsonl for append-vs-replace semantics — the live check finds collisions, the static matrix tells you how to resolve them at rebase. Include any collision findings in the prompt as a note.

## 4. Compose the prompt

**Only plan-specific content.** The worktree protocol, tracey protocol, commit protocol, and verification gate are all baked into `rio-implementer`'s system prompt — do NOT repeat them.

```
Plan <N>: <title from doc>

Primary files:
  <extracted file paths, one per line>

Component spec refs:
  <extracted docs/src/components/ paths, one per line>

Tracey markers to cover (domain-indexed):
  <extracted r[domain.*] markers>

<Conflict note if applicable>

<Any plan-specific gotchas from the doc — e.g., "this plan touches
 a type that P<X> renames; check main before writing">
```

## 5. Launch

```
Agent(
  subagent_type="rio-implementer",
  name="p$ARGUMENTS",
  cwd="/root/src/rio-build/main",
  run_in_background=true,
  prompt=<composed>
)
```

## 6. Record

```bash
python3 .claude/lib/state.py agent-row \
  '{"plan":"P'$ARGUMENTS'","role":"impl","agent_id":"p'$ARGUMENTS'","worktree":"/root/src/rio-build/p'$ARGUMENTS'","status":"running","note":"<collision-group-from-step-3>"}'
```
