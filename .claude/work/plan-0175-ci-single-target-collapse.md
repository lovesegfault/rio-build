# Plan 0175: CI .#ci single target — collapse fast/slow split + 2min fuzz

## Design

`ci-local-fast` / `ci-local-slow` / `ci-fast` / `ci-slow` → single `.#ci`. The four targets only differed by fuzz duration (30s/10min) and VM-test inclusion. With 2-minute fuzz (`0f9e77a`) and stabilized k3s scenarios (after P0176), a single target covers everything.

**Fuzz tier collapse (`0f9e77a`):** 30s smoke / 10min nightly → single 2min. The smoke tier was too short to find anything; nightly was too expensive for every PR. 2 minutes is enough to exercise the full corpus + a few hundred mutations. S3 corpus sync dropped — the checked-in seeds are the reproducible baseline; CI-grown corpus was never actually producing findings that the seeds didn't.

**Aggregate collapse (`89ba88f`):** Deleted `ciAggregates`, `mkCiAggregate`, `isK3sTest` filter. k3s scenarios are in the default set — no more fast/slow split. On non-Linux, `.#ci` degrades to cargo checks + pre-commit only (`vmTests` and `fuzz.runs` are both `optionalAttrs isLinux`). `ghMatrix`: `fuzz-smoke` → `fuzz`. `packages`: `fuzz.nightly` splice deleted. `checks`: `fuzz.smoke` → `fuzz.runs`.

**Doc sync (`acbe191`):** `.#ci-*` → `.#ci` in CLAUDE.md + contributing/verification/phase docs. **Helm `--set-string` fix (`6716d29`):** Helm coerces `'3'` to int, breaks `EnvVar` (which requires string); `--set-string` preserves type.

## Files

```json files
[
  {"path": "flake.nix", "action": "MODIFY", "note": "ciAggregates deleted; single .#ci; ghMatrix fuzz-smoke\u2192fuzz"},
  {"path": "nix/fuzz.nix", "action": "MODIFY", "note": "smoke/nightly \u2192 single 2min tier; S3 corpus sync dropped"},
  {"path": "docs/src/contributing.md", "action": "MODIFY", "note": ".#ci"},
  {"path": "nix/helm-render.nix", "action": "MODIFY", "note": "--set-string for extraSet (Helm int coercion)"}
]
```

## Tracey

No tracey markers — CI infrastructure.

## Entry

- Depends on P0149: GitHub Actions matrix workflow (this simplifies it)
- Depends on P0173: scenario tests (the fast/slow split was by fixture)

## Exit

Merged as `0f9e77a`, `89ba88f`, `acbe191`, `896c21e`, `6716d29` (5 commits). `.#ci` evaluates to `rio-ci.drv`; `.#ci-fast` errors with "does not provide attribute".
