# Plan 0143: VM coverage pipeline — .#coverage-full, instrumented build, profraw collection

## Design

Unit-test coverage sat at ~78%. The "permanently red" ~15% was code that ONLY runs in VM tests: FUSE callbacks (kernel calls them), namespace setup (needs `CAP_SYS_ADMIN`), cgroup tracking (needs real cgroup2fs), `main.rs` wiring (never unit-tested), k8s lease/reconcilers (need real kube-apiserver), SSH accept loop (needs real ssh client). This plan built the pipeline to collect coverage FROM VM tests and merge it with unit coverage.

**Instrumented build (`rio-workspace-cov`):** `RUSTFLAGS="-C instrument-coverage"` on the full workspace. Distinct pname → separate Crane deps cache → builds in PARALLEL with normal workspace (doesn't poison the normal cache). `mkDockerImages` helper parameterized by which workspace to use.

**Coverage-mode VM tests (`vmTestsCov`):** `nix/tests/common.nix` parameterized with `coverage ? false`. When true: every `rio-*` systemd service gets `LLVM_PROFILE_FILE=/var/lib/rio/cov/rio-%p-%m.profraw` in its Environment. `%p`=PID (handles restarts — each scheduler restart writes a new file), `%m`=binary signature (safe merging). Controller's `builders.rs` propagates `LLVM_PROFILE_FILE` to worker pods when the controller itself has it set (detects instrumented mode via env).

**Profraw collection (`common.nix collectCoverage`):** at the end of each `testScript`: `systemctl stop rio-*` (graceful, thanks to P0142) → tar `/var/lib/rio/cov` → `copy_from_vm` to `$out/coverage/<node>/profraw.tar.gz`. For phase3a/3b, ALSO `kubectl delete sts --all` — worker pod's profraws are on hostPath, flush only on pod termination.

**Merge (`nix/coverage.nix`):** toolchain `llvm-profdata merge` (NOT system llvm — profile format is tied to rustc version) → `llvm-cov export --format=lcov` → source-path normalize (strip sandbox prefix, anchor on `source/` — crane's unpackPhase convention) → `lcov -a` union with unit-test lcov → `--extract 'rio-*'` → `genhtml`. The source-path normalize MUST match between unit and VM lcovs — if `lcov -a` shows doubled files with zero intersection, the strip regex is wrong.

**`mkVmTests` refactor (`ba0d5e2`):** `flake.nix` had ~200 lines of per-phase VM test boilerplate. Extracted into `mkVmTests { phases, coverage }` in `nix/coverage.nix`. `vmTests` = normal mode, `vmTestsCov` = coverage mode, `cov-vm-<phase>` = per-phase coverage-mode run (debugging).

**systemd `%`-escape (`fdcf6ce`):** `LLVM_PROFILE_FILE=.../rio-%p-%m.profraw` in systemd Environment → `%p` interpreted as systemd specifier (`%p` = unit prefix name). Escaped as `%%p`.

**`.#coverage-full` + `.#coverage-full-html`:** standalone targets. NOT in `.#ci` — ~25min, needs KVM, invoke on demand.

**Branch coverage attempt (`8126dcf` + `395c049`, both reverted in `166016e` + `4c8365d`):** `-Z coverage-options=branch` on nightly. `llvm-cov export` segfaulted with 20+ object files (one per crate × unit + VM). Upstream issue. Reverted; line coverage only. The revert pair is part of the story — branch coverage was attempted and deliberately abandoned.

**SIGKILL for V3 (`aab1499`):** after graceful shutdown landed, vm-phase3b's V3 "crash simulation" (`systemctl stop rio-scheduler`) became a graceful drain — in-flight `WatchBuild` streams finished → build completed → no non-terminal rows → V3's "recovery loads real rows" assertion failed. Switched to `systemctl kill -s SIGKILL rio-scheduler`.

## Files

```json files
[
  {"path": "nix/coverage.nix", "action": "NEW", "note": "mkVmTests + rio-workspace-cov + merge pipeline"},
  {"path": "flake.nix", "action": "MODIFY", "note": "mkDockerImages helper + vmTestsCov + .#coverage-full + .#coverage-full-html + revert branch coverage"},
  {"path": "nix/tests/common.nix", "action": "MODIFY", "note": "coverage param + LLVM_PROFILE_FILE env (%%-escaped) + collectCoverage helper"},
  {"path": "nix/tests/phase1a.nix", "action": "MODIFY", "note": "coverage param passthrough"},
  {"path": "nix/tests/phase1b.nix", "action": "MODIFY", "note": "coverage param passthrough"},
  {"path": "nix/tests/phase2a.nix", "action": "MODIFY", "note": "coverage param passthrough"},
  {"path": "nix/tests/phase2b.nix", "action": "MODIFY", "note": "coverage param passthrough"},
  {"path": "nix/tests/phase2c.nix", "action": "MODIFY", "note": "coverage param passthrough"},
  {"path": "nix/tests/phase3a.nix", "action": "MODIFY", "note": "coverage param + delete STS for pod profraw flush"},
  {"path": "nix/tests/phase3b.nix", "action": "MODIFY", "note": "coverage param + SIGKILL for V3 crash simulation"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "propagate LLVM_PROFILE_FILE to worker pod when set on controller"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests.rs", "action": "MODIFY", "note": "LLVM_PROFILE_FILE env test"},
  {"path": "rio-controller/Cargo.toml", "action": "MODIFY", "note": "test deps"}
]
```

## Tracey

No tracey markers landed. Build-system infrastructure.

## Entry

- Depends on P0142: graceful shutdown (profraw flush on SIGTERM).

## Exit

Merged as `7984ea6..4c8365d` (11 commits, 2 reverted). `.#ci` green at merge — coverage mode is opt-in, doesn't affect normal CI. `.#coverage-full` produces `result/lcov.info` (combined) + `result/html/` + `result/per-test/vm-phase*.lcov`. Branch coverage deliberately abandoned (llvm-cov segfault with 20+ objects).
