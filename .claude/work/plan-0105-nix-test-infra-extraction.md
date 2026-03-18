# Plan 0105: Nix test infra extraction — common.nix + fuzz.nix

## Design

Five commits extracted shared NixOS-test machinery from per-phase test files into `nix/tests/common.nix` and `nix/fuzz.nix`, prerequisite for P0118 (vm-phase3a k3s test) which would have added a sixth copy of the same boilerplate.

`7653ca6` created `nix/tests/common.nix` by extracting ~250 LOC that was duplicated across `nix/tests/phase{1a,1b,2a,2b,2c}.nix`: PostgreSQL config block (5× identical), SSH key setup Python snippet (5× near-identical, differed only in gateway-host var name), `workerConfig` function (3× near-identical in 2a/2b/2c, differed in `maxBuilds`/`sizeClass`/OTel endpoint), client node config (5× near-identical), shared let-bindings (`busybox`, `busyboxClosure`, `databaseUrl`). Exposed: `mkWorkerNode { hostName, maxBuilds, sizeClass?, otelEndpoint? }`, `mkClientNode { gatewayHost, extraPackages? }`, `sshKeySetup` (parameterized by gateway node var name), `postgresqlConfig` (NixOS module attrset, merge via imports), `gatewayTmpfiles`. The 30-line explanatory comments for `writableStore=false` and `cores=4` moved to `common.nix` (single source of truth). Net: 1662 → 1515 LOC across test files; `common.nix` is +217 so per-test savings total ~364 LOC.

`1c770c9` extended `common.nix` with `mkControlNode` + `waitForControlPlane` + `seedBusybox` + `mkBuildHelper` — the control-plane machinery that was still duplicated after the first extraction. Another −200 LOC from per-phase files.

`87ee605` extracted `nix/fuzz.nix` from `flake.nix`, collapsing the fuzz-target definitions via `map` over a `fuzzTargets` list. `flake.nix` shrank by 238 LOC.

Two flake fixes (`57bb66b`, `ddae232`): set `NIXBUILDNET_MIN_CPU` on VM tests to prevent nixbuild.net from scheduling onto a starved host (boot timeout flake); added `wants=network-online.target` to `tempo.service` (it was starting before DNS was ready, flaking the phase2b OTLP test).

## Files

```json files
[
  {"path": "nix/tests/common.nix", "action": "NEW", "note": "mkWorkerNode, mkClientNode, mkControlNode, waitForControlPlane, seedBusybox, mkBuildHelper, sshKeySetup, postgresqlConfig, gatewayTmpfiles; shared let-bindings"},
  {"path": "nix/fuzz.nix", "action": "NEW", "note": "mkFuzzWorkspace + fuzzTargets list; extracted from flake.nix"},
  {"path": "nix/tests/phase1a.nix", "action": "MODIFY", "note": "-50 LOC, uses common.nix"},
  {"path": "nix/tests/phase1b.nix", "action": "MODIFY", "note": "-80 LOC, uses common.nix"},
  {"path": "nix/tests/phase2a.nix", "action": "MODIFY", "note": "-150 LOC, uses common.nix (2 workers via mkWorkerNode)"},
  {"path": "nix/tests/phase2b.nix", "action": "MODIFY", "note": "-130 LOC; Jaeger→Tempo comment fix; wants=network-online.target"},
  {"path": "nix/tests/phase2c.nix", "action": "MODIFY", "note": "-120 LOC, uses common.nix"},
  {"path": "flake.nix", "action": "MODIFY", "note": "-238 LOC fuzz extraction; NIXBUILDNET_MIN_CPU on VM tests"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `.nix` files are **not** parsed by tracey (noted in `.config/tracey/config.styx` — tracey handles `.rs`/`.md` only; VM-verified rules use `.sh` shims). No tracey markers in this plan's files.

## Entry

- Depends on P0096: phase 2c complete — all five phase-test files existed to extract from.

## Exit

Merged as `7653ca6..ddae232` (5 commits, non-contiguous). `.#ci` green at merge. Smoke-checked with vm-phase2a (152s, PASS) — exercises `mkWorkerNode` (2 workers), `mkClientNode`, `postgresqlConfig`, `gatewayTmpfiles`, `sshKeySetup`. All five phase VM tests pass unchanged.
