# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

rio-build is an early-stage Rust project. It uses a Nix flake-based development environment with Crane for building Rust packages and protobuf for gRPC code generation.

## Quick Start

The dev environment is managed by Nix. If you have direnv, it activates automatically via `.envrc`.

```bash
# Enter the dev shell (if direnv isn't set up)
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
```

## Build System

- **Nix + Crane**: The flake.nix defines the full build pipeline. Crane handles dependency caching via `buildDepsOnly` and the actual build via `buildPackage`.
- **Cargo workspace**: Root Cargo.toml is a workspace; crates live in subdirectories.
- **Protobuf**: `.proto` files are included in the Crane source filter. `PROTOC` and `LIBCLANG_PATH` are set automatically in the dev shell.

## Key Commands via Nix

| Command | What it does |
|---|---|
| `nix build` | Build the workspace (release profile with thin LTO) |
| `nix flake check` | Run clippy, tests, doc check, and coverage |
| `nix fmt` | Same as `treefmt` |

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

## Design Book

This project has a comprehensive design book in `docs/src/`. When implementing any phase, cross-reference ALL relevant design docs — not just the phase plan:

- **Phase plan** (`docs/src/phases/phaseXY.md`): Task list and milestones
- **Component specs** (`docs/src/components/`): Protocol details, API contracts
- **Observability spec** (`docs/src/observability.md`): Metric names, log format, tracing structure
- **Crate structure** (`docs/src/crate-structure.md`): Expected modules and file layout
- **Architecture** (`docs/src/architecture.md`): System-level design

### Keeping docs and code in sync

When implementation reveals that a design doc is wrong (e.g., the spec says u32 but the real protocol uses u64), update the design doc in the same commit that fixes the code. Don't let them drift.

## Protocol Implementation Guidelines

rio-build implements the Nix worker protocol. Protocol work has specific pitfalls:

### Validate against the real implementation early

Don't trust specs or design docs alone. Run integration tests against a real `nix` client (e.g., `nix store info --store ssh-ng://localhost`) as soon as the handshake compiles. Protocol bugs found through integration are cheaper than protocol bugs found in review.

### Wire-level testing

Every opcode and wire primitive needs a byte-level test that constructs raw bytes, feeds them through the parser, and asserts the result. High-level integration tests are not sufficient — they hide framing and encoding bugs.

Include:
- Proptest roundtrips for all wire primitives (u64, bytes, bool, strings, collections)
- Live-daemon golden conformance tests: each test starts an isolated nix-daemon and compares responses at the byte level (see `tests/golden/`)
- Fuzz targets for wire parsing (`cargo-fuzz` in `rio-nix/fuzz/`)

### Safety bounds

Protocol parsers must enforce:
- Maximum collection sizes (prevent DoS via unbounded allocation)
- Maximum string/path lengths (Nix store paths are max 211 chars)
- Graceful handling of unknown opcodes (close the connection after STDERR_ERROR; don't try to skip unknown payloads)

## Observability Checklist

When adding metrics or tracing, verify end-to-end — don't just initialize the exporter:

- Metrics are actually **registered** (not just the exporter)
- Metric names match `observability.md` naming conventions (`rio_{component}_`)
- Gauges are decremented on cleanup (connection close, session end)
- Default log format is JSON, not pretty-printed
- Handlers have `#[instrument]` spans with meaningful fields
- Root span includes `component` field per structured logging spec
