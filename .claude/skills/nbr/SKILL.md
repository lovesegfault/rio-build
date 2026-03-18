---
name: nbr
description: Run nix-build-remote with correct flags, log lifecycle, and optional --copy. Use for ALL .#ci / .#coverage-full runs. rio-build CANNOT nix-build locally (3 prior machine crashes) — this is the ONLY way to build.
---

## Invocation

```bash
python3 .claude/skills/nbr/nbr.py <target>           # e.g. .#ci, .#coverage-full
python3 .claude/skills/nbr/nbr.py <target> --copy    # merger coverage artifacts
```

Streams build output to stdout (you see it live), tees to `/tmp/nbr-<branch>-<target>-XXXXXX.log`, then prints `NbrReport` JSON (`--schema` for the contract). Log deleted on green, kept on red. The script resolves `git rev-parse --show-toplevel` so `.#<target>` works from any subdirectory.

Read `rc` from the JSON. `rc == 0` → green. `rc != 0` → `log_path` has the full log, `log_tail` has the last 80 lines. No shell exit-code plumbing — the subprocess rc is the model field.

## Why each flag

| Flag | Purpose |
|---|---|
| `--dev` | Dev-mode remote builder selection |
| `--no-nom` | Skip nix-output-monitor — faster, no TTY garbage in logs |
| `--` | End of nix-build-remote's own args |
| `-L` | Passed to `nix build` — print full build logs (critical when CI red) |
| `--copy` | Pull outputs from the fleet to local store (coverage lcov, etc.) |

## rio-build constraint

**NEVER** `nix build` or `nix eval .#attr` locally — this has crashed the machine 3 times. ALWAYS use this skill (or raw `nix-build-remote --no-nom --dev -- -L`).
