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

# Full CI-equivalent checks (clippy, tests, docs, coverage)
nix flake check

# Fuzz a parser (default shell is nightly, so this works directly)
cd rio-nix/fuzz && cargo fuzz run wire_primitives
```

## Build System

- **Nix + crate2nix**: The flake.nix defines the full build pipeline. crate2nix generates per-crate derivations from `Cargo.lock` (JSON output mode — no `Cargo.nix` checked in), giving per-crate caching via nixpkgs' `buildRustCrate`.
- **Cargo workspace**: Root Cargo.toml is a workspace; crates live in subdirectories.
- **Protobuf**: `.proto` files are picked up by the crate2nix source filter. `PROTOC` and `LIBCLANG_PATH` are set automatically in the dev shell.

## Key Commands via Nix

| Command | What it does |
|---|---|
| `nix build` | Build the workspace (release profile with thin LTO) |
| `/nixbuild .#ci` | Full validation: build, clippy, nextest, doc, coverage, pre-commit, 2min fuzz ×9, all VM tests (Linux+KVM only) |
| `nix flake check` | Runs all `checks.*` (build, clippy, nextest, doc, coverage, 2min fuzz, VM tests) |
| `nix develop .#stable` | Dev shell with stable Rust (CI parity) |
| `nix build .#checks.x86_64-linux.tracey-validate` | Spec-coverage validation (r[...] annotation integrity) |
| `tracey query status` | Spec-coverage summary (in dev shell) |
| `nix fmt` | Same as `treefmt` |
| `/nixbuild .#coverage-full` | Combined unit+VM coverage (lcov+HTML, ~25min, needs KVM) |
| `/nixbuild .#cov-vm-protocol-warm-standalone` | Run one VM test in coverage mode (debugging, raw profraws at `result/coverage/`) |
| `nix build .#coverage-vm-protocol-warm-standalone` | Per-test lcov from one coverage-mode VM run |

### CI aggregate target

`/nixbuild .#ci` bundles all checks + VM tests + 2min fuzz into a single build. Needs KVM for the VM tests. On non-Linux, degrades to cargo checks + pre-commit only (VM tests and fuzz are both Linux-only). Result is a directory of symlinks to each constituent's output (`ls result/`).

### Coverage

Two tiers:

- **Unit-test only** (~5min): `nix build .#checks.x86_64-linux.coverage`. Output: `result/lcov.info`. HTML via `nix build .#coverage-html`.
- **Combined unit+VM** (~25min, manual, needs KVM): `/nixbuild .#coverage-full`. Output: `result/lcov.info` (combined), `result/html/`, `result/per-test/vm-<scenario>-<fixture>.lcov`. Fills the ~15% "permanently red" gap of VM-only code (FUSE callbacks, namespace setup, cgroup tracking, main.rs wiring, k8s lease/reconcilers, SSH accept loop). **Not** in `.#ci` — invoke on demand.

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
- **Always run `nix develop -c cargo nextest run` before committing** to catch regressions early.
- PostgreSQL integration tests bootstrap their own ephemeral postgres server (via `rio-test-support`) using `initdb`/`postgres` binaries from the dev shell. **No manual setup needed.** Tests panic (not skip) if postgres binaries are unavailable. Set `DATABASE_URL` to override with an external PG for debugging.
- Use semantic commit messages scoped by crate (e.g., `feat(rio-nix): add ATerm derivation parser`).
- **tracey MCP (optional):** `nix develop -c tracey ai --claude` registers the tracey MCP server + installs the annotation skill. After registration, Claude Code can query `tracey_uncovered` / `tracey_untested` / `tracey_rule` during dev sessions. The daemon caches scan results — `rm -rf .tracey/` to force rescan.

### `nix develop -c … 2>&1 | foo` hangs forever

