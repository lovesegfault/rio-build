# Plan 0069: Golden-test hermeticity + coverage-html infrastructure

## Design

The golden conformance tests (ported in P0060) failed on nixbuild.net's hermetic sandbox with three cascading errors, each only visible once the prior was fixed. This plan is the fix-chain plus the coverage infrastructure that exposed the need for one more fix.

**Hermeticity fix #1** (`9594961`): `nix eval` failed with "experimental-features unset" — no user `nix.conf` in the sandbox. Fixed by setting `NIX_CONFIG` on all `nix` invocations. **Then** `nix eval` failed again: `/nix/var` is not writable in the sandbox. Fixed by precomputing fixture paths in `flake.nix` (`pkgs.writeText` + FOD) and passing via `RIO_GOLDEN_{TEST,CA}_PATH` env vars. Tests check env vars first; compute `narHash`/`narSize` themselves from `nix-store --dump` (legacy command, no state dir needed); fall back to `nix eval` for local dev. **Then** the isolated daemon returned not-found for everything: `start_local_daemon()` symlinks `/nix/var/nix/*` into its temp state, but that directory doesn't exist in the sandbox → daemon starts with an empty SQLite db. Fixed by registering fixture paths via `nix-store --load-db` (writes SQLite directly; not `--register-validity`, which `chown`s the store path and fails on read-only `/nix/store`).

**Hermeticity fix #2** (`c57fbb7`): tests shared one `nix-daemon` via `LazyLock` static. Fine under nextest (each test is its own process → fresh static). Under `cargo test` (which `llvm-cov` uses), all 13 tests share the process → share the daemon → `test_golden_live_add_signatures` adds a sig, `test_golden_live_query_path_info` sees it, conformance comparison fails. Removed `LazyLock`; `fresh_daemon_socket()` starts a new daemon per call. ~100ms startup × 13 tests ≈ 1.3s overhead — negligible.

**Hermeticity fix #3** (`649f160`): `query_path_info_json()` always used computed metadata (`deriver=None`) when env vars were set. But `start_local_daemon()` branches: if `/nix/var/nix/db` *does* exist (local non-sandbox), it symlinks the real db — the daemon then knows the real deriver. Mismatch → local `nix build .#coverage` fails (remote sandbox passes because no real db → both sides use `--load-db` data). Fixed by mirroring the same `/nix/var/nix/db` check in the test helper.

**PG bootstrap race** (`1bfe157`): on 64-CPU nixbuild.net, many test processes bootstrap PG concurrently and hit a mkdtemp→write race: process A creates `/tmp/rio-pg-X/`, process B's GC sweep reads `X/owner.pid` → ENOENT (or empty: `std::fs::write` is open-CREAT-TRUNC then write, so a concurrent reader sees zero bytes) → `is_dir_stale=true` → `remove_dir_all(X)` → A's `initdb` hits ENOENT on `data/postgresql.conf`. Fix: only reap when we can *positively identify* the PID AND it is dead. Missing/empty `owner.pid` means an in-flight bootstrap. In the Nix sandbox `/tmp` is fresh per build, so no legacy dirs to worry about.

**PG connection bounding** (`a5df762`): `cargo test` (single process, `--test-threads=NCPU`) × 56 CPUs saturates the shared ephemeral PG server. Bound per-test connection usage.

**Coverage infra** (`adb6645`, `97f3e11`): `lcov` in dev shell (`lcov --summary result` / `--list result`). `coverage-html` package: `genhtml` wrapper that strips the sandbox-local `/nix/var/nix/builds/.../source/` prefix from lcov paths then runs from the real source root so it can read files for annotation.

Smaller bundled fixes: dropped an `#[ignore]` FUSE stub test (superseded by `vm-phase2a`), `NixHash::to_colon` made public (golden tests need it), pub fields on stderr wire-carrier types (pure data, no invariants, 11 getters removed), rustdoc warnings fixed.

## Files

```json files
[
  {"path": "rio-gateway/tests/golden/daemon.rs", "action": "MODIFY", "note": "RIO_GOLDEN_* env, NIX_CONFIG, nix-store --load-db, fresh_daemon_socket (remove LazyLock)"},
  {"path": "rio-gateway/tests/golden_conformance.rs", "action": "MODIFY", "note": "per-test daemon binding"},
  {"path": "rio-gateway/tests/integration_distributed.rs", "action": "MODIFY", "note": "drop ignored FUSE stub"},
  {"path": "rio-gateway/tests/wire_opcodes/build.rs", "action": "MODIFY", "note": "adopt pub stderr fields"},
  {"path": "rio-gateway/tests/wire_opcodes/misc.rs", "action": "MODIFY", "note": "adopt pub stderr fields"},
  {"path": "rio-gateway/tests/wire_opcodes/opcodes_read.rs", "action": "MODIFY", "note": "adopt pub stderr fields"},
  {"path": "rio-gateway/tests/wire_opcodes/opcodes_write.rs", "action": "MODIFY", "note": "adopt pub stderr fields"},
  {"path": "rio-test-support/src/pg.rs", "action": "MODIFY", "note": "gc_stale_dirs: treat missing/empty owner.pid as in-flight; bound connections"},
  {"path": "rio-test-support/src/wire.rs", "action": "MODIFY", "note": "adopt pub stderr fields"},
  {"path": "rio-nix/src/hash.rs", "action": "MODIFY", "note": "NixHash::to_colon public (was cfg(test)-only)"},
  {"path": "rio-nix/src/protocol/stderr.rs", "action": "MODIFY", "note": "Position/Trace/StderrError: pub fields, remove 11 getters"},
  {"path": "rio-nix/src/protocol/client.rs", "action": "MODIFY", "note": "rustdoc fix"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "rustdoc fix (escape angle brackets)"},
  {"path": "rio-worker/src/executor/daemon.rs", "action": "MODIFY", "note": "rustdoc fix"},
  {"path": "flake.nix", "action": "MODIFY", "note": "precomputed golden fixtures (writeText+FOD), RIO_GOLDEN_* env, lcov in shell, coverage-html package"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`.

## Entry

- Depends on **P0060** (golden port + monolith delete): hermeticizes the tests P0060 ported.
- Depends on **P0068** (fuzz CI + aggregates): `flake.nix` modified in both; `coverage-html` builds on the check attributes P0068 normalized.

## Exit

Merged as `1b69c65..97f3e11` (11 commits). `.#ci` green both locally AND on nixbuild.net hermetic sandbox (the goal). `nix build .#coverage-html && xdg-open result/index.html` works.
