---
name: impl-phase
description: Launch a rio-build phase implementation agent. Reads the phase doc, extracts file refs and spec pointers, composes a minimal prompt, and launches rio-phase-impl in a named worktree. Usage — /impl-phase <ID>
---

Given phase ID `$ARGUMENTS` (e.g., `4b`, `2c`):

## 1. Read the phase doc

```bash
cat docs/src/phases/phase$ARGUMENTS.md
```

If no exact match, check for umbrella docs (`phase4.md` vs `phase4a.md`) — the sub-phase docs (`a`/`b`/`c`) are the implementable units; umbrella docs (`phase3.md`, `phase4.md`) are overviews.

## 2. Extract phase-specific refs

From the phase doc, extract:

- **File paths** — primary surgery sites:
  ```bash
  grep -oE 'rio-[a-z-]+/src/[a-z_/]+\.rs(:[0-9]+)?' docs/src/phases/phase$ARGUMENTS.md | sort -u
  grep -oE 'nix/tests/[a-z_/]+\.nix(:[0-9]+)?' docs/src/phases/phase$ARGUMENTS.md | sort -u
  grep -oE 'migrations/[0-9]+[a-z_]*\.sql' docs/src/phases/phase$ARGUMENTS.md | sort -u
  ```
- **Component spec refs** — from the `**Implements:**` line (e.g., `[Observability](../observability.md)`). These hold the `r[...]` markers.
- **Milestone** — the `## Milestone` section (done-definition — usually a nextest pass + VM test target name).
- **Tracey markers** — from the `### Tracey markers` task:
  ```bash
  grep -oE '(gw|sched|store|worker|ctrl|obs|sec|proto)\.[a-z.-]+' docs/src/phases/phase$ARGUMENTS.md | sort -u
  ```
- **Dependency note** — the `> **Depends on Phase X:**` blockquote if present.

## 3. Check DAG.md for conflicts

Read `docs/src/phases/DAG.md` — find this phase's conflict group in the collision matrix. If other frontier phases touch the same files, include a note: `"Conflict group: shares <file> with <other-phase> — expect rebase when merging."`

## 4. Compose the prompt

**Only phase-specific content.** The worktree protocol, tracey protocol, commit protocol, TODO tagging, and verification gate are all baked into `rio-phase-impl`'s system prompt — do NOT repeat them.

```
Phase <ID>: <title from doc>

Primary files:
  <extracted rio-*/src/*.rs paths, one per line>
  <extracted nix/tests/*.nix paths>
  <extracted migrations/*.sql>

Component specs (hold the r[...] markers):
  <extracted **Implements:** link targets>

Tracey markers to cover:
  <extracted domain.area.detail IDs>

Milestone:
  <copied ## Milestone section>

<Dependency note if applicable>

<Conflict note if applicable>

<Any phase-specific gotchas — e.g., "migration 009 is chained across
 4a/4b/4c as Part A/B/C; this phase adds Part B only">
```

## 5. Launch

```
Agent(
  subagent_type="rio-phase-impl",
  name="phase-$ARGUMENTS",
  cwd="/root/src/rio-build/main",
  run_in_background=true,
  prompt=<composed>
)
```

## 6. Record

Add to your running-agents table:

| Phase | Agent | Worktree | Conflict group |
|---|---|---|---|
| $ARGUMENTS | phase-$ARGUMENTS | /root/src/rio-build/phase-$ARGUMENTS-dev | <from step 3> |
