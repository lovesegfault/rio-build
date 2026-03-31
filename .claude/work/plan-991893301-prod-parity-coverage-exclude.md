# Plan 991893301: exclude prod-parity VM test from coverage-full

[P0500](plan-0500-prod-parity-vm-fixture.md)'s `vm-lifecycle-prod-parity-k3s` test is ABOUT PSA enforcement — [`bootstrap-job-ran`](../../nix/tests/scenarios/lifecycle.nix) at `:3286-3307` asserts `readOnlyRootFilesystem=true` on the Job's pod template to prove the `rio.containerSecurityContext` helper rendered PSA-restricted. But coverage-mode bumps PSA to `privileged` (instrumented binaries write profraws to mounted volumes → need writable fs → can't be PSA-restricted). Under coverage, the jsonpath returns empty → `assert rorfs == "true"` fails.

This is a **deterministic** failure every `.#coverage-full` run. The implementer who wrote the assert at P0500 predicted it in the message: "coverage mode does this — prod-parity fixture shouldn't." Log: `/tmp/rio-dev/rio-sprint-1-merge-193.log:3961`.

**Route A (recommended): exclude from coverage-full entirely.** The test's purpose is to prove prod-default PSA renders correctly. Coverage-mode IS non-prod by construction (it changes security posture to collect profraws). Running it under coverage doesn't measure anything useful — there's no `r[impl]`-annotated Rust in the PSA rendering path (it's Helm+YAML), so no coverage delta.

**Route B (conditionally drop the rorfs assert when `common.coverage`):** keeps the bootstrap-progress check (`"[bootstrap] generating rio/hmac"` log line) under coverage. Weaker — the test becomes half-hollow under coverage, and the conditional is a silent forking of test semantics.

**Route C (override PSA bump in prod-parity fixture only):** fights the coverage infrastructure. The PSA bump is there for a reason (profraw writes); overriding it means either coverage data is incomplete for this one scenario, or the write fails silently.

Route A is cleanest. discovered_from=500. origin=coverage.

## Entry criteria

- [P0500](plan-0500-prod-parity-vm-fixture.md) merged (prod-parity fixture exists at [`lifecycle.nix:3286`](../../nix/tests/scenarios/lifecycle.nix))

## Tasks

### T1 — `fix(nix):` exclude `vm-lifecycle-prod-parity-k3s` from `vmTestsCov`

[`flake.nix:1206-1213`](../../flake.nix) builds `vmTestsCov` as a full `mkVmTests` call — no filtering today. Add `removeAttrs` post-call:

```nix
# flake.nix:~1206
vmTestsCov = removeAttrs
  (mkVmTests {
    rio-workspace = rio-workspace-cov;
    dockerImages = mkDockerImages {
      rio-workspace = rio-workspace-cov;
      coverage = true;
    };
    coverage = true;
  })
  # prod-parity asserts readOnlyRootFilesystem=true (PSA-restricted);
  # coverage-mode bumps PSA to privileged → assertion deterministically
  # fails. The test is ABOUT PSA — running it under a mode that changes
  # PSA defeats the point. No coverage delta lost: PSA rendering is
  # Helm+YAML, no r[impl]-annotated Rust.
  [ "vm-lifecycle-prod-parity-k3s" ];
```

[`nix/coverage.nix:121`](../../nix/coverage.nix) `perTestLcov = lib.mapAttrs mkPerTestLcov vmTestsCov` then won't see the excluded attr — no per-test lcov for it, no contribution to the merged report. No change needed there.

**Verify:** `nix eval .#packages.x86_64-linux --apply 'p: builtins.hasAttr "cov-vm-lifecycle-prod-parity-k3s" p'` → `false`. But `nix eval .#checks.x86_64-linux --apply 'c: builtins.hasAttr "vm-lifecycle-prod-parity-k3s" c'` → `true` (still in non-coverage `vmTests`, still gates `.#ci`).

### T2 — `fix(nix):` add `vm-lifecycle-prod-parity-k3s` to `cpuHints` table

Bundled from the P0500 reviewer finding: [`flake.nix:1151-1192`](../../flake.nix) `cpuHints` table lacks `vm-lifecycle-prod-parity-k3s`. It falls through to `or 4` at `:1195` while every sibling k3s test gets `8` (`:1172-1191`). Same 2-node k3s fixture (k3s-server 8-core + k3s-agent 8-core) plus bootstrap Job backoff (~90s to Failed, 240s timeout).

`withMinCpu` formula is `numVMs × 4 + 1`: at hint=4 that's a 17-core request vs 33-core at hint=8. Undersubscription → TCG fallback → the qemu stall class the table exists to prevent (`:1148-1150` comment).

```nix
# After :1191 (vm-netpol-k3s = 8;)
# Same 2-node k3s fixture + bootstrap Job backoff.
# Asserts PSA-restricted — NOT in vmTestsCov (T1).
vm-lifecycle-prod-parity-k3s = 8;
```

This is independent of T1: the cpuHints entry applies to the non-coverage `vmTests` build where this test still runs.

## Exit criteria

- `/nbr .#ci` green (prod-parity still runs under non-coverage `vmTests`, still passes)
- `nix eval .#packages.x86_64-linux --apply 'p: builtins.hasAttr "cov-vm-lifecycle-prod-parity-k3s" p'` → `false`
- `nix eval .#checks.x86_64-linux --apply 'c: builtins.hasAttr "vm-lifecycle-prod-parity-k3s" c'` → `true`
- `/nbr .#coverage-full` green — no longer includes prod-parity (this is the load-bearing check; pre-plan it fails deterministically at merge-193.log:3961)
- `grep 'vm-lifecycle-prod-parity-k3s = 8' flake.nix` → 1 hit (cpuHints entry added)
- Comment at the `removeAttrs` site explains why (PSA-is-the-point; coverage-mode changes PSA)

## Tracey

No domain markers — this is test-infrastructure, not spec-governed behavior. The PSA enforcement itself is covered by `r[sec.psa.control-plane-restricted]` (already `r[verify]`-annotated at [`default.nix`](../../nix/tests/default.nix) subtests wiring per [`lifecycle.nix:3284-3285`](../../nix/tests/scenarios/lifecycle.nix)); that marker's verify status is unchanged (non-coverage `vmTests` still runs the assertion).

## Files

```json files
[
  {"path": "flake.nix", "action": "MODIFY", "note": "T1: removeAttrs vm-lifecycle-prod-parity-k3s from vmTestsCov at :1206. T2: add cpuHints entry at :1192"}
]
```

```
flake.nix    # T1: vmTestsCov removeAttrs :1206
             # T2: cpuHints entry :1192
```

## Dependencies

```json deps
{"deps": [500], "soft_deps": [], "note": "P0500 created the fixture; this excludes it from coverage-full + adds its cpuHints entry. Both discovered_from=500."}
```

**Depends on:** [P0500](plan-0500-prod-parity-vm-fixture.md) — created `vm-lifecycle-prod-parity-k3s` and the `bootstrap-job-ran` assertion this plan works around.

**Conflicts with:** `flake.nix` is a medium-heat file (the `cpuHints` table at `:1151-1192` is append-only by convention; `vmTestsCov` at `:1206` is rarely touched). Low conflict risk — both changes are localized.
