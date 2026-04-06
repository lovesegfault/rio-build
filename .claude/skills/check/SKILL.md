---
name: check
description: Fast local checks — clippy + fmt. Skips nextest + VM tests. Use during the edit loop; let .#ci catch the rest before merge.
---

Builds run locally (this host handles x86_64+aarch64 KVM directly; nxb-dev retired 2026-03).

Run, in order:

1. `nix develop -c treefmt` — format everything locally (idempotent; if it changes files, stage them). This one is safe locally — it's just formatting, no eval.
2. `/nixbuild .#checks.x86_64-linux.clippy` — clippy with `--deny warnings` (remote)
3. `nix develop -c cargo check --workspace` — local syntax check (safe — no nix eval)

The `.driverInteractive` attribute is also safe to build remotely for fast VM-test python validation (~10s, no VM boot, just mypy+pyflakes):

```bash
nix-build-remote --no-nom --dev -- .#checks.x86_64-linux.vm-<name>.driverInteractive
```

Report results tersely — pass/fail per step, first error if any failed.

If the user asks for "full" checks, use `/nixbuild .#ci` (all VM tests, fuzz, coverage — ~20min).
