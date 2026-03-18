---
name: bump-refs
description: Bump flake inputs in lockstep. rio-build has no reference submodules (rio-build is the ORIGINAL implementation, not a port) — this just wraps `nix flake update` for the inputs that matter.
---

## Default: bump nixpkgs + tracey to latest

```bash
nix flake update nixpkgs
nix flake update tracey-src  # tracey source is a flake input (IFD-free since c1d6a05)
git add flake.lock
```

Then show the user the flake.lock diff. Do **not** commit — the user reviews and commits.

## With a specific input argument (`/bump-refs nixpkgs`)

```bash
nix flake update $ARGUMENTS
git add flake.lock
```

Show the diff. Do not commit.

## Post-bump

Remind the user that the next `nix develop` / build will re-fetch inputs and may rebuild. The crane `cargoArtifacts` cache survives.

**tracey bump extra step:** if `tracey-src` was bumped and `pnpmDeps` hash changed, one `fakeHash` roundtrip is needed — see memory `tracey-adoption.md`.
