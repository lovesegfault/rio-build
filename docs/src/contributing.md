# Contributing

## Development Environment

rio-build uses a Nix flake-based development environment. If you have [direnv](https://direnv.net/) installed, the environment activates automatically.

### Prerequisites

- [Nix](https://nixos.org/download/) (2.20+ with `flakes` and `nix-command` enabled)
- Git

### Setup

```bash
git clone https://github.com/lovesegfault/rio-build.git
cd rio-build

# Enter the dev shell (if direnv isn't set up)
nix develop

# Verify the environment
cargo build
cargo nextest run
```

The dev shell provides: Rust toolchain (nightly by default so `cargo fuzz` works; use `nix develop .#stable` for CI parity), `protoc`, `libclang`, PostgreSQL binaries (`initdb`/`postgres` for ephemeral test databases), `tracey` (spec-coverage tooling), formatters (`rustfmt`, `nixfmt`, `taplo`), `treefmt`, and pre-commit hooks.

## Building and Testing

```bash
# Build
cargo build

# Run tests (prefer nextest for better output)
cargo nextest run

# Run a specific test
cargo nextest run test_name

# Lint (clippy enforces --deny warnings)
cargo clippy --all-targets -- --deny warnings

# Format (runs rustfmt + nixfmt + taplo via treefmt)
nix fmt

# Full local validation (build, clippy, nextest, doc, coverage, pre-commit, 30s fuzz smoke)
nix build .#ci-local-fast
```

## Code Style

- **Rust edition 2024** --- use the latest Rust idioms and features
- **Clippy `--deny warnings`** --- all warnings must be fixed before merge
- **Formatting** --- always run `nix fmt` before committing (pre-commit hooks run treefmt automatically)
- **Dependencies** --- dual-licensed under MIT OR Apache-2.0. Do not introduce GPL-3.0 dependencies into any crate (see [Architecture Decision #8](./decisions.md))

## Commit Messages

Commits use [Conventional Commits](https://www.conventionalcommits.org/) enforced by the `convco` pre-commit hook. Scope by crate or area:

```
feat(rio-nix): add ATerm derivation parser
fix(rio-worker): propagate BuildResult start_time/stop_time
docs(challenges): update FUSE timeout description
```

The scope regex only allows alphanumerics and `-`/`_`/`/` --- **no commas** in the scope (use the broader scope or split into multiple commits).

## Pull Request Conventions

1. **Branch from `main`**, name branches descriptively (e.g., `feat/nar-streaming`, `fix/handshake-padding`)
2. **Keep PRs focused** --- one logical change per PR
3. **Write tests** for new functionality. Protocol parsers must include property-based tests (`proptest`)
4. **Run `nix build .#ci-local-fast`** before opening a PR --- this bundles all local validation
5. **Update docs** if your change affects the design or configuration

## Project Structure

The workspace is split into 9 crates (`rio-common`, `rio-nix`, `rio-proto`, `rio-gateway`, `rio-scheduler`, `rio-store`, `rio-worker`, `rio-controller`, `rio-test-support`). See [Crate Structure](./crate-structure.md) for the responsibilities and module layout of each.

## Where to Start

Good first contributions:

- **Current phase tasks** in [phases/phase3b.md](./phases/phase3b.md), or grep the codebase for `TODO(phase3b)` / `TODO(phase4)` markers
- **Fuzzing targets** described in [verification.md](./verification.md) --- wire format parsers are security-critical
- **Golden tests** --- add live-daemon conformance scenarios for new opcodes (see `rio-gateway/tests/golden/`)
- **Documentation** --- improvements to this design book (typos, clarifications, missing details)

## Architecture Overview

Before contributing code, read these docs in order:

1. [Introduction](./introduction.md) --- what rio-build is and isn't
2. [System Architecture](./architecture.md) --- component diagram and data flow
3. [Data Flows](./data-flows.md) --- step-by-step protocol sequences
4. The component doc for the area you're working on (e.g., [gateway](./components/gateway.md), [scheduler](./components/scheduler.md))

## License

rio-build is dual-licensed under MIT OR Apache-2.0. By contributing, you agree that your contributions will be licensed under the same terms.
