---
name: nbr
description: Run nix-build-remote with correct flags, structured log lifecycle, and optional --copy. Use for ALL .#ci / .#coverage-full runs. rio-build CANNOT nix-build locally (3 prior machine crashes) — this is the ONLY way to build.
---

## Invocation

```bash
python3 .claude/skills/nbr/nbr.py <target>                    # e.g. .#ci, .#coverage-full
python3 .claude/skills/nbr/nbr.py <target> --role verify      # validator/reviewer runs
python3 .claude/skills/nbr/nbr.py <target> --copy             # merger coverage artifacts
python3 .claude/skills/nbr/nbr.py <target> --loud             # debugging: tee output live
```

**Quiet by default.** Two lines: log path to stderr (tail it if impatient), `NbrReport` JSON to stdout at the end. No 3MB tee into agent context. `--loud` restores the old stream-to-stderr behavior for interactive debugging. `--schema` for the contract. The script resolves `git rev-parse --show-toplevel` so `.#<target>` works from any subdirectory.

## Logs

`/tmp/rio-dev/rio-{plan}-{role}-{iter}.log` — always kept, green or red.

| Component | Source |
|---|---|
| `{plan}` | branch name: `p214` → `p0214` (zero-pad); non-`pNNN` branches kept verbatim (`docs-171650`, `nbr-redesign`) |
| `{role}` | `--role {impl,verify,merge,writer}`, default `impl` |
| `{iter}` | auto-increments per `(plan, role)` pair — `rio-p0214-impl-1.log`, `rio-p0214-impl-2.log`, … |

History is discoverable: `ls /tmp/rio-dev/rio-p0214-*` shows every run for that plan.

## Output

Read `rc` from the JSON. No shell exit-code plumbing — the subprocess rc is the model field. `log_path` is always set.

| `rc` | Meaning |
|---|---|
| `0` | Green — `log_tail` empty, log kept for reference |
| `1` | Build failed — `log_tail` has last 80 lines |
| `3` | `nix-build-remote` exited 0 but `nix path-info --store ssh-ng://nxb-dev <target>` rejects it — dispatch died silently, output invalid |

`rc=3` is the hardening from rix P172: `nix-build-remote` can exit 0 when the dispatch dies mid-flight (998-byte log, "don't know how to build", no valid output). Builds happen on nixbuildnet — local store never sees the output, so the check must query the remote: `nix path-info --eval-store auto --store ssh-ng://nxb-dev <target>` (ssh_config `Host nxb-dev` + wildcard `User root`/`Port 2222` resolve the fleet HA address). `--eval-store auto` because the flake source is local disk. The script appends an `[nbr] FAIL:` line to the log so `log_tail` surfaces it.

**`log_bytes`** is the build-log size captured at `proc.wait()` (before verification appends). A real `.#ci` run is ~100KB+. `log_bytes < 5000` with `rc=0` is the smell — check even if rc says green.

## Why each flag

| Flag | Purpose |
|---|---|
| `--dev` | Dev-mode remote builder selection |
| `--no-nom` | Skip nix-output-monitor — faster, no TTY garbage in logs |
| `--` | End of nix-build-remote's own args |
| `-L` | Passed to `nix build` — print full build logs (critical when CI red) |
| `--copy` | Pull outputs from the fleet to local store (coverage lcov, etc.) |
