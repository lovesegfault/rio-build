# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

rio-build is an early-stage Rust project. It uses a Nix flake-based development environment with Crane for building Rust packages and protobuf for gRPC code generation.

## Quick Start

The dev environment is managed by Nix. If you have direnv, it activates automatically via `.envrc`.

```bash
# Enter the dev shell (if direnv isn't set up).
# Default shell uses NIGHTLY Rust so cargo-fuzz works; use .#stable for CI parity.
nix develop

# Build
cargo build

# Run
cargo run

# Test (prefer nextest for better output)
cargo nextest run
# or: cargo test

# Single test
cargo nextest run test_name
# or: cargo test test_name

# Lint
cargo clippy --all-targets -- --deny warnings

# Format (uses treefmt: rustfmt + nixfmt + taplo)
treefmt

# Full CI-equivalent checks (clippy, tests, docs, coverage)
nix flake check

# Fuzz a parser (default shell is nightly, so this works directly)
cd rio-nix/fuzz && cargo fuzz run wire_primitives
```

## Build System

- **Nix + Crane**: The flake.nix defines the full build pipeline. Crane handles dependency caching via `buildDepsOnly` and the actual build via `buildPackage`.
- **Cargo workspace**: Root Cargo.toml is a workspace; crates live in subdirectories.
- **Protobuf**: `.proto` files are included in the Crane source filter. `PROTOC` and `LIBCLANG_PATH` are set automatically in the dev shell.

## Key Commands via Nix

| Command | What it does |
|---|---|
| `nix build` | Build the workspace (release profile with thin LTO) |
| `nix build .#ci-local-fast` | Full local validation: build, clippy, nextest, doc, coverage, pre-commit, 30s fuzz smoke ×8 |
| `nix build .#ci-local-slow` | Same as `ci-local-fast` but with 10-min fuzz instead of 30s smoke |
| `nix build .#ci-fast` / `.#ci-slow` | Same as `ci-local-*` plus VM tests (Linux+KVM only — typically via `nix-build-remote`) |
| `nix flake check` | Runs all `checks.*` (build, clippy, nextest, doc, coverage, 30s fuzz smoke, VM tests) |
| `nix develop .#stable` | Dev shell with stable Rust (CI parity) |
| `nix build .#fuzz-nightly-<target>` | 10-minute nightly-tier fuzz run for a single target |
| `nix fmt` | Same as `treefmt` |

### CI aggregate targets

Single-target validation bundles — build one of these instead of remembering individual checks:

|                | 30s fuzz smoke | 10min fuzz nightly |
|----------------|----------------|--------------------|
| **no VM tests** | `ci-local-fast` | `ci-local-slow` |
| **with VM tests** | `ci-fast` | `ci-slow` |

The VM-including aggregates need KVM — use `nix-build-remote -- .#ci-slow`. On non-Linux, `ci-local-*` degrades to cargo checks + pre-commit only (fuzz is Linux-only). Result is a directory of symlinks to each constituent's output (`ls result/`).

## Formatting

treefmt runs three formatters:
- **rustfmt** for `.rs` files
- **nixfmt** for `.nix` files
- **taplo** for `.toml` files

Pre-commit hooks run treefmt automatically on commit.

## Development Notes

