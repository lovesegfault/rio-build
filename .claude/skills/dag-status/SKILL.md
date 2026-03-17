---
name: dag-status
description: Show live phase execution state — which tracey markers are uncovered, which worktrees exist and how far ahead/behind the integration branch, what the current frontier is. Usage — /dag-status
---

## 1. Tracey uncovered

```bash
cd /root/src/rio-build/main
nix develop -c tracey query uncovered
```

Or use `mcp__tracey__tracey_uncovered` if the MCP daemon is registered. rio-build markers are subsystem-indexed (`gw.*`, `sched.*`, `store.*`, `worker.*`, `ctrl.*`, `obs.*`, `sec.*`, `proto.*`), NOT phase-indexed — so uncovered markers map to phases via the `### Tracey markers` task in each phase doc rather than directly.

## 2. Worktree state

```bash
cd /root/src/rio-build/main
BASE=$(git branch --show-current)
git worktree list --porcelain
```

For each worktree (excluding `main`):

```bash
cd <worktree-path>
ahead=$(git rev-list --count $BASE..HEAD)
behind=$(git rev-list --count HEAD..$BASE)
dirty=$([ -n "$(git status --porcelain)" ] && echo "DIRTY" || echo "clean")
echo "$(basename $(pwd)): ahead $ahead, behind $behind, $dirty"
```

## 3. Phase status from docs

```bash
grep -H 'Status:' /root/src/rio-build/main/docs/src/phases/phase*.md
```

Phases with `Status: COMPLETE` are done. Phases without a status line (or `## Milestone` unchecked) are the pending set.

## 4. Frontier from DAG.md

Read `docs/src/phases/DAG.md` — the dependency table. Cross-reference:

- Which pending phases have all dependencies marked COMPLETE (from step 3)?
- Which have no running worktree (from step 2)?
- Which have no running conflict-group-mate (from DAG.md's collision matrix)?

Those are the **ready-to-launch** set.

## 5. Compose status

| Phase | Status | Worktree | Ahead | Behind | Conflict group |
|---|---|---|---|---|---|
| 4a | COMPLETE | — | — | — | — |
| 4b | running | /root/src/rio-build/phase-4b-dev | 12 | 0 | gc-chain |
| 4c | pending | — | — | — | gc-chain (blocked by 4b) |

Then:

- **Ready to launch:** frontier phases with all deps COMPLETE, no running worktree, no running conflict-group-mate
- **Needs rebase:** any worktree >10 commits behind the integration branch
- **Stuck:** any worktree with 0 commits ahead and a running agent (agent might be spinning)
