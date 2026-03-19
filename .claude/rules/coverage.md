---
paths:
  - "nix/coverage.nix"
  - "nix/common.nix"
  - "nix/tests/**"
---

VM coverage architecture (`nix/coverage.nix`):

1. **Instrumented build** (`rio-workspace-cov`): `RUSTFLAGS="-C instrument-coverage"` + distinct pname → separate deps cache, builds in parallel with normal workspace.
2. **Coverage-mode VM tests** (`vmTestsCov`): same tests with `coverage=true` → `LLVM_PROFILE_FILE=/var/lib/rio/cov/rio-%p-%m.profraw` set in all rio-* service envs (`%p`=PID for restarts, `%m`=binary signature for safe merging).
3. **Graceful shutdown** (`rio-common::signal::shutdown_signal`): SIGTERM or SIGINT → CancellationToken → main() returns normally → atexit handlers fire → LLVM profraw flush. All binaries: store, scheduler, gateway, worker, controller.
4. **Collection** (`common.nix collectCoverage`): at end of each testScript, `systemctl stop rio-*` → graceful drain → tar `/var/lib/rio/cov` → `copy_from_vm` to `$out/coverage/<node>/profraw.tar.gz`. phase3a also deletes the k3s STS so the worker pod's hostPath-mounted profraws flush.
5. **Merge** (`nix/coverage.nix`): toolchain `llvm-profdata merge` → `llvm-cov export --lcov` → source-path normalize (strip sandbox prefix, anchor on `source/`) → `lcov -a` union with unit-test lcov → `--extract 'rio-*'` → genhtml.

Gotchas:
- Source-path normalization MUST match between unit and VM lcovs — both anchor on `source/` (crane's unpackPhase convention). If `lcov -a` merge shows doubled files with zero intersection, the strip regex is wrong.
- Use **toolchain** `llvm-profdata`/`llvm-cov` (`$(rustc --print sysroot)/lib/rustlib/<target>/bin/`), never system llvm. Profile format versioning is tied to rustc.