The `ssh-ng://nxb-*` substituters spawn SSH ControlMaster daemons (`ssh nxb-prod -M -N -S /tmp/nix-<pid>-<rand>/ssh.sock`) that daemonize to PPID=1 and **inherit stderr (fd 2)** from the nix process. Nix redirects their stdin/stdout to /dev/null but not stderr. When you run `nix develop -c cmd 2>&1 | tail`, stderr becomes the pipe — the orphaned SSH daemons hold the write side open forever, so the pipe reader never sees EOF. `</dev/null` does **not** fix this (stdin isn't the problem).

**Safe patterns when piping `nix develop`/`nix build` output:**

```bash
# BAD — hangs when nix queries ssh-ng:// substituters
nix develop -c cargo build 2>&1 | tee log

# GOOD — pipe stdout only, stderr goes to terminal/parent
nix develop -c cargo build | tee log

# GOOD — stderr to a file, pipe stdout
nix develop -c cargo build 2>log.err | tee log.out

# GOOD — capture both without a pipe
nix develop -c cargo build &>log; tail log

# GOOD — process substitution (SSH inherits real fd, not the pipe)
nix develop -c cargo build > >(tee log) 2>&1
```

**Cleanup:** orphaned SSH masters accumulate (found 360 after two days). Periodic sweep:
```bash
pkill -f 'ssh nxb-(prod|dev) -M -N' 2>/dev/null
```

### Migration files are frozen after they ship

`sqlx::migrate!()` checksums `.sql` files by content (SHA-384 over the full file body, including comments). Editing a comment changes the checksum → persistent-DB deploys fail with `VersionMismatch`. Hit twice pre-production before P0353 froze it.

- **Commentary, rationale, history:** goes in `rio-store/src/migrations.rs` (per-migration `M_NNN` doc-consts). NOT in the `.sql`.
- **New migration:** add the SQL, run `cargo test -p rio-store --test migrations`, copy the hex-SHA from the `unpinned migration NNN` panic into `PINNED` at `rio-store/tests/migrations.rs`, commit both.
- **Behavior change to a shipped migration:** write a NEW migration. Never edit shipped ones. The checksum-freeze test (`migration_checksums_frozen`) fails CI on any content change.

## Plan-driven development

Work is granularized into plan docs at `.claude/work/plan-NNNN-*.md`. The DAG (deps, status, frontier) lives at `.claude/dag.jsonl` — typed `PlanRow` records, `onibus dag render` for display. File-collision matrix is live-computed via `onibus collisions top` / `onibus collisions check`.

**Every implementation MUST pass `/nixbuild .#ci` before merge.** This is the single gate — it covers build, clippy, nextest, docs, coverage, pre-commit, 2min fuzz, and all VM tests. "Done but CI red" is not done. **NEVER `nix build` locally** — 3 prior machine crashes; ALWAYS `/nixbuild .#ci` (see skill for mechanism).

### DAG runner workflow

Invoke `/dag-run` to become the coordinator. The loop: frontier → `/implement <N>` (≤10 parallel) → `/dag-tick` (mechanical reflex) → `/validate-impl` → `/review-impl` (post-PASS, advisory) → `/merge-impl` → coverage backgrounded → cadence agents every 5th/7th merge.

**State machine** (`.claude/bin/onibus` — grouped CLI, pydantic models + JSONL):

| File | Model | Written by | Consumed by |
|---|---|---|---|
| `dag.jsonl` | `PlanRow` | `rio-planner` via `onibus dag append`; merger via `onibus dag set-status` | frontier computation, `/dag-status` |
| `known-flakes.jsonl` | `KnownFlake` | `rio-ci-flake-fixer` (add/remove) | impl `.#ci` retry gate |
| `state/agents-running.jsonl` | `AgentRow` | coordinator via `onibus state agent-row` | `/dag-tick` scan |
| `state/merge-queue.jsonl` | `MergeQueueRow` | `/dag-tick` on PASS | coordinator merge ordering |
| `state/coverage-pending.jsonl` | `CoverageResult` | merger step 6 (backgrounded) | `/dag-tick` → test-gap followup |
| `state/followups-pending.jsonl` | `Followup` | `rio-impl-reviewer`, cadence agents | `/plan` promotion to plan docs |

**Agents** (`.claude/agents/`): `rio-implementer`, `rio-impl-validator`, `rio-impl-reviewer`, `rio-impl-merger`, `rio-planner`, `rio-plan-reviewer`, `rio-ci-fixer`, `rio-ci-flake-fixer`, `rio-ci-flake-validator`, `rio-impl-consolidator`, `rio-impl-bughunter`.

**Skills** (`.claude/skills/`): `/dag-run`, `/dag-tick`, `/dag-stop`, `/dag-status`, `/implement`, `/validate-impl`, `/review-impl`, `/merge-impl`, `/fix-impl`, `/plan`, `/check`, `/nixbuild`, `/bump-refs`.

**Coverage policy:** `.#ci` gates; `.#coverage-full` is backgrounded non-gating. Merger step 6 fires it in a subshell, writes `CoverageResult` to `coverage-pending.jsonl`. `/dag-tick` consumes: `exit_code≠0` → `test-gap` followup. Regression means "write a test", not "undo the merge."

**Cadence:** merge-count%5 → `rio-impl-consolidator` (duplication across last 5); %7 → `rio-impl-bughunter` (smell accumulation across last 7). Both write to followups sink with `origin` field; don't auto-flush — coordinator reviews.

Historical phase boundaries are git tags (`phase-1a`..`phase-4a`). Backfill plan docs (P0000-P0150, status=DONE) were generated from these ranges — onboarding-grade archaeology.

## Fuzzing

Fuzz targets live in per-crate `fuzz/` workspaces (excluded from the main workspace, separate `Cargo.lock` each). Currently: `rio-nix/fuzz/` (protocol/wire parsers) and `rio-store/fuzz/` (manifest parser). The default dev shell is nightly, so `cargo fuzz` works without extra setup:

```bash
nix develop -c bash -c 'cd rio-nix/fuzz && cargo fuzz run wire_primitives'
nix develop -c bash -c 'cd rio-store/fuzz && cargo fuzz run manifest_deserialize'
```

CI equivalent:
```bash
nix build .#checks.x86_64-linux.fuzz-wire_primitives  # 2min, in flake check
```

When adding a new parser, also add a fuzz target:
1. Add a `[[bin]]` entry in the relevant `fuzz/Cargo.toml` + target file in `fuzz_targets/`
2. Add seed inputs to `fuzz/corpus/<target>/` (must be prefixed `seed-`; NAR seeds: see `gen-nar-corpus.sh`)
3. Add the target to `fuzzTargets` in `nix/fuzz.nix` (target name + which `fuzzBuild` + `corpusRoot`)
4. If the fuzzed crate's deps changed, run `cd <crate>/fuzz && cargo update -p <crate>` (fuzz lockfile is independent)

## Design Book

This project has a comprehensive design book in `docs/src/`. When implementing a plan, cross-reference ALL relevant design docs — not just the plan doc:

- **Component specs** (`docs/src/components/`): Protocol details, API contracts
- **Observability spec** (`docs/src/observability.md`): Metric names, log format, tracing structure
- **Crate structure** (`docs/src/crate-structure.md`): Expected modules and file layout
- **Architecture** (`docs/src/architecture.md`): System-level design

### Keeping docs and code in sync

When implementation reveals that a design doc is wrong (e.g., the spec says u32 but the real protocol uses u64), update the design doc in the same commit that fixes the code. Don't let them drift.

### Spec traceability (tracey)

Normative requirements in `docs/src/` are marked with `r[domain.area.detail]` standalone paragraphs. Code that implements a requirement carries `// r[impl domain.area.detail]`; tests carry `// r[verify …]`. The CI check `tracey-validate` fails on broken references.

| Command | Use |
|---|---|
| `tracey query uncovered` | Spec rules with no `impl` annotation — unimplemented features |
| `tracey query untested` | Spec rules with `impl` but no `verify` — missing test coverage |
| `tracey query rule <id>` | See spec text + all impl/verify sites for one rule |
| `tracey query validate` | CI check — structural violations (e.g. `r[impl]` in test_include file); exits nonzero on error |
| `tracey query status` | Overall coverage summary |
| `tracey bump` | Bump version of staged requirements whose text changed (marks existing annotations stale) |

**When adding spec text that describes a new behavior or constraint:** add an `r[...]` marker (standalone paragraph, blank line before, col 0), then annotate the implementing code with `// r[impl ...]` and the test with `// r[verify ...]`. The marker-first discipline means `tracey query uncovered` surfaces unimplemented spec requirements immediately.

**VM-test `r[verify]` placement:** for NixOS VM tests under `nix/tests/`, place `# r[verify ...]` markers in `default.nix` at the `subtests = [...]` entry that wires the fragment — NOT in the scenario file's col-0 header block. A marker in a scenario header tells tracey the rule is tested; it does not tell tracey the fragment runs. A marker at the subtests entry structurally couples the two: no wiring → no marker → tracey catches it.

```nix
subtests = [
  # r[verify store.gc.tenant-retention]
  "gc-sweep"
  # r[verify worker.upload.references-scanned]
  # r[verify worker.upload.deriver-populated]
  # r[verify store.gc.two-phase]
  "refs-end-to-end"
];
```

Scenario-file header blocks MAY keep prose descriptions of what each marker covers (useful for humans); they MUST NOT carry the marker token itself. `config.styx`'s `test_include` is narrowed to `nix/tests/default.nix` only, so a stray marker in a scenario file is invisible to tracey — the rule stays listed as untested until properly wired.

**When spec text changes meaningfully:** run `tracey bump` before committing. This version-bumps the marker (e.g., `r[gw.opcode.foo]` → `r[gw.opcode.foo+2]`), making existing `r[impl gw.opcode.foo]` annotations stale until someone reviews and bumps them.

**tracey ≠ `TODO(P0NNN)`.** tracey answers "what does the spec say, what's covered, what's tested." `TODO(P0NNN)` answers "which plan owns this." A feature with a spec marker but no `r[impl]` shows up in `tracey query uncovered` — pair that with a `TODO(P0NNN)` pointing at the plan that will land it.

### Deferred work and TODOs

**Every deferred task must have a plan-tagged TODO comment.** Untagged TODOs accumulate into an untracked backlog.

Format: `TODO(P0NNN): <what>` where `P0NNN` is the plan number that will close the TODO.

```rust
// GOOD: tagged with the owning plan
// TODO(P0208): xmax-based inserted-check. Currently re-queries refcount
// post-upsert (race: concurrent PutPath can bump refcount between).

// GOOD: points to the blocking plan
// TODO(P0NNN): this path is unreachable under the escape-hatch
// config. Exercise it once the production path (see ADR-NNN) lands.

// BAD: no plan tag — nothing schedules this
// TODO: fix this later
```

**Finding the right plan:**
- Grep `.claude/work/plan-*.md` for the file/feature — the plan that touches it is usually the owner
- `.claude/bin/onibus dag render | grep <keyword>` for a quick title scan
- If NO plan exists, the work needs a plan first: write to `followups-pending.jsonl` via `onibus state followup`, or `/plan --inline`

**When to write a TODO:**
- You're implementing a stub/placeholder that a scheduled plan will fill in
- You're making a simplifying assumption that a scheduled plan will relax
- You're skipping something that's another plan's responsibility

**When NOT to write a TODO:**
- The deferred behavior is already in a `> **Scheduled:** [P0NNN](...)` block in the design doc — reference that instead
- No plan owns it — write a plan, don't leave an orphan TODO

**Audit periodically:** `grep -rn 'TODO[^(]' rio-*/src/` finds untagged TODOs (should be zero). `grep -rn 'TODO(P0NNN)' rio-*/src/` where the plan is DONE finds stale TODOs the plan should have closed.

**Format for explicitly-not-doing:** `WONTFIX(P0NNN): <why>` where `P0NNN` is the plan whose investigation concluded this won't be fixed. Unlike TODO, WONTFIX does NOT schedule future work — it's a grep-able marker that someone looked at this and decided against it. The plan doc contains the rationale.

```rust
// WONTFIX(P0310): ssh-ng client options are dropped client-side — Nix
// overrides setOptions() with an empty body (088ef8175, intentional).
// The accessor stays for future-proofing if rio ever advertises
// `set-options-map-only`; until then this path is unreachable.
```

Audit: `grep -rn 'WONTFIX[^(]' rio-*/src/` finds untagged WONTFIX (should be zero).

## Protocol Implementation Guidelines

See `.claude/rules/protocol-wire.md` (loads when editing `rio-gateway/src/**` or `rio-nix/src/{protocol,wire}/**`).

## Observability Checklist

When adding metrics or tracing, verify end-to-end — don't just initialize the exporter:

- Metrics are actually **registered** (not just the exporter)
- Metric names match `observability.md` naming conventions (`rio_{component}_`)
- Gauges are decremented on cleanup (connection close, session end)
- Default log format is JSON, not pretty-printed
- Handlers have `#[instrument]` spans with meaningful fields
- Root span includes `component` field per structured logging spec
