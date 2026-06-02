# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

rio-build is an early-stage Rust project. It uses a Nix flake-based development environment with crate2nix for building Rust packages and protobuf for gRPC code generation.

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

# Fast edit-loop checks — clippy/docs/drift/policy for all crates, no tests
.claude/bin/nixbuild

# Full CI gate (wraps nix-fast-build; serial alternative: nix flake check)
.claude/bin/nixbuild --checks

# Fuzz a parser (default shell is nightly, so this works directly)
cd fuzz/rio-nix && cargo fuzz run wire_primitives
```

## Build System

- **Nix + crate2nix**: The flake.nix defines the full build pipeline. crate2nix generates per-crate derivations from `Cargo.lock` (JSON output mode — no `Cargo.nix` checked in), giving per-crate caching via nixpkgs' `buildRustCrate`.
- **Cargo workspace**: Root Cargo.toml is a workspace; crates live in subdirectories.
- **Protobuf**: `.proto` files are picked up by the crate2nix source filter. `PROTOC` and `LIBCLANG_PATH` are set automatically in the dev shell.

## Key Commands via Nix

| Command | What it does |
|---|---|
| `/nixbuild .#default` | Build the workspace (release profile with thin LTO) |
| `/nixbuild --checks` | Full CI gate (wraps `nix-fast-build --flake .#checks.x86_64-linux`): per-member clippy/doc/nextest, pre-commit, 2min fuzz per target, all VM tests, cov-smoke (Linux+KVM only). Streams eval→build. |
| `/nixbuild` (= `--quick`) | Edit-loop slice of the gate: clippy/docs/drift/policy for all crates, **no test execution** (vm/nextest/fuzz/golden/mutants/cov-smoke excluded). Untouched crates are cache hits. |
| `nix flake check` | Runs all `checks.*` (same set as `/nixbuild --checks`, but serial eval) |
| `/nixbuild .#ci` | Everything the GHA pipeline builds (checks/fuzz/vm-test/coverage matrix kinds) as one target — exact CI replication. Per-kind/per-entry via passthru: `.#ci.vm-test`, `.#ci.checks.clippy-rio-nix`. Needs KVM; cached entries substitute. |
| `nix develop .#stable` | Dev shell with stable Rust (CI parity) |
| `/nixbuild .#checks.x86_64-linux.tracey-validate` | Spec-coverage validation (r[...] annotation integrity) |
| `tracey query status` | Spec-coverage summary (in dev shell) |
| `nix fmt` | Same as `treefmt` |
| `/nixbuild .#coverage` | Combined unit+VM coverage (lcov+HTML, ~25min, needs KVM) |
| `/nixbuild .#checks.x86_64-linux.cov-smoke` | Fast (~5min) one-scenario coverage-infra smoke |
| `/nixbuild .#coverage.vm-protocol-warm-standalone` | Per-entry lcov (one VM test or one `unit-<crate>`); `.raw` for the underlying coverage-mode VM run |
| `/nixbuild .#coverage.unit` / `.#coverage.vm` / `.#coverage.html` | Unit-only / VM-only lcov aggregates / HTML report |

### CI gate

`checks.*` is flat and granular: a per-member clippy/clippy-test/doc/nextest matrix for every workspace crate, plus fuzz runs, VM tests, and misc policy checks — each its own derivation. `nix-fast-build` (what `/nixbuild --checks` runs) streams evaluation into builds via nix-eval-jobs — VM tests start evaluating in parallel with rust checks instead of after, and individual check failures surface immediately without waiting for the whole graph.

`packages.*` is the minimal set of deployable artifacts (workspace binaries, docker images, AMIs, tfvars). Debug/manual targets (per-test coverage, fuzz builds, helm subcharts) hang off `packages.{ci,coverage,helm,dockerImages,mutants}` as passthru attrs — reachable by attr path, not enumerated by `nix flake show`.

`packages.ci` aggregates everything the GHA matrices build — `nix build .#ci` is the exact CI build set on one host (needs KVM), with per-kind (`.#ci.vm-test`) and per-entry (`.#ci.checks.clippy-rio-nix`) targets via passthru.

### Coverage

Three tiers:

