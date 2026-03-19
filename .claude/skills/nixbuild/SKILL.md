---
name: nixbuild
description: Remote build via nix build --store ssh-ng://nxb-dev with structured log lifecycle. Use for ALL .#ci / .#coverage-full runs. rio-build CANNOT nix-build locally (3 prior machine crashes) — this is the ONLY way to build.
---

## Invocation

```bash
.claude/skills/nixbuild/nixbuild.py <target>                    # e.g. .#ci
.claude/skills/nixbuild/nixbuild.py <target> --role verify      # validator/reviewer runs
.claude/skills/nixbuild/nixbuild.py <target> --copy             # pull output to local store
.claude/skills/nixbuild/nixbuild.py --rio-coverage <branch> <merged_at>   # merger step 6
.claude/skills/nixbuild/nixbuild.py <target> --loud             # debugging: tee output live
```

**Quiet by default.** Two lines: log path to stderr (tail it if impatient), `BuildReport` JSON to stdout at the end. No 3MB tee into agent context. `--loud` restores stream-to-stderr for interactive debugging. `--schema` for the contract. The script resolves `git rev-parse --show-toplevel` so `.#<target>` works from any subdirectory.

**Callers jq.** Process exit is always 0 — `rc` is in the JSON. `report=$(nixbuild.py .#ci); rc=$(jq -r .rc <<<"$report")`.

## What it runs

```
nix build --no-link --eval-store auto --store ssh-ng://nxb-dev -L <target>
```

`--eval-store auto` because the flake source is on local disk. `--store ssh-ng://nxb-dev` builds on the fleet (ssh_config `Host nxb-dev` + wildcard `User root`/`Port 2222` resolve the HA address). `--no-link` because the output lands remote — a local `result/` symlink would dangle.

Supersedes the `nix-build-remote` wrapper. `nix build --store` is atomic: either succeeds (output exists remote) or fails. No more "dispatch died silently, rc=0 but invalid output" failure mode.

With `--copy`, after a green build:

```
nix copy --no-check-sigs --from ssh-ng://nxb-dev <outpaths>
```

`store_path` is the first outpath from `--print-out-paths` (single-output derivations; `.#coverage-full` is single-output with subdirectories inside).

## `--rio-coverage <branch> <merged_at>`

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
