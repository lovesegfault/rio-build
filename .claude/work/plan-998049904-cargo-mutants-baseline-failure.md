# Plan 998049904: cargo-mutants baseline build failure — unblock weekly tier

User query (2026-03-20): `mutants.out` timestamp `01:48` shows `1 Failure, 0 caught/0 missed`. The [P0301](plan-0301-cargo-mutants-ci.md)-wired `.#mutants` derivation completes (the `|| true` at [`flake.nix:922`](../../flake.nix) swallows exit codes) but the mutation pass never runs — `cargo-mutants` runs a **baseline** build-and-test first (unmutated source, full test suite) and only proceeds if that passes. A baseline failure means `0 caught, 0 missed` regardless of how many mutations it would have found.

The `|| true` masks this: the derivation succeeds, `jq` on `outcomes.json` finds zero `CaughtMutant`/`MissedMutant` entries, and the weekly diff sees "0 → 0, no change." [P0304-T74](plan-0304-trivial-batch-p0222-harness.md) already tracks the exit-code-check fix (exit 2 = survived mutants, expected; anything else = real failure). This plan is the diagnostic half: WHY did the baseline fail?

**Likely causes (triage at dispatch):**

1. **Missing test-time environment.** `cargo-mutants` runs `cargo nextest run` ([`flake.nix:887-892`](../../flake.nix) notes PG/nix-cli/openssh are needed). If a recent plan added a test-time dep (e.g., a new VM fixture binary, a `fuser` dev-dependency, a hermetic-only env var), the nextest check would have it but the mutants derivation wouldn't.
2. **Test-scope regression in an `examine_globs` target.** The 7 scoped files at [`.config/mutants.toml`](../../.config/mutants.toml) — if one of them (or its test suite) has a test that passes in `.#ci` but fails in the mutants environment (different `$PWD`, missing fixture path, `NEXTEST_PROFILE` not set), the baseline catches it.
3. **`--in-place` mutation of a read-only crane source.** [`flake.nix:902-905`](../../flake.nix) says `--in-place` "mutate the unpacked source in $PWD." If crane's source unpacking leaves some directory read-only (e.g., vendored deps under `target/`), the baseline `cargo build` might fail on a write to a symlinked path. Less likely — the baseline doesn't mutate source — but `cargo-mutants --in-place` may still try to set up its sandbox.
4. **`cargoArtifacts` drift.** The derivation inherits [`cargoArtifacts`](../../flake.nix) at `:876` — the shared dep-cache. If a recent plan added a workspace dep that `cargoArtifacts` (from `buildDepsOnly`) doesn't include (race between dep-cache rebuild and mutants run), `cargo build` in the baseline phase recompiles from scratch and may hit a dep-resolution error if `Cargo.lock` drifted.

**Diagnostic path:** `nix build .#mutants --keep-failed` (or the remote equivalent via `/nixbuild .#mutants`) → inspect `result/mutants.out/debug.log` + `result/mutants.out/baseline.log` (cargo-mutants writes both). The baseline log shows the exact `cargo nextest run` stderr that failed.

## Entry criteria

- [P0301](plan-0301-cargo-mutants-ci.md) merged (`.#mutants` derivation + `.config/mutants.toml` exist)

## Tasks

### T1 — `fix(nix):` diagnose and fix mutants baseline failure

Run the diagnostic (via `/nixbuild .#mutants` — captures the 1-Failure output state) and inspect `result/mutants.out/debug.log` for the baseline failure cause. Then MODIFY [`flake.nix`](../../flake.nix) at the mutants derivation (`:873-938`) to address whichever root cause applies:

- **If missing env:** add the missing dep to `nativeBuildInputs` (`:893-900`), or set the missing env var in `buildPhaseCargoCommand` before `cargo mutants`.
- **If test-scope fixture path:** either fix the fixture reference in the source (absolute path via `env!` → relative to `CARGO_MANIFEST_DIR`), or set `RIO_FIXTURE_ROOT` / equivalent in `buildPhaseCargoCommand`.
- **If `cargoArtifacts` drift:** wire a fresh `buildDepsOnly` specific to mutants (with `cargoExtraArgs = "--workspace"` to cover all deps), or accept the cold rebuild by setting `cargoArtifacts = null`.

At dispatch, read `result/mutants.out/baseline.log` first — the failure signature determines the fix. Commit message should cite the actual cause, not the hypothesis list above.

### T2 — `fix(nix):` baseline-failure gates derivation (complements P0304-T74's exit-code check)

MODIFY [`flake.nix`](../../flake.nix) at `:916-923`. P0304-T74 replaces `|| true` with an exit-code check that propagates everything except exit 2. Add an EXPLICIT baseline-health check after the `cargo mutants` call:

```nix
buildPhaseCargoCommand = ''
  cargo mutants \
    --in-place --no-shuffle \
    --config .config/mutants.toml \
    --output $out \
    --timeout-multiplier 2.0 \
    || { rc=$?; [ $rc -eq 2 ] || exit $rc; }

  # Baseline must have passed — if outcomes.json has zero mutants
  # tested (not zero caught — zero TESTED), the baseline failed and
  # the run is void. Fail the derivation so the weekly diff notices.
  tested=$(jq '.outcomes | length' $out/mutants.out/outcomes.json)
  if [ "$tested" -eq 0 ]; then
    echo "mutants baseline failed — zero mutants tested" >&2
    cat $out/mutants.out/debug.log >&2 || true
    exit 1
  fi
'';
```