- **Cov-smoke** (~5min, in checks): `/nixbuild .#checks.x86_64-linux.cov-smoke`. One representative VM scenario in coverage mode, asserts profraw→lcov pipeline produced non-empty data. Catches "coverage infrastructure broken" at merge-gate. A PSA break went 118 commits undetected before this was added — `.#coverage` failures were triaged as individual test-gaps instead of a pipeline-level halt.
- **Combined unit+VM** (~25min, needs KVM): `/nixbuild .#coverage`. Output: `result/lcov.info` (combined), `result/html/`, `result/per-test/vm-<scenario>-<fixture>.lcov`. HTML alone: `/nixbuild .#coverage.html`. Fills the ~15% "permanently red" gap of VM-only code (FUSE callbacks, namespace setup, cgroup tracking, main.rs wiring, k8s lease/reconcilers, SSH accept loop). **Not** a check — run on demand.

VM coverage architecture details: see `.claude/rules/coverage.md` (loads when editing `nix/coverage.nix`).

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
- **Devshell cargo builds go through [kache](https://github.com/kunobi-ninja/kache)** (`RUSTC_WRAPPER`): compiled artifacts are content-addressed and shared across worktrees via `~/.cache/rio-build/kache/<env-salt>/`. Local-only is enforced *inside* the wrapper script (allowlisted `KACHE_*` env + pinned config — survives any spawner, including xtask's `.env.local` reload), and the store epoch is salted by the toolchain/linker closure so restored binaries never reference GC'd store paths. Nix builds/CI are hermetic and unaffected. Opt out with `KACHE_DISABLED=1` in `.env.local` (free — no rebuild penalty). Debug unexpected misses with `kache why-miss <crate>`.
  - **Out-of-band macro inputs are tracked**: `.sqlx/` and `migrations/` are hashed into rustc env-deps by `rio-buildhash` build scripts, so both cargo and kache re-key when they change without `.rs` edits. If you add a macro that reads files invisible to dep-info, give its crate the same two-line `build.rs` + `const _: &str = env!(…)` pattern.
  - **Debugger paths**: cached compiles remap source prefixes to sentinels. Map back with gdb `set substitute-path <WORKSPACE> .` / lldb `settings set target.source-map <WORKSPACE> .`, or rebuild the crate with `KACHE_DISABLED=1`.
  - **Clippy flags aren't keyed** (kache v0.4.0): after changing lint flags, run once with `KACHE_DISABLED=1` or trust the hermetic `/nixbuild --checks` — a warm cache can replay the previous flags' verdict.
- **Always run `nix develop -c cargo nextest run` before committing** to catch regressions early.
- PostgreSQL integration tests bootstrap their own ephemeral postgres server (via `rio-test-support`) using `initdb`/`postgres` binaries from the dev shell. **No manual setup needed.** Tests panic (not skip) if postgres binaries are unavailable. Set `DATABASE_URL` to override with an external PG for debugging.
- Use semantic commit messages scoped by crate (e.g., `feat(rio-nix): add ATerm derivation parser`).
- **tracey MCP (optional):** `nix develop -c tracey ai --claude` registers the tracey MCP server + installs the annotation skill. After registration, Claude Code can query `tracey_uncovered` / `tracey_untested` / `tracey_rule` during dev sessions. The daemon caches scan results — `rm -rf .tracey/` to force rescan.

### Generated files (`cargo xtask regen`)

Several committed files are derived from source (`Cargo.json`, `workspace-hack/Cargo.toml`, `.sqlx/`, `infra/helm/crds/`, `docs/gen/`, `fuzz/*/Cargo.{json,lock}`, `rio-*/tests/fixtures/config-schema.json`, `infra/eks/generated.auto.tfvars.json`, …). Each has a `cargo xtask regen <subcommand>`; **`cargo xtask regen` with no subcommand runs all the idempotent regenerators in dependency order.** Run `cargo xtask regen --help` for the current subcommand list and what each owns.

CI catches stale files via per-file drift checks (`hakari-drift`, `crds-drift`, `docs-data-fresh`, the `crate2nix-check` / `hakari-check` / `sqlx-prepare-check` pre-commit hooks, …). A failing drift check names the regen command in its error message — when in doubt, run the no-subcommand umbrella before committing.

### Migration files are frozen after they ship

`sqlx::migrate!()` checksums `.sql` files by content (SHA-384 over the full file body, including comments). Editing a comment changes the checksum → persistent-DB deploys fail with `VersionMismatch`.

- **Commentary, rationale, history:** goes in `rio-migrations/src/migrations.rs` (per-migration `M_NNN` doc-consts). NOT in the `.sql`.
- **New migration:** add the SQL, run `cargo test -p rio-migrations --test migrations`, copy the hex-SHA from the `unpinned migration NNN` panic into `PINNED` at `rio-migrations/tests/migrations.rs`, commit both.
- **Behavior change to a shipped migration:** write a NEW migration. Never edit shipped ones. The checksum-freeze test (`migration_checksums_frozen`) fails CI on any content change.

### Config schemas are committed snapshots

`xtask regen docs-data` reads each binary crate's committed `tests/fixtures/config-schema.json` (a `{"schema": <schema_for!>, "defaults": <Config::default()>}` blob) instead of compiling rio-{gateway,builder,controller,store,scheduler}. The per-crate `tests/config_schema.rs` test (`rio_test_support::config_schema_frozen!`) fails CI when the fixture drifts from `Config`.

When you change a `Config` field (add/remove/rename, change a default, edit a doc comment that flows into the description column):

```bash
BLESS=1 cargo nextest run -E 'test(config_schema_frozen)'   # rewrites the per-crate fixture(s)
cargo xtask regen docs-data                                  # re-flattens docs/gen/config.json
```

Commit BOTH the regenerated fixture(s) AND `docs/gen/config.json`. The `docs-data-fresh` and `nextest-rio-X` checks each catch one half of forgetting.

## CI gate

**Every change MUST pass `/nixbuild --checks` before merge.** This is the single gate — it covers per-member clippy, nextest, docs, pre-commit, 2min fuzz, and all VM tests. "Done but CI red" is not done.

`/nixbuild --checks` wraps `nix-fast-build --flake .#checks.x86_64-linux` — it captures the log to `/tmp/rio-dev/`, emits a short report instead of streaming megabytes of build output, and exits with the underlying nix exit code. For a single check, `/nixbuild .#checks.x86_64-linux.<name>`; any other flake target works the same way (`/nixbuild .#<attr>`). During the edit loop, bare `/nixbuild` (= `--quick`) builds everything except test execution (clippy, docs, drift/policy) with untouched crates served from cache — useful for fast iteration, but **not** a substitute for the full gate. Prefer `/nixbuild` over raw `nix build` / `nix-fast-build` invocations; raw commands are for interactive debugging where you want live streaming output. Budget ~20min+ for `--checks` when the tree has substantive changes (seconds-to-minutes when mostly cached) and run it in the background from agent context — the report prints only at the end.

When the gate is red and the cause isn't obvious from the log, see `.claude/rules/ci-failure-patterns.md` — it catalogs every failure signature that has bitten this project before.

## Fuzzing

Fuzz targets live in per-crate `fuzz/<crate>` workspaces (excluded from the main workspace, separate `Cargo.lock` each). Currently: `fuzz/rio-nix/` (protocol/wire parsers) and `fuzz/rio-store/` (manifest parser). The default dev shell is nightly, so `cargo fuzz` works without extra setup:

```bash
nix develop -c bash -c 'cd fuzz/rio-nix && cargo fuzz run wire_primitives'
nix develop -c bash -c 'cd fuzz/rio-store && cargo fuzz run manifest_deserialize'
```

CI equivalent:
```bash
.claude/bin/nixbuild .#checks.x86_64-linux.fuzz-wire_primitives  # 2min, part of the checks gate
```

When adding a new parser, also add a fuzz target:
1. Add a `[[bin]]` entry in the relevant `fuzz/<crate>/Cargo.toml` + target file in `fuzz_targets/`
2. Add seed inputs to `fuzz/<crate>/corpus/<target>/` (must be prefixed `seed-`; NAR seeds: see `gen-nar-corpus.sh`)
3. Add the target to `fuzzTargets` in `nix/fuzz.nix` (target name + which `fuzzBuild` + `corpusRoot`)
4. If the fuzzed crate's deps changed, run `cargo xtask regen fuzz-lock` (fuzz lockfiles are independent of the main workspace)

## Design Book

This project has a comprehensive design book in `docs/` (typst sources). When implementing a feature, cross-reference ALL relevant design docs:

- **Component specs** (`docs/spec/components/`): Protocol details, API contracts
- **Observability spec** (`docs/spec/system/observability.typ`): Metric names, log format, tracing structure
- **Crate structure** (`docs/spec/system/crate-structure.typ`): Expected modules and file layout
- **Architecture** (`docs/architecture.typ`): System-level design

Render with `/nixbuild .#docs` (shiroa HTML, `result/index.html`) or `/nixbuild .#docs-pdf` (stitched PDF). The dev shell has `typst` and `shiroa` available for `typst watch docs/book-pdf.typ` / `shiroa serve docs/`.

### Keeping docs and code in sync

When implementation reveals that a design doc is wrong (e.g., the spec says u32 but the real protocol uses u64), update the design doc in the same commit that fixes the code. Don't let them drift.

### Spec traceability (tracey)

Normative requirements in `docs/spec/` are marked with `#r("domain.area.detail")[body]` function calls (the `#r` helper is exported from `/lib/rio.typ` and asserts the ID matches the file's declared `domains:`). Code that implements a requirement carries `// r[impl domain.area.detail]`; tests carry `// r[verify …]`. The CI check `tracey-validate` fails on broken references.

| Command | Use |
|---|---|
| `tracey query uncovered` | Spec rules with no `impl` annotation — unimplemented features |
| `tracey query untested` | Spec rules with `impl` but no `verify` — missing test coverage |
| `tracey query rule <id>` | See spec text + all impl/verify sites for one rule |
| `tracey query validate` | CI check — structural violations (e.g. `r[impl]` in test_include file); exits nonzero on error |
| `tracey query status` | Overall coverage summary |
| `tracey bump` | Bump version of staged requirements whose text changed (marks existing annotations stale) |

**When adding spec text that describes a new behavior or constraint:** add an `#r("...")[body]` call (the `body` is the normative MUST sentence; rationale prose goes *after* the block, not inside it), then annotate the implementing code with `// r[impl ...]` and the test with `// r[verify ...]`. The marker-first discipline means `tracey query uncovered` surfaces unimplemented spec requirements immediately.

**VM-test `r[verify]` placement:** for NixOS VM tests under `nix/tests/`, place `# r[verify ...]` markers in `default.nix` at the `subtests = [...]` entry that wires the fragment — NOT in the scenario file's col-0 header block. A marker in a scenario header tells tracey the rule is tested; it does not tell tracey the fragment runs. A marker at the subtests entry structurally couples the two: no wiring → no marker → tracey catches it.

```nix
subtests = [
  # r[verify store.gc.tenant-retention]
  "gc-sweep"
  # r[verify builder.upload.references-scanned]
  # r[verify builder.upload.deriver-populated]
  # r[verify store.gc.two-phase]
  "refs-end-to-end"
];
```

Scenario-file header blocks MAY keep prose descriptions of what each marker covers (useful for humans); they MUST NOT carry the marker token itself. `config.styx`'s `test_include` is narrowed to `nix/tests/default.nix` only, so a stray marker in a scenario file is invisible to tracey — the rule stays listed as untested until properly wired.

**When spec text changes meaningfully:** run `tracey bump` before committing. This version-bumps the marker (e.g., `#r("gw.opcode.foo")` → `#r("gw.opcode.foo+2")`), making existing `r[impl gw.opcode.foo]` annotations stale until someone reviews and bumps them. `tracey bump` works on `.typ` sources.

### Deferred work

Mark deferred work with a plain `// TODO:` comment that says *what* and *why* in enough detail that someone else could pick it up. Mark explicit non-goals with `// WONTFIX:` and the rationale inline. Existing `TODO(P0NNN)`/`WONTFIX(P0NNN)` tags reference historical plan docs; the plan number is archaeology — `git log -S P0NNN` finds the relevant commits.

## Protocol Implementation Guidelines

See `.claude/rules/protocol-wire.md` (loads when editing `rio-gateway/src/**` or `rio-nix/src/{protocol,wire}/**`).

## Observability Checklist

When adding metrics or tracing, verify end-to-end — don't just initialize the exporter:

- Metrics are actually **registered** (not just the exporter)
- Metric names match `observability.typ` naming conventions (`rio_{component}_`)
- Gauges are decremented on cleanup (connection close, session end)
- Default log format is JSON, not pretty-printed
- Handlers have `#[instrument]` spans with meaningful fields
- Root span includes `component` field per structured logging spec
