---
name: dag-status
description: Show live DAG execution state — which tracey markers are uncovered, which worktrees exist and how far ahead/behind main, what the current frontier is. Usage — /dag-status
---

## 1. Tracey uncovered

Use `mcp__tracey__tracey_uncovered` (or `tracey query uncovered` via Bash). rio-build tracey is **domain-indexed** (`r[gw.*]`, `r[sched.*]`, `r[store.*]`, etc.) — these live in `docs/src/components/*.md`, not plan docs. Uncovered markers are spec requirements with no `r[impl ...]` annotation.

## 2. Worktree state

```bash
cd /root/src/rio-build/main
git worktree list --porcelain
```

For each worktree (excluding the coordinator's):

```bash
TGT=$(python3 .claude/lib/state.py integration-branch)
cd <worktree-path>
ahead=$(git rev-list --count $TGT..HEAD)
behind=$(git rev-list --count HEAD..$TGT)
echo "$(basename $(pwd)): ahead $ahead, behind $behind"
```

## 3. Frontier from dag.jsonl

Compute the frontier via `state.py dag-render` (stderr emits the frontier list):

```bash
python3 .claude/lib/state.py dag-render 2>&1 >/dev/null | grep frontier
```

Or inline via `_compute_frontier()`. Cross-reference:

- Which frontier plans have no running worktree (from step 2)?
- Which have no running conflict-group-mate (from `.claude/collisions.jsonl` — each row is `{path, plans, count}`)?

Those are the **ready-to-launch** set.

## 4. Compose status

| Plan | Status | Worktree | Ahead | Behind | Conflict group |
|---|---|---|---|---|---|
| p134 | running | /root/src/rio-build/p134 | 3 | 0 | scheduler-actor |
| p135 | uncovered | — | — | — | scheduler-actor (blocked by p134) |
| p076 | uncovered | — | — | — | singleton (ready) |

Then:

- **Ready to launch:** list of frontier plans with no deps, no running conflict-group-mate
- **Needs rebase:** any worktree >10 commits behind `$TGT`
- **Stuck:** any worktree with 0 commits ahead and a running agent (agent might be spinning)
