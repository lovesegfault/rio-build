---
name: dag-status
description: Show live DAG execution state — which tracey markers are uncovered, which worktrees exist and how far ahead/behind main, what the current frontier is. Usage — /dag-status
---

## 1. Tracey uncovered

Use `mcp__tracey__tracey_uncovered` (or `tracey query uncovered` via Bash). rio-build tracey is **domain-indexed** (`r[gw.*]`, `r[sched.*]`, `r[store.*]`, etc.) — these live in `docs/src/components/*.md`, not plan docs. Uncovered markers are spec requirements with no `r[impl ...]` annotation.

## 2. Worktree + agent state

```bash
.claude/bin/onibus merge behind-report   # BehindReport — per-worktree behind count
.claude/bin/onibus state reconcile        # stale agent-rows (worktree gone), orphan worktrees (no row)
```

## 3. Ready-to-launch

```bash
.claude/bin/onibus dag launchable --parallel 10
```

Impact-sorted frontier plans with **no mutual file collision**. This IS the ready set — no manual cross-referencing. For the full frontier (including colliding ones): `.claude/bin/onibus dag frontier --json`. For impact ranking: `.claude/bin/onibus dag impact`.

## 4. Compose

| Signal | Query |
|---|---|
| Ready to launch | `launchable` output (already collision-free) |
| Needs rebase | any `behind-report` worktree with `behind > 10` |
| Stale agent rows | `reconcile.stale_rows` (worktree gone, row says running) |
| Orphan worktrees | `reconcile.orphan_worktrees` (worktree exists, no row — manual debug?) |
| Stuck agent | `reconcile.stuck_agents` (running row + worktree exists + zero commits ahead) |
