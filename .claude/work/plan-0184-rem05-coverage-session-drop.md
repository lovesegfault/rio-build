# Plan 0184: Rem-05 — Coverage session drop 15→1 (codecov after_n_builds)

## Design

**P0 dev-signal only (no runtime impact).** Codecov was receiving 15 lcov uploads per CI run (1 unit + 14 VM) but retaining only 1. With 15 uploads arriving within a ~2s window after `ci-gate` fires, Codecov's report processor raced itself and last-writer-wins. All GHA jobs green; Codecov showed 46.6% coverage (whichever single fragment landed).

The existing `wait_for_ci: true` in `codecov.yml` waits for GitHub's commit-status to go green; it does NOT wait for Codecov's own upload-ingest pipeline. `after_n_builds` is the only knob that gates on upload count server-side.

Ground truth measured: `nix eval .#githubActions.matrix.coverage --json | jq length` → 15. The remediation doc's §1.5 said "6 unit + 9 VM" — wrong on the split (correct on the total). `codecov.yml:3` also had a stale "Nine coverage flags upload per PR" comment.

Remediation doc: `docs/src/remediations/phase4a/05-coverage-session-drop.md` (226 lines, shortest remediation).

## Outcome

**Fix rode inside docs commit `a335485`** — subject line says "docs(remediations): add phase4a/18-ssh-hardening plan" but the commit also touches `codecov.yml` and `flake.nix`. The codecov change is invisible in the subject. Commit body does not mention rem-05. This is the only remediation whose fix is not visible in any commit subject line.

## Files

```json files
[
  {"path": "codecov.yml", "action": "MODIFY", "note": "after_n_builds: 15; stale comment fixed"},
  {"path": "flake.nix", "action": "MODIFY", "note": "coverage matrix count comment"}
]
```

## Tracey

No tracey markers — CI infrastructure.

## Entry

- Depends on P0149: GitHub Actions matrix workflow (the 15-upload structure)

## Exit

Merged as `5e8f7a7` (plan doc) + `a335485` (fix, mixed commit — also added rem-17 doc). `.#ci` green. Codecov now waits for all 15 uploads; reports consistent ~93% coverage.
