---
name: check
description: Fast local checks — clippy + fmt. Skips nextest + VM tests. Use during the edit loop; let .#ci catch the rest before merge.
---

Run, in order:

1. `nix develop -c treefmt` — format everything (idempotent; if it changes files, stage them)
2. `/nixbuild .#checks.x86_64-linux.clippy` — clippy with `--deny warnings`
3. `nix develop -c cargo check --workspace` — fast syntax check

For fast VM-test python validation (~10s, no VM boot, just mypy+pyflakes):

```bash
nix build .#checks.x86_64-linux.vm-<name>.driverInteractive
```

Report results tersely — pass/fail per step, first error if any failed.

If the user asks for "full" checks, use `/nixbuild .#ci` (all VM tests, fuzz, coverage — ~20min).
