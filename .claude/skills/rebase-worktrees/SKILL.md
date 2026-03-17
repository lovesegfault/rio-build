---
name: rebase-worktrees
description: Rebase all running phase worktrees onto the current integration branch. Skips worktrees with uncommitted changes. Usage — /rebase-worktrees
---

## 1. Enumerate worktrees

```bash
cd /root/src/rio-build/main
BASE=$(git branch --show-current)   # the integration branch (may be main or a phase-*-dev)
git worktree list --porcelain
```

## 2. For each worktree path (excluding main)

```bash
cd <path>

# Dirty? Skip.
if [ -n "$(git status --porcelain)" ]; then
  echo "SKIP $(basename <path>): uncommitted changes"
  continue
fi

# Already current? Skip.
behind=$(git rev-list --count HEAD..$BASE)
if [ "$behind" -eq 0 ]; then
  echo "SKIP $(basename <path>): already current"
  continue
fi

# Rebase.
git rebase $BASE
if [ $? -ne 0 ]; then
  git rebase --abort
  echo "CONFLICT $(basename <path>): rebase aborted"
  continue
fi
echo "REBASED $(basename <path>): now at $(git rev-parse --short HEAD)"
```

## 3. Report

| Worktree | Outcome |
|---|---|
| phase-4b-dev | rebased clean → `a1b2c3d` |
| fix-21b | skipped: uncommitted changes |
| phase-4c-dev | **conflict: aborted** — files: `<list>` |
| fix-refscan-test | skipped: already current |

## 4. Notify running agents

If any running agents are in rebased worktrees, note that **their working tree just changed under them** — they should re-read files before continuing. Use `SendMessage` if agents are active.
