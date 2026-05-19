#import "/lib/rio.typ": *
#show: rio.with(domains: none)

= Development Environment

rio-build uses a Nix flake-based development environment. If you have
#link("https://direnv.net/")[direnv] installed, the environment activates
automatically.

== Prerequisites

- #link("https://nixos.org/download/")[Nix] (2.20+ with `flakes` and
  `nix-command` enabled)
- Git

== Setup

```bash
git clone https://github.com/lovesegfault/rio-build.git
cd rio-build

# Enter the dev shell (if direnv isn't set up)
nix develop

# Verify the environment
cargo build
cargo nextest run
```

The dev shell provides: Rust toolchain (nightly by default so `cargo fuzz`
works; use `nix develop .#stable` for CI parity), `protoc`, `libclang`,
PostgreSQL binaries (`initdb`/`postgres` for ephemeral test databases),
`tracey` (spec-coverage tooling), formatters (`rustfmt`, `nixfmt`, `taplo`),
`treefmt`, and pre-commit hooks.

= Building and Testing

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

# Full validation (per-member clippy/doc/nextest, pre-commit, 2min fuzz, VM tests)
nix-fast-build --flake .#checks.x86_64-linux --remote root@<builder>
```

= CI Pipeline Tiers

#table(
  columns: (auto, auto, 1fr, auto, auto),
  align: (left, left, left, left, left),
  table.header([Tier], [Trigger], [Tests], [Aggregate target], [Time Budget]),
  [CI],
  [Every push],
  [Unit tests, functional tests (real rio-store), clippy, treefmt,
    live-daemon golden conformance tests, cargo-deny, 2min fuzz per
    target, VM integration tests],
  [`nix-fast-build .#checks.<system>`],
  [< 20 min],

  [On-demand],
  [Dev-initiated],
  [
    + mutation testing, EKS cluster tests, chaos tests, load tests
  ],
  [`.#mutants`, `xtask k8s qa`],
  [Unbounded],
)

== Mutation Testing

#table(
  columns: (auto, 1fr),
  align: (left, left),
  table.header([Invocation], [What]),
  [`cargo xtask mutants`],
  [Local run against `$PWD` with `--in-place`. Commit/stash first --- a `^C`
    mid-mutation can leave a mutated file behind. Results in
    `./mutants.out/`.],

  [`nix build .#mutants`],
  [Hermetic (vendored deps, pinned toolchain).
    `result/mutants.out/outcomes.json` + `result/{caught,missed}-count`.
    Week-over-week comparable.],

  [`cargo mutants --list --config .config/mutants.toml`],
  [Preview which mutations would be applied, without running them.],
)

= Code Style

- *Rust edition 2024* --- use the latest Rust idioms and features
- *Clippy `--deny warnings`* --- all warnings must be fixed before merge
- *Formatting* --- always run `nix fmt` before committing (pre-commit hooks
  run treefmt automatically)
- *Dependencies* --- dual-licensed under MIT OR Apache-2.0. Do not introduce
  GPL-3.0 dependencies into any crate (see
  #cross-link("/spec/components/proto.typ")[proto §Rationale])

= Commit Messages

Commits use #link("https://www.conventionalcommits.org/")[Conventional
  Commits] enforced by the `convco` pre-commit hook. Scope by crate or area:

```
feat(rio-nix): add ATerm derivation parser
fix(rio-builder): propagate BuildResult start_time/stop_time
docs(challenges): update FUSE timeout description
```

The scope regex only allows alphanumerics and `-`/`_`/`/` --- *no commas* in
the scope (use the broader scope or split into multiple commits).

= Pull Request Conventions

+ *Branch from `main`*, name branches descriptively (e.g.,
  `feat/nar-streaming`, `fix/handshake-padding`)
+ *Keep PRs focused* --- one logical change per PR
+ *Write tests* for new functionality. Protocol parsers must include
  property-based tests (`proptest`)
+ *Run `nix-fast-build --flake .#checks.x86_64-linux`* before opening a PR
  --- this runs all validation including VM tests (needs KVM --- use
  `--remote` for the remote builder)
+ *Update docs* if your change affects the design or configuration

= Project Structure

The workspace is split into #(refs.crate-count)() crates
(#(refs.crate-list)()) plus the `workspace-hack` hakari stub.
See #cross-link("/spec/system/crate-structure.typ")[Crate Structure] for the responsibilities
and module layout of each.

= Where to Start

Good first contributions:

- *Spec gaps* --- `tracey query uncovered` lists spec requirements with no
  implementation yet
- *Fuzzing targets* described in #cross-link("/spec/system/verification.typ")[Verification]
  --- wire format parsers are security-critical
- *Golden tests* --- add live-daemon conformance scenarios for new opcodes
  (see `rio-gateway/tests/golden/`)
- *Documentation* --- improvements to this design book (typos,
  clarifications, missing details)

= Architecture Overview

Before contributing code, read these docs in order:

+ #cross-link("/intro.typ")[Introduction] --- what rio-build is and isn't
+ #cross-link("/architecture.typ")[System Architecture] --- component diagram and
  data flow
+ #cross-link("/architecture.typ")[Architecture §Data Flows] --- step-by-step protocol sequences
+ The component doc for the area you're working on (e.g.,
  #cross-link("/spec/components/gateway.typ")[gateway],
  #cross-link("/spec/components/scheduler.typ")[scheduler])

= License

rio-build is dual-licensed under MIT OR Apache-2.0. By contributing, you
agree that your contributions will be licensed under the same terms.
