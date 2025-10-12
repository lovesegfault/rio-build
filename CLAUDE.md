# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Quick Start Commands

This is a Rust workspace project with two binaries (`rio-dispatcher` and `rio-builder`) managed with Nix flakes.

```bash
# Enter development environment (using direnv, already configured)
direnv allow

# Or manually enter the Nix development shell
nix develop

# Build all workspace members
cargo build

# Build specific binary
cargo build -p rio-dispatcher
cargo build -p rio-builder

# Run tests
cargo test

# Run a specific test
cargo test test_name

# Check code without building
cargo check

# Run clippy linter
cargo clippy

# Format code (all files in project)
nix fmt

# Or format just Rust code
cargo fmt

# Watch for changes and rebuild
cargo watch -x check

# Run the dispatcher (with defaults)
cargo run -p rio-dispatcher

# Run the dispatcher with custom settings
cargo run -p rio-dispatcher -- --grpc-addr=0.0.0.0:50051 --ssh-addr=0.0.0.0:2222 --log-level=debug

# Run a builder (auto-detects platform)
cargo run -p rio-builder

# Run a builder with custom settings
cargo run -p rio-builder -- --dispatcher-endpoint=http://dispatcher:50051 --platforms=x86_64-linux,aarch64-linux --features=kvm

# View CLI help
cargo run -p rio-dispatcher -- --help
cargo run -p rio-builder -- --help
```

## Architecture Overview

**Project Type**: Distributed build service for Nix (open-source nixbuild.net alternative)

**Project Structure**:
- Cargo workspace with 3 crates:
  - `rio-dispatcher`: Fleet manager and SSH frontend (binary)
  - `rio-builder`: Worker node that executes builds (binary)
  - `rio-common`: Shared types and gRPC protocol definitions (library)

**Development Environment**:
- Managed by Nix flakes with flake-parts
- Uses rust-overlay for stable Rust toolchain with extensions
- Requires protobuf compiler (protoc) for gRPC code generation
- Pre-commit hooks configured via git-hooks.nix (cargo check, clippy)
- Multi-formatter setup with treefmt (nixfmt for Nix, rustfmt for Rust, taplo for TOML)

**Key Files**:
- `flake.nix`: Defines development environment, tools, and hooks
- `DESIGN.md`: Comprehensive architecture and design decisions
- `Cargo.toml`: Workspace manifest
- `crates/rio-common/proto/build_service.proto`: gRPC service definition

**Communication**:
- Client ↔ Dispatcher: SSH with Nix daemon protocol (using `nix-daemon` crate)
- Dispatcher ↔ Builder: gRPC (using `tonic`)

**Dependencies**:
- `nix-daemon` (0.1.x): Nix protocol implementation in Rust
- `russh`: SSH server/client
- `tonic`: gRPC framework
- `tokio`: Async runtime

## Development Best Practices

### Committing Code

**IMPORTANT: Always run `nix flake check` before committing!**
```bash
nix fmt && nix flake check
```

This runs:
- `cargo clippy --all-targets -- --deny warnings` (strict linting)
- `cargo test` (all tests)
- `cargo doc` (documentation check)
- `cargo tarpaulin` (code coverage)
- `treefmt` (formatting check)

**Commit Message Format**: Clean, professional commit messages. Do NOT add Claude Code branding or Co-Authored-By lines.

**TODO.md Updates**: Update TODO.md when meaningful progress is made (not on every commit). Remove "last updated" lines - TODO.md tracks implementation status, not commit counts.

### Dependencies

**Always use workspace dependencies!** All crate versions are defined in root `Cargo.toml` `[workspace.dependencies]`. In crate-specific Cargo.toml files, use:
```toml
dependency-name = { workspace = true }
```

**Keep dependencies up to date**: Run `cargo update --verbose` periodically to check for updates.

**Current versions** (as of this writing):
- tonic = "0.14" (with tonic-prost split)
- prost = "0.14"
- russh = "0.54.5" (no russh-keys needed, native async traits)
- tokio = "1.42"
- nix-daemon = "0.1"

### Error Handling

**Use `.with_context()` for file operations** to include file paths in error messages:
```rust
use camino::Utf8Path;  // Always use UTF-8 paths

tokio::fs::read(path)
    .await
    .with_context(|| format!("Failed to read file: {}", path))?;
```

### Observability

**Use `#[tracing::instrument]` on all public async functions:**
```rust
#[tracing::instrument(skip(self, large_data), fields(job_id = %job.id, platform = %job.platform))]
pub async fn enqueue(&self, job: BuildJob) -> JobId {
    // ...
}
```

- Use `skip()` for large/sensitive data
- Extract key identifiers as fields for correlation
- See METRICS.md for full observability strategy

### Testing

**Write tests for new functionality** - aim for 70%+ coverage:
```bash
cargo test -p rio-dispatcher module::tests
```

**Use tokio's built-in utilities** instead of hand-rolling:
- `StreamReader` for AsyncRead from Stream
- `SinkWriter` + `PollSender` + `CopyToBytes` for AsyncWrite from mpsc::Sender
- `ReceiverStream` for Stream from mpsc::Receiver

### Architecture Notes

**russh 0.54.5**: Uses native async traits (Rust 1.75+), no async-trait crate needed. Handler trait methods work with async fn declarations.

**tonic 0.14**: Prost functionality split into separate crates:
- Runtime: `tonic` + `tonic-prost`
- Build: `tonic-prost-build` (not `tonic-build`)
- Update build.rs: `tonic_prost_build::configure()...`

**nix-daemon crate**: Implement the `Store` trait and return `Progress` values. Use a simple helper struct that implements Progress for immediate returns.

## Development Gotchas

**Pre-commit Hook Check**: `pre-commit.check.enable` is now `true` in flake.nix. All commits must pass hooks.

**Rust Edition 2024**: This project uses Rust edition 2024 (via Cargo.toml `edition = "2024"`).

**Cross-Platform Targets**: The flake automatically configures the correct Rust target triple for your platform using `pkgs.hostPlatform.rust.rustcTarget`.

**Generated Files**: `.pre-commit-config.yaml` is auto-generated by git-hooks.nix and should not be edited manually. It's already in `.gitignore`.
- run cargo clippy before committing