- Rust edition is **2024** — use latest Rust idioms and features.
- Clippy is configured with `--deny warnings` — all warnings must be fixed.
- The `target/` directory is gitignored; Nix builds go to `result`/`result-*` (also gitignored).
- Integration tests may need `nix` available in PATH (it's provided in the dev shell).
- **Default dev shell uses nightly Rust** so `cargo fuzz` works directly. CI builds (clippy, nextest, workspace) use stable via `rust-toolchain.toml` — nightly-only code will be rejected by `nix flake check`. Use `nix develop .#stable` for CI-parity development.
- **Always run cargo commands via `nix develop -c`** to ensure all dev shell dependencies (including fuse3) are available. E.g., `nix develop -c cargo nextest run`, `nix develop -c cargo clippy --all-targets -- --deny warnings`.
- **Always run `nix develop -c cargo nextest run` before committing** to catch regressions early.
- PostgreSQL integration tests bootstrap their own ephemeral postgres server (via `rio-test-support`) using `initdb`/`postgres` binaries from the dev shell. **No manual setup needed.** Tests panic (not skip) if postgres binaries are unavailable. Set `DATABASE_URL` to override with an external PG for debugging.
- Use semantic commit messages scoped by crate (e.g., `feat(rio-nix): add ATerm derivation parser`).
- Keep phase plan docs (`docs/src/phases/`) in sync: mark tasks `[x]` as they're completed.

## Fuzzing

Fuzz targets live in per-crate `fuzz/` workspaces (excluded from the main workspace, separate `Cargo.lock` each). Currently: `rio-nix/fuzz/` (protocol/wire parsers) and `rio-store/fuzz/` (manifest parser). The default dev shell is nightly, so `cargo fuzz` works without extra setup:

```bash
nix develop -c bash -c 'cd rio-nix/fuzz && cargo fuzz run wire_primitives'
nix develop -c bash -c 'cd rio-store/fuzz && cargo fuzz run manifest_deserialize'
```

CI equivalents:
```bash
nix build .#checks.x86_64-linux.fuzz-smoke-wire_primitives  # 30s smoke (PR tier, in flake check)
nix build .#fuzz-nightly-wire_primitives                    # 10min (nightly tier, explicit)
```

When adding a new parser, also add a fuzz target:
1. Add a `[[bin]]` entry in the relevant `fuzz/Cargo.toml` + target file in `fuzz_targets/`
2. Add seed inputs to `fuzz/corpus/<target>/` (must be prefixed `seed-`; NAR seeds: see `gen-nar-corpus.sh`)
3. Add the target to `fuzzTargets` in `flake.nix` (target name + which `fuzzBuild` + `corpusRoot`)
4. If the fuzzed crate's deps changed, run `cd <crate>/fuzz && cargo update -p <crate>` (fuzz lockfile is independent)

## Design Book

This project has a comprehensive design book in `docs/src/`. When implementing any phase, cross-reference ALL relevant design docs — not just the phase plan:

- **Phase plan** (`docs/src/phases/phaseXY.md`): Task list and milestones
- **Component specs** (`docs/src/components/`): Protocol details, API contracts
- **Observability spec** (`docs/src/observability.md`): Metric names, log format, tracing structure
- **Crate structure** (`docs/src/crate-structure.md`): Expected modules and file layout
- **Architecture** (`docs/src/architecture.md`): System-level design

### Keeping docs and code in sync

When implementation reveals that a design doc is wrong (e.g., the spec says u32 but the real protocol uses u64), update the design doc in the same commit that fixes the code. Don't let them drift.

### Deferred work and TODOs

**Every deferred task must have a phase-tagged TODO comment.** Untagged TODOs accumulate into an untracked backlog that never gets scheduled.

Format: `TODO(phaseXY): <what> — <why deferred / what it's blocked on>`

```rust
// GOOD: tagged with phase, explains current behavior and future intent
// TODO(phase2b): buffer and forward build logs to gateway.
// Phase 2b spec: 64-line/100ms batching, per-derivation ring buffer.

// GOOD: points to the blocking dependency
// TODO(phase3a): actual leader generation from Kubernetes Lease.
// Phase 2a has a single scheduler instance; constant 1 is correct.
generation: 1,

// BAD: no phase tag, no context — when does this get done? what's blocking it?
// TODO: fix this later
```

**Choosing the right phase:**
- Check `docs/src/phases/phase*.md` task lists — find the one that mentions the feature
- If nothing matches, the work is either (a) actually in-scope for the current phase, or (b) a design gap that needs a phase doc update before tagging
- When the phase doc doesn't mention it but clearly should, update the doc in the same commit

**When to write a TODO:**
- You're implementing a stub/placeholder that a later phase will fill in
- You're making a simplifying assumption that's correct now but won't be later (e.g., "single scheduler instance")
- You're skipping an optimization that's out of scope (e.g., scheduler-side closure computation)
- A test is blocked on infrastructure (e.g., privileged CI containers)

**When NOT to write a TODO:**
- The deferred behavior is already documented in a `> **Phase X deferral:**` block in the design doc — reference it in a normal comment instead
- The code is actually complete for the current phase and there's no concrete future work

**Audit before phase completion:** Grep for `TODO[^(]` (untagged) and `TODO\(phase2a\)` (current-phase) before marking a phase done. Untagged TODOs must be either tagged or resolved; current-phase TODOs must be resolved.

## Protocol Implementation Guidelines

rio-build implements the Nix worker protocol. Protocol work has specific pitfalls:

### Validate against the real implementation early

Don't trust specs or design docs alone. Run integration tests against a real `nix` client (e.g., `nix store info --store ssh-ng://localhost`) as soon as the handshake compiles. Protocol bugs found through integration are cheaper than protocol bugs found in review.

### Wire encoding conventions

The Nix worker protocol sends `narHash` fields as **hex-encoded SHA-256 digests** — no algorithm prefix, no nixbase32. Use `hex::decode` + `NixHash::new`, not `NixHash::parse_colon`. The `sha256:nixbase32` format appears in narinfo text and colon-format hashes, not on the wire. When in doubt, check the real daemon with a golden conformance test before assuming an encoding.

### Wire-level testing

Every opcode and wire primitive needs a byte-level test that constructs raw bytes, feeds them through the parser, and asserts the result. High-level integration tests are not sufficient — they hide framing and encoding bugs.

Include:
- Proptest roundtrips for all wire primitives (u64, bytes, bool, strings, collections)
- Live-daemon golden conformance tests: each test starts an isolated nix-daemon and compares responses at the byte level (see `tests/golden/`)
- Fuzz targets for wire parsing (`cargo-fuzz` in `rio-nix/fuzz/`)

**Before marking an opcode as complete, verify:**
- [ ] Byte-level test in `direct_protocol.rs` (constructs raw wire bytes, no SSH)
- [ ] At least one error-path test (e.g., missing path, hash mismatch) verifying STDERR_ERROR is sent
- [ ] Proptest roundtrip for any new wire types introduced by the opcode
- [ ] Fuzz target for any new parser (ATerm, NAR, DerivedPath, etc.)
- [ ] Integration test asserts success (not just "exercises the protocol path")

### Safety bounds

Protocol parsers must enforce:
- Maximum collection sizes (prevent DoS via unbounded allocation)
- Maximum string/path lengths (Nix store paths are max 211 chars)
- Graceful handling of unknown opcodes (close the connection after STDERR_ERROR; don't try to skip unknown payloads)

**Every count-prefixed loop must enforce `MAX_COLLECTION_COUNT`** before entering the loop — not just in `wire::read_strings`/`read_string_pairs`, but in any custom reader (e.g., `read_basic_derivation` output loop, `read_build_result` built-outputs loop, STDERR trace/field readers). Use the same pattern: `if count > wire::MAX_COLLECTION_COUNT { return Err(WireError::CollectionTooLarge(count)); }`.

### Error propagation

Every handler error path that returns `Err(...)` must send `STDERR_ERROR` to the client **first**. Never use bare `?` to propagate errors from store operations, NAR extraction, or ATerm parsing — always wrap in a match that sends `STDERR_ERROR` before returning. For batch opcodes like `wopBuildPathsWithResults`, per-entry errors should push `BuildResult::failure` and `continue`, not abort the entire batch.

### Stub opcodes

When adding a recognized opcode as a stub/no-op, **read the expected wire payload** and return `STDERR_LAST` + a neutral result (e.g., `u64(1)` for success, `u64(0)` for empty set). Never fall through to the "unimplemented" catch-all — that closes the connection, which breaks modern Nix clients that may send the opcode opportunistically (e.g., CA-related opcodes after a build).

### Local daemon subprocesses

When spawning `nix-daemon --stdio` for local build execution:
- Never `.unwrap()` on `daemon.stdin.take()` / `daemon.stdout.take()` — use `.ok_or_else()`
- Wrap all daemon communication in `tokio::time::timeout` (default: `DEFAULT_DAEMON_TIMEOUT` = 2h, configurable via `RIO_DAEMON_TIMEOUT_SECS` / `--daemon-timeout-secs` / `worker.toml`)
- Always `daemon.kill().await` in both success and error paths
- See `spawn_daemon_in_namespace` + `run_daemon_build` in `rio-worker/src/executor/daemon.rs` for the canonical pattern

## Observability Checklist

When adding metrics or tracing, verify end-to-end — don't just initialize the exporter:

- Metrics are actually **registered** (not just the exporter)
- Metric names match `observability.md` naming conventions (`rio_{component}_`)
- Gauges are decremented on cleanup (connection close, session end)
- Default log format is JSON, not pretty-printed
- Handlers have `#[instrument]` spans with meaningful fields
- Root span includes `component` field per structured logging spec
