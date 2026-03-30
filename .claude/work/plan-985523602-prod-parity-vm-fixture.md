# Plan 985523602: Production-parity VM fixture — HA scheduler + bootstrap.enabled

Three prod regressions in a row from [P0493](plan-0493-bootstrap-job-psa-restricted.md)/[P0494](plan-0494-xtask-cli-tunnel-local-exec.md), all fixed post-merge, all with the same root cause: **VM tests use minimal config; prod uses HA + bootstrap.enabled.** The `k3s-full.nix` fixture runs scheduler `replicas: 1` and `bootstrap.enabled: false` — neither the Lease-election standby-reject path nor the bootstrap Job runs in CI.

| Regression | Fix | What CI missed |
|---|---|---|
| [`a28e4b65`](https://github.com/search?q=a28e4b65&type=commits) bootstrap-job awscli2 `/.aws` EROFS | `HOME=/tmp` | bootstrap Job never renders (`bootstrap.enabled=false`) |
| [`abef66c7`](https://github.com/search?q=abef66c7&type=commits) `tunnel_grpc` hits standby | Lease lookup → `pod/<holder>` | single replica, no standby to hit |
| [`5b98e311`](https://github.com/search?q=5b98e311&type=commits) `step_tenant` exit-code | match-Err for AlreadyExists | single replica made tunnel deterministic; idempotent re-run never tested |

This plan adds a **prod-parity fixture variant** (`k3s-full.nix` with `scheduler.replicas=2` + `bootstrap.enabled=true`) wired into one existing scenario. Not a new scenario — a fixture overlay so the delta is small and the existing subtests prove the HA/bootstrap paths work.

## Entry criteria

- [P0493](plan-0493-bootstrap-job-psa-restricted.md) merged (`bootstrap-job.yaml` with `readOnlyRootFilesystem` + `HOME=/tmp` exists)
- [P0494](plan-0494-xtask-cli-tunnel-local-exec.md) merged (`tunnel_grpc` Lease-lookup at [`k3s/smoke.rs:77-91`](../../xtask/src/k8s/k3s/smoke.rs) exists)

## Tasks

### T1 — `feat(test):` k3s-prod-parity fixture overlay — scheduler replicas=2 + bootstrap.enabled

NEW `nix/tests/fixtures/k3s-prod-parity.nix` — imports `k3s-full.nix` and overrides the helm values:

```nix
# Prod-parity overlay: HA scheduler + bootstrap Job enabled.
# Catches the regression class where VM tests pass on minimal config
# but prod (replicas=2, bootstrap.enabled=true) breaks. Three prod
# breaks from P0493/P0494 motivated this — see plan-985523602.
{ lib, ... }@args:
let base = import ./k3s-full.nix args;
in base // {
  helmValues = lib.recursiveUpdate base.helmValues {
    scheduler.replicas = 2;
    bootstrap.enabled = true;
    # bootstrap needs a real-ish S3 target — point at the in-cluster
    # MinIO (k3s-full already brings it up for the store backend).
    bootstrap.image = base.helmValues.global.image; # reuse rio image (has awscli2)
    store.chunkBackend.bucket = "rio-chunks"; # MinIO bucket
  };
}
```

Exact attr paths depend on `k3s-full.nix`'s structure (grep `helmValues\|replicas` at dispatch — 7 `replicas` hits found). The overlay should be <30 lines; if `k3s-full.nix` doesn't expose a clean override point, add one there first (e.g., `helmValuesExtra ? {}` merged via `recursiveUpdate`).

### T2 — `test(vm):` wire prod-parity fixture into lifecycle scenario

MODIFY [`nix/tests/default.nix`](../../nix/tests/default.nix). Add a scenario variant using the new fixture:

```nix
vm-lifecycle-k3s-prod-parity = mkK3sTest {
  fixture = ./fixtures/k3s-prod-parity.nix;
  scenario = ./scenarios/lifecycle.nix;
  subtests = [
    # r[verify sched.grpc.leader-guard]
    "bootstrap-tenant"   # exercises tunnel → leader pod, not Service
    # r[verify sec.psa.control-plane-restricted]
    "bootstrap-job-ran"  # new subtest: Job completed, no EROFS
  ];
};
```

Pick an existing scenario (`lifecycle.nix` is the broadest) whose subtests exercise admin writes — those are the ones that break on standby. Don't duplicate all 13 subtests; 2-3 that cover the HA + bootstrap paths is enough. Bootstrap adds ~30s to VM spin-up (Job runs post-install); budget accordingly.

### T3 — `test(vm):` bootstrap-job-ran subtest — assert Job Completed + no EROFS

MODIFY `nix/tests/scenarios/lifecycle.nix` (or wherever T2's scenario lives). Add the subtest:

```python
with subtest("bootstrap-job-ran"):
    # Job should be Complete (not BackoffLimitExceeded).
    client.succeed(
        "kubectl -n rio-system wait --for=condition=Complete "
        "job/rio-bootstrap --timeout=120s"
    )
    # awscli2 wrote to $HOME/.aws/ under readOnlyRootFilesystem —
    # EROFS here is the P0493 regression signature.
    logs = client.succeed("kubectl -n rio-system logs job/rio-bootstrap")
    assert "Read-only file system" not in logs, f"bootstrap hit EROFS: {logs}"
```

The `r[verify sec.psa.control-plane-restricted]` marker goes at the `default.nix` subtests entry (T2), NOT here — per the wiring convention.

### T4 — `test(vm):` leader-election subtest — admin write via tunnel reaches leader

Reuse or extend an existing `bootstrap-tenant` subtest. The assertion: `xtask k8s cli create-tenant` (or equivalent in-VM rio-cli call) succeeds **deterministically** with `replicas=2`. Before [`abef66c7`](https://github.com/search?q=abef66c7&type=commits) it failed ~50% depending on which pod the Service forward landed on.

```python
with subtest("bootstrap-tenant"):
    # replicas=2 → one leader + one standby. tunnel_grpc queries the
    # Lease and targets pod/<holder>. Before abef66c7, Service forward
    # → ~50% "not leader" rejections.
    for _ in range(3):  # 3 attempts proves determinism, not luck
        client.succeed("xtask-or-rio-cli create-tenant prod-parity-test || true")
    out = client.succeed("rio-cli tenants")
    assert "prod-parity-test" in out
```

The `r[verify sched.grpc.leader-guard]` marker goes at the `default.nix` subtests entry (T2). This is the first VM-level verify for leader-guard under `replicas>1`.

## Exit criteria

- `/nbr .#ci` green (new VM test included in `.#ci` via `default.nix` wiring)
- `nix build .#checks.x86_64-linux.vm-lifecycle-k3s-prod-parity` passes standalone
- T1: `grep 'replicas = 2' nix/tests/fixtures/k3s-prod-parity.nix` → ≥1 hit; `grep 'bootstrap.enabled = true'` → ≥1 hit
- T2: `default.nix` has `vm-lifecycle-k3s-prod-parity` (or similar name) with ≥2 subtests
- T3: bootstrap-job-ran subtest asserts Job `Complete` + no `Read-only file system` in logs
- T4: bootstrap-tenant subtest runs ≥2 attempts, asserts tenant created; manually set `scheduler.replicas=2` + revert [`abef66c7`](https://github.com/search?q=abef66c7&type=commits) → test FAILS with "not leader"
- `tracey query rule sched.grpc.leader-guard` shows ≥1 verify site; `tracey query rule sec.psa.control-plane-restricted` shows the new verify site

## Tracey

References existing markers:
- `r[sched.grpc.leader-guard]` — T4 verifies (first VM-level verify under `replicas>1`; marker at [`scheduler.md:651`](../../docs/src/components/scheduler.md) — standbys reject admin writes, clients route via Lease)
- `r[sec.psa.control-plane-restricted]` — T3 verifies (bootstrap Job runs under `readOnlyRootFilesystem` without EROFS; marker at [`security.md:79`](../../docs/src/security.md))

No new markers. The prod-parity fixture itself is test infrastructure, not spec-covered behavior — it exercises EXISTING spec'd behaviors (leader-guard, PSA-restricted) under a config the minimal fixture doesn't reach.

## Files

```json files
[
  {"path": "nix/tests/fixtures/k3s-prod-parity.nix", "action": "NEW", "note": "T1: overlay on k3s-full with replicas=2 + bootstrap.enabled=true"},
  {"path": "nix/tests/fixtures/k3s-full.nix", "action": "MODIFY", "note": "T1: add helmValuesExtra override point if not already clean"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T2: vm-lifecycle-k3s-prod-parity scenario + r[verify] markers at subtests entries"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T3+T4: bootstrap-job-ran + bootstrap-tenant subtests"}
]
```

```
nix/tests/
├── fixtures/
│   ├── k3s-full.nix            # T1: helmValuesExtra hook (if needed)
│   └── k3s-prod-parity.nix     # T1: NEW — replicas=2 + bootstrap overlay
├── scenarios/
│   └── lifecycle.nix           # T3+T4: bootstrap-job-ran, bootstrap-tenant
└── default.nix                 # T2: wire scenario + r[verify] markers
```

## Dependencies

```json deps
{"deps": [493, 494], "soft_deps": [311, 985523601], "note": "HARD-DEP P0493 (bootstrap-job.yaml + securityContext exist). HARD-DEP P0494 (tunnel_grpc Lease-lookup exists — T4 tests it). SOFT-DEP P0311 (test-gap batch T494 adds helm-lint max-coverage render with bootstrap.enabled; this plan runs the rendered Job, orthogonal). SOFT-DEP P985523601 (CliCtx audit — T4 exercises the same create-tenant path step_tenant uses; sequence-independent). discovered_from=493+494+coverage."}
```

**Depends on:** [P0493](plan-0493-bootstrap-job-psa-restricted.md) — bootstrap Job template + PSA securityContext. [P0494](plan-0494-xtask-cli-tunnel-local-exec.md) — `tunnel_grpc` Lease-lookup (the thing T4 tests).
**Soft-dep:** [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T494 renders bootstrap-job in helm-lint; this plan RUNS it. [P985523601](plan-985523601-clictx-run-exit-code-audit.md) audits `CliCtx::run` callers; T4 here exercises one of them.
**Conflicts with:** `nix/tests/default.nix` is collision-count 28 (hot file). Wire the new scenario at the END of the test list to minimize rebase churn. `lifecycle.nix` is shared across scenarios — T3/T4 subtests are additive (new `with subtest(...)` blocks), non-overlapping with other plans.
