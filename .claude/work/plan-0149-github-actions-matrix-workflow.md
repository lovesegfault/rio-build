# Plan 0149: GitHub Actions matrix workflow + codecov thresholds

## Design

Prior to phase 4a, CI ran as a single monolithic `nix flake check` invocation — all checks, all VM tests, all fuzz targets in one opaque job. Failures surfaced as a single red X with no per-target granularity; the CI gate was fragile to any single flake; and Codecov uploads were unstructured.

This plan introduced a `.#githubActions` flake output that the workflow evaluates to generate matrices dynamically. The policy for "what runs in CI" now lives entirely in Nix — adding a VM test or cargo check is zero-touch on the workflow side. The workflow has a `gen-matrix` job that emits JSON via `nix eval .#githubActions.matrix.*`, and downstream jobs (`checks`, `fuzz`, `coverage`, `vm-test`, `vm-coverage`) consume those matrices. A single `ci-gate` fan-in job (via `alls-green`) is the one stable required check for branch protection.

The iteration across 20 commits reflects the usual CI-workflow friction: ARC container quirks (no `python` symlink, `enable-kvm` breaking in containers), JUnit upload plumbing attempted and then abandoned (`62baef8` — "too much crane plumbing"), progress-bar noise in nextest output, and the k3s airgap import gate (`51dde5a`). The end state: per-target red/green, 60-minute timeouts, niks3 skip-on-fork-PR when OIDC unavailable, and `codecov.yml` with explicit thresholds and trimmed PR comments.

## Files

```json files
[
  {"path": "codecov.yml", "action": "NEW", "note": "explicit thresholds, trimmed comment, wait_for_ci"},
  {"path": "flake.nix", "action": "MODIFY", "note": "githubActions output via legacyPackages alias"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "k3s airgap import gate before pod waits"}
]
```

## Tracey

No tracey markers — pure CI infrastructure, no spec behaviors.

## Entry

- Depends on P0148: phase 3b complete

## Exit

Merged as `1c017b9..86e3699` + `8eb304c` + `275df15` (20 commits). `.#githubActions.matrix.*` evaluates; workflow matrix jobs fan out correctly; `ci-gate` passes on green. Commits `d661fce` and `fe61c6a` (phase4 → 4a/4b/4c doc split) land in this window but are phase-doc maintenance, not code.
