---
name: merge-phase
description: Merge a completed phase branch into the integration branch with full safety protocol — rebase, ff-merge, .#ci + .#coverage-full gate, worktree cleanup, rebase broadcast to remaining worktrees. Usage — /merge-phase <branch-name>
---

Given branch name `$ARGUMENTS` (e.g., `phase-4b-dev`):

## 0. Identify the integration target

rio-build phases build on each other. The merge target is whatever the `/root/src/rio-build/main` worktree has checked out — check with `git -C /root/src/rio-build/main branch --show-current`. This might be `main`, or a running integration branch like `phase-4a-dev` if sub-phases are stacking.

## 1. Rebase the branch onto the integration target

```bash
cd /root/src/rio-build/$ARGUMENTS   # or wherever the worktree is
git fetch
git rebase <integration-target>
```

If conflicts: **stop**. Report the conflict files. Do NOT force-resolve — the user decides how to handle semantic conflicts.

## 2. ff-only merge into the integration target

```bash
cd /root/src/rio-build/main
git merge --ff-only $ARGUMENTS
```

If not fast-forward (shouldn't happen after a clean rebase): **stop**, investigate. Linear history is enforced; merge commits are not allowed.

## 3. Post-merge CI gate

```bash
nix-build-remote -- .#ci .#coverage-full
```

- **If RED:** launch `Agent(subagent_type="rio-ci-fixer", prompt=<log tail>)`. Do NOT proceed to cleanup. Report to the user — a merged-but-red integration branch is a fire.
- **If GREEN:** proceed.

## 4. Cleanup

```bash
git worktree remove /root/src/rio-build/$ARGUMENTS
git branch -d $ARGUMENTS
```

Only if the worktree path matches the standard layout; if it's somewhere else, remove by path.

## 5. Rebase broadcast

```bash
git worktree list
```

For each remaining worktree (excluding `main`), the integration branch has moved under them. If there are running agents in those worktrees, use `SendMessage` to tell each:

> <integration-target> advanced to `<new-hash>` — rebase when you reach a clean point.

Do NOT force-rebase their worktrees yourself — they may have uncommitted state.

## 6. Report

- New integration-target hash
- Which worktrees were notified
- Confirmation `.#ci` and `.#coverage-full` are green
