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
