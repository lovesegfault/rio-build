---
name: nixbuild
description: Local nix build with structured log lifecycle. Use for ALL .#ci / .#coverage-full runs. This host handles x86_64+aarch64 KVM builds directly (nxb-dev retired 2026-03).
---

## Invocation

```bash
.claude/bin/onibus build <target>                    # e.g. .#ci
.claude/bin/onibus build <target> --role verify      # validator/reviewer runs
.claude/bin/onibus build <target> --copy             # pull output to local store
.claude/bin/onibus build <target> --copy --link      # pull output + ./result symlink (for .#crds, .#coverage-html)
.claude/bin/onibus build --coverage <branch> <merged_at>   # merger step 6
.claude/bin/onibus build <target> --loud             # debugging: tee output live
```

**Quiet by default.** Two lines: log path to stderr (tail it if impatient), `BuildReport` JSON to stdout at the end. No 3MB tee into agent context. `--loud` restores stream-to-stderr for interactive debugging. `--schema` for the contract. onibus resolves `git rev-parse --show-toplevel` so `.#<target>` works from any subdirectory.

**Callers jq.** Process exit is always 0 — `rc` is in the JSON. `report=$(.claude/bin/onibus build .#ci); rc=$(jq -r .rc <<<"$report")`.

## What it runs

```
nix build --no-link --print-out-paths -L <target>
```

Local build — this host handles x86_64+aarch64 KVM directly. Outputs land in the local `/nix/store`. `--no-link` keeps the worktree clean; callers that want `./result` use `--link`.

`store_path` is the first outpath from `--print-out-paths` (single-output derivations; `.#coverage-full` is single-output with subdirectories inside). `--copy` is a no-op kept for API compat (outputs are already local).

With `--link`, creates `./result` → store path. Use for `.#crds` regen (`cp result/*.yaml infra/helm/...`) and `.#coverage-html` (`open result/index.html`). Without `--link`, `store_path` in the JSON is still the handle — `ln -sf $(jq -r .store_path) result` is the manual equivalent.

## `--coverage <branch> <merged_at>`

Composite mode for the merger's backgrounded coverage step. Internally fixes target=`.#coverage-full`, role=`merge`, copy=`True`, then writes a `CoverageResult` row to `.claude/state/coverage-pending.jsonl`. Prints the `CoverageResult` JSON that was written.

`branch` and `merged_at` are passed explicitly — coverage runs from `main/` after the ff-merge, so cwd branch is `main` (not the merged branch) and HEAD may move while backgrounded.

## Logs

`/tmp/rio-dev/rio-{plan}-{role}-{iter}.log` — always kept, green or red.

| Component | Source |
|---|---|
| `{plan}` | branch name: `p214` → `p0214` (zero-pad); non-`pNNN` branches kept verbatim (`docs-171650`) |
| `{role}` | `--role {impl,verify,merge,writer}`, default `impl` |
| `{iter}` | auto-increments per `(plan, role)` pair — `rio-p0214-impl-1.log`, `rio-p0214-impl-2.log`, … |

History is discoverable: `ls /tmp/rio-dev/rio-p0214-*` shows every run for that plan.

## Output

| `rc` | Meaning |
|---|---|
| `0` | Green — `log_tail` empty, log kept for reference |
| nonzero | Build or copy failed — `log_tail` has last 80 lines |

`store_path` is set when `--copy` succeeded. No `result/` symlink — `store_path` is the handle. `log_bytes` is the build-log size at `proc.wait()`; a real `.#ci` run is ~100KB+.
