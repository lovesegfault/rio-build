---
name: check
description: Fast local checks — clippy + fmt. Skips nextest + VM tests. Use during the edit loop; let the full checks gate catch the rest before merge.
---

Run, in order:

1. `nix develop -c treefmt` — format everything (idempotent; if it changes files, stage them)
2. `nix-fast-build --flake '.#checks.x86_64-linux.{clippy-rio-common,clippy-rio-<touched-crate>,...}'` — clippy on the crates you touched (or omit the brace-set to lint all members)
3. `nix develop -c cargo check --workspace` — fast syntax check

For fast VM-test python validation (~10s, no VM boot, just mypy+pyflakes):

```bash
nix build .#checks.x86_64-linux.vm-<name>.driverInteractive
```

Report results tersely — pass/fail per step, first error if any failed.

If the user asks for "full" checks, use `/nixbuild --checks` (all ~110 granular checks via nix-fast-build — VM tests, fuzz, ~20min).
