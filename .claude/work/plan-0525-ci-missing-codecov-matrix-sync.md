# Plan 525: `.#ci` omits codecov-matrix-sync — the tooling that let after_n_builds drift through

[`ciBaseDrvs` at flake.nix:1267-1280](../../flake.nix) is a MANUAL list covering 44 of 45 checks. [`codecov-matrix-sync` at :2194](../../flake.nix) is the one missing check — an eval-time assertion that `.github/codecov.yml` `after_n_builds` equals the coverage-matrix length. It's IN `checks.x86_64-linux` (so `nix flake check` catches drift), but NOT in `ciBaseDrvs` (so `.#ci` — the merge gate — does not).

[CLAUDE.md:63](../../CLAUDE.md) says `.#ci` "bundles all checks". False in code. The [P0519](plan-0519-exclude-prod-parity-cov.md) reviewer found this via constituent-level diff (`ls result/` of `.#ci` vs `nix flake show`).

**Concrete cost:** the `after_n_builds=19` drift (matrix had 21) sat undetected for two commits ([`84471a1f`](https://github.com/search?q=84471a1f&type=commits) + [`78b7406f`](https://github.com/search?q=78b7406f&type=commits) each added a vm-* fragment). `nix flake check` WOULD have caught it instantly. `.#ci` did not. Coordinator hand-patched at [`ee957551`](https://github.com/search?q=ee957551&type=commits) post-verify.

**sprint-1-always-green policy:** break = fix the issue AND the tooling that let it through. [`ee957551`](https://github.com/search?q=ee957551&type=commits) fixed the issue. **This plan fixes the tooling.** Also explains why the P0519 validator's stale-outpath concern was more than hygiene — even a fresh `.#ci` run wouldn't have seen this.

discovered_from=519. origin=reviewer.

## Tasks

### T1 — `fix(ci):` add codecov-matrix-sync to ciBaseDrvs OR derive ci from checks attrset

Two routes. **Route B preferred** — structural fix, closes the class.

**Route A (minimal):** append to `ciBaseDrvs`:

```nix
ciBaseDrvs = [
  rio-workspace
  # ... existing entries ...
  config.checks.pre-commit
] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
  # Eval-time after_n_builds assertion. MUST gate merge —
  # drift went 2 commits undetected before ee957551
  # because .#ci omitted this.
  checks.codecov-matrix-sync  # or self.checks.${system}.codecov-matrix-sync
];
```

`optionals isLinux` wraps because `codecov-matrix-sync` itself is `optionalAttrs pkgs.stdenv.isLinux` at [`:2193`](../../flake.nix) (depends on `githubActions`, which is Linux-only).

**Route B (structural):** derive from `checks` directly, making the manual list impossible to miss-sync:

```nix
# Derived — every check in checks.${system} is a CI constituent.
# Before ee957551, the manual ciBaseDrvs list had drifted to 44/45
# and the missing check (codecov-matrix-sync) was exactly the one
# guarding codecov.yml drift. Deriving from checks closes the class.
#
# builtins.attrValues is DETERMINISTIC (attr-name sorted) so
# linkFarmFromDrvs sees a stable input order → stable output hash.
ciCheckDrvs = builtins.attrValues self.checks.${system};

ci = pkgs.linkFarmFromDrvs "rio-ci" (
  ciCheckDrvs
  ++ builtins.attrValues fuzz.runs
  ++ pkgs.lib.optionals pkgs.stdenv.isLinux (
    builtins.attrValues vmTests
    ++ [ coverage.smoke ]
  )
);
```

**Route B caveats — verify at dispatch:**

1. **Check the current `checks` set doesn't include expensive attrs `.#ci` deliberately excludes.** The manual list was explicit; deriving is implicit. `nix flake show | jq '.checks'` → compare to `ciBaseDrvs`. The existing 44 covered should be a strict subset of what `attrValues` returns. If `checks` has debug/dev-only attrs (unlikely — that's what `packages` is for), filter them out with `removeAttrs`.

2. **vmTests overlap:** [`:2187`](../../flake.nix) `// vmTests` adds every VM test to `checks`. If Route B pulls `attrValues checks`, and `.#ci` ALSO adds `attrValues vmTests` at `:1286`, each VM test appears TWICE in the linkFarm input list. `linkFarmFromDrvs` dedupes by name (it's a derivation-symlink directory) so this is harmless — but it's surprising. Either drop the explicit `++ attrValues vmTests` (checks already has them) OR filter vmTests out of `ciCheckDrvs`:

   ```nix
   ciCheckDrvs = builtins.attrValues (
     # vmTests already added explicitly below — don't double-count
     removeAttrs self.checks.${system} (builtins.attrNames vmTests)
   );
   ```

3. **Non-Linux degradation:** `self.checks.${system}` on darwin won't have `codecov-matrix-sync` (optionalAttrs isLinux) or vmTests. `attrValues` of the reduced set is fine. Verify: the existing `ciBaseDrvs` entries ([`:1268-1279`](../../flake.nix)) are all cross-platform — `crateChecks.*`, `miscChecks.*`, `rioDashboard`, `config.checks.pre-commit`. Route B preserves this.

**Decision heuristic:** Route A is a 3-line fix. Route B is ~15 lines but prevents the 46th check from being the next gap. Prefer B unless the vmTests-overlap rework touches too much during a HOT week (`flake.nix` collision count = 47).

### T2 — `test(ci):` eval-time assert checks ⊆ ci constituents

Belt-and-suspenders for Route A (or independent check if Route B's derivation has a `removeAttrs` exclusion that could drift). Add alongside `codecov-matrix-sync`:

```nix
ci-covers-checks =
  let
    ciNames = map (d: d.name or d.pname or "<unnamed>") ciBaseDrvs;
    checkNames = builtins.attrNames (
      # VM tests are added separately at :1286
      removeAttrs checks (builtins.attrNames vmTests)
    );
    missing = builtins.filter (c: !(builtins.elem c ciNames)) checkNames;
  in
  assert pkgs.lib.assertMsg (missing == [ ]) ''
    .#ci doesn't include these checks: ${builtins.toJSON missing}
    Add to ciBaseDrvs (flake.nix:1267) or they won't gate merge.
    (codecov-matrix-sync went 2 commits undetected this way — ee957551.)
  '';
  pkgs.emptyFile;
```

**Route B makes T2 tautological** (attrValues checks → ci constituents is structural) — **skip T2 if Route B lands.** Include T2 only if Route A is chosen, OR if Route B's `removeAttrs` filter has ≥2 exclusions (at which point the exclusion list itself can drift).

## Exit criteria

- `/nbr .#ci` green
- `nix build .#ci && ls result/ | grep -q codecov-matrix-sync` → match (codecov-matrix-sync constituent present in the linkFarm)
- Route B: `nix eval .#ci.passthru.constituents --json 2>/dev/null | jq 'length'` ≥ 45, OR `diff <(nix flake show --json | jq -r '.checks."x86_64-linux" | keys[]' | sort) <(ls result/ | sort)` → checks fully covered (no checks missing from `result/`)
- Mutation: bump `after_n_builds` in `.github/codecov.yml` to wrong value → `nix build .#ci` FAILS with the assertMsg from [`:2203`](../../flake.nix)
- `grep 'bundles all checks\|all checks' CLAUDE.md` — claim now TRUE (no edit needed; this plan makes the code match the doc, not the other way)

## Tracey

No domain markers — CI-infra plumbing below the spec surface. `docs/src/components/` has no `r[ci.*]` domain and no marker covers "the merge gate includes check X". Closest neighbor is `r[obs.metric.*]` (observability infra), but this isn't a metric.

The `codecov-matrix-sync` check itself guards something spec-adjacent (`docs/src/remediations/phase4a/05-coverage-session-drop.md` per [`:2191`](../../flake.nix)) — but that's remediation-doc, not component-spec.

## Files

```json files
[
  {"path": "flake.nix", "action": "MODIFY", "note": "T1: :1267-1293 ciBaseDrvs — Route A adds 1 entry, Route B replaces manual list with attrValues checks. T2 (Route A only): new ci-covers-checks assert near :2194. HOT (count=47) — serialize with any in-flight flake.nix plan"}
]
```

```
flake.nix     # T1: :1267 ciBaseDrvs; T2 (conditional): assert near :2194
```

## Dependencies

```json deps
{"deps": [519], "soft_deps": [], "note": "P0519 is discovered_from — reviewer's constituent-diff surfaced the 44/45 gap. P0519 DONE; this plan ships independently. ee957551 fixed the symptom (after_n_builds 19→21); this fixes the class."}
```

**Depends on:** [P0519](plan-0519-exclude-prod-parity-cov.md) (DONE) — discovered_from; its reviewer's constituent-level diff found the gap.

**Conflicts with:** `flake.nix` count=47 — HOTTEST file in the repo. Route A is a 3-line append at `:1280`; Route B rewrites `:1267-1293`. Check `onibus collisions check <this-plan>` at dispatch. [P0304](plan-0304-trivial-batch-p0222-harness.md) T538 (removeAttrs assert at `:1224`) and T539 (dead cpuHints at `:1179`) touch the same file — DIFFERENT sections (`:1179`/`:1224` vs `:1267-1293`), clean rebase.