This is belt-and-braces with P0304-T74's exit-code check: T74 catches `cargo-mutants` exiting non-2; T2 catches `cargo-mutants` exiting 0 with an empty outcomes list (which can happen if the baseline fails gracefully instead of exiting with an error — check cargo-mutants' behavior at dispatch).

### T3 — `test(nix):` smoke — mutants derivation produces >0 outcomes

NEW check in [`nix/tests/smoke-mutants.nix`](../../nix/tests/smoke-mutants.nix) OR add to [`flake.nix`](../../flake.nix) `checks.*`. A cheap derivation that depends on `.#mutants` and asserts `outcomes.json` has ≥1 mutant tested:

```nix
checks.mutants-smoke = pkgs.runCommand "mutants-smoke" {
  # Don't actually build mutants here — that's hours. Instead,
  # read the result if it's already built (nix will substitute if
  # cached). For CI: gate this check on a weekly cron that runs
  # .#mutants first, then this smoke check.
  nativeBuildInputs = [ pkgs.jq ];
} ''
  tested=$(jq '.outcomes | length' ${self.packages.${system}.mutants}/mutants.out/outcomes.json)
  [ "$tested" -gt 0 ] || { echo "mutants baseline failed — 0 tested"; exit 1; }
  touch $out
'';
```

**Scope caveat:** this check transitively builds `.#mutants` (hours). Do NOT add to `.#ci`. Either (a) make it a `packages.mutants-smoke` that the weekly cron invokes after `.#mutants`, or (b) use `nix-store --query --references` to check if `.#mutants` is already built and skip if not. Prefer (a) — simpler, and the weekly cron already sequences mutants → diff.

## Exit criteria

- `/nixbuild .#mutants` → `result/mutants.out/outcomes.json` has `.outcomes | length > 0` (T1: baseline passes, mutation phase runs)
- `jq '.outcomes[] | select(.phase == "Baseline")' result/mutants.out/outcomes.json` → shows a successful baseline outcome (T1: baseline healthy)
- `grep 'tested.*-eq 0\|baseline.*fail' flake.nix` → ≥1 hit in the mutants derivation (T2: baseline-health gate)
- **T2 mutation:** revert T1's fix → the derivation FAILS (T2's gate fires on zero-tested)
- `result/caught-count` and `result/missed-count` — both non-zero OR both zero with `tested > 0` (i.e., the counts are real, not silently-swallowed)

## Tracey

No spec markers — this is build-infrastructure repair, not rio-build behavior. The mutants derivation itself isn't a spec requirement; it's tooling.

## Files

```json files
[
  {"path": "flake.nix", "action": "MODIFY", "note": "T1: fix baseline-failure root cause (env/fixture/artifacts — determined at dispatch from debug.log); T2: +baseline-health jq check after cargo mutants; coordinates with P0304-T74's || true → exit-code check"}
]
```

```
flake.nix                  # T1: root-cause fix + T2: baseline gate
```

**Conditional files** (depending on T1's root cause — at most one of):
- `.config/mutants.toml` — if the fix is an `examine_globs` scope narrowing to skip a test that can't run in the mutants sandbox
- `rio-*/src/*.rs` — if the fix is a fixture-path correction (e.g., `env!("CARGO_MANIFEST_DIR")` instead of a hardcoded relative path)

## Dependencies

```json deps
{"deps": [301], "soft_deps": [304], "note": "discovered_from=user-query(2026-03-20). mutants.out @ 01:48: 1 Failure, 0 caught/0 missed — baseline build-and-test failed, mutation phase never ran. P0301 wired the derivation; || true at flake.nix:922 swallows the failure; weekly diff sees 0→0 no-change. P0304-T74 tracks the || true → exit-code check (exit 2 expected, others propagate). This plan is the diagnostic + root-cause fix. Soft-dep P0304-T74: T2 here adds baseline-health jq check as belt-and-braces with T74's exit-code check — sequence-independent, both additive to the same buildPhaseCargoCommand block. flake.nix count=32 (HOT) — T1+T2 edit the mutants derivation at :873-938, isolated from other checks/packages."}
```

**Depends on:** [P0301](plan-0301-cargo-mutants-ci.md) — `.#mutants` derivation exists. **DONE.**

**Soft-dep:** [P0304-T74](plan-0304-trivial-batch-p0222-harness.md) — `|| true` → exit-code check. Coordinates with T2 here; both edit the same `buildPhaseCargoCommand` block at `:916-923`. Sequence-independent (both additive, non-overlapping lines).

**Conflicts with:** [`flake.nix`](../../flake.nix) count=32 (HOT). T1+T2 edit the mutants derivation (`:873-938`); [P0304-T74/T75/T76/T78](plan-0304-trivial-batch-p0222-harness.md) touch the same derivation + `mutants.toml`. All edits are localized — T74 is `:916-922` (|| true → exit-code), T76 extracts `goldenTestEnv` (touches env list), T78 edits `.config/mutants.toml`. T1's root-cause fix may overlap T76 if it's a missing env var. Rebase at dispatch.

## Dispatch note (2026-03-20, coord)

Hermetic `.#mutants` re-launched on remote fleet (bk3ooi8me bg-task). If that run completes with `.outcomes | length > 0` and no Failure entry, T1 is OBE — narrow to T2 (baseline-health jq gate) + T3 (smoke check) only. The 01:48 local run was ENOSPC /tmp (cargo-mutants copied 4.2GB + full aws-sdk-s3 rebuild); hermetic path uses crane `cargoArtifacts` (pre-built deps) and remote sandbox TMPDIR.
