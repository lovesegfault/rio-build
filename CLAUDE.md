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
| `nix-build-remote -- .#ci` | Full validation: build, clippy, nextest, doc, coverage, pre-commit, 2min fuzz ×8, all VM tests (Linux+KVM only) |
| `nix flake check` | Runs all `checks.*` (build, clippy, nextest, doc, coverage, 2min fuzz, VM tests) |
| `nix develop .#stable` | Dev shell with stable Rust (CI parity) |
| `nix build .#checks.x86_64-linux.tracey-validate` | Spec-coverage validation (r[...] annotation integrity) |
| `tracey query status` | Spec-coverage summary (in dev shell) |
| `nix fmt` | Same as `treefmt` |
| `nix-build-remote -- .#coverage-full` | Combined unit+VM coverage (lcov+HTML, ~25min, needs KVM) |
| `nix-build-remote -- .#cov-vm-phase1a` | Run one VM test in coverage mode (debugging, raw profraws at `result/coverage/`) |
| `nix build .#coverage-vm-phase1a` | Per-test lcov from one coverage-mode VM run |

### CI aggregate target

`nix-build-remote -- .#ci` bundles all checks + VM tests + 2min fuzz into a single build. Needs KVM for the VM tests. On non-Linux, degrades to cargo checks + pre-commit only (VM tests and fuzz are both Linux-only). Result is a directory of symlinks to each constituent's output (`ls result/`).

### Coverage

Two tiers:

- **Unit-test only** (~5min): `nix build .#checks.x86_64-linux.coverage`. Output: `result/lcov.info`. HTML via `nix build .#coverage-html`.
- **Combined unit+VM** (~25min, manual, needs KVM): `nix-build-remote -- .#coverage-full`. Output: `result/lcov.info` (combined), `result/html/`, `result/per-test/vm-phase*.lcov`. Fills the ~15% "permanently red" gap of VM-only code (FUSE callbacks, namespace setup, cgroup tracking, main.rs wiring, k8s lease/reconcilers, SSH accept loop). **Not** in `.#ci` — invoke on demand.

VM coverage architecture (`nix/coverage.nix`):

1. **Instrumented build** (`rio-workspace-cov`): `RUSTFLAGS="-C instrument-coverage"` + distinct pname → separate deps cache, builds in parallel with normal workspace.
2. **Coverage-mode VM tests** (`vmTestsCov`): same tests with `coverage=true` → `LLVM_PROFILE_FILE=/var/lib/rio/cov/rio-%p-%m.profraw` set in all rio-* service envs (`%p`=PID for restarts, `%m`=binary signature for safe merging).
3. **Graceful shutdown** (`rio-common::signal::shutdown_signal`): SIGTERM or SIGINT → CancellationToken → main() returns normally → atexit handlers fire → LLVM profraw flush. All binaries: store, scheduler, gateway, worker, controller.
4. **Collection** (`common.nix collectCoverage`): at end of each testScript, `systemctl stop rio-*` → graceful drain → tar `/var/lib/rio/cov` → `copy_from_vm` to `$out/coverage/<node>/profraw.tar.gz`. phase3a also deletes the k3s STS so the worker pod's hostPath-mounted profraws flush.
5. **Merge** (`nix/coverage.nix`): toolchain `llvm-profdata merge` → `llvm-cov export --lcov` → source-path normalize (strip sandbox prefix, anchor on `source/`) → `lcov -a` union with unit-test lcov → `--extract 'rio-*'` → genhtml.

Gotchas:
- Source-path normalization MUST match between unit and VM lcovs — both anchor on `source/` (crane's unpackPhase convention). If `lcov -a` merge shows doubled files with zero intersection, the strip regex is wrong.
- Use **toolchain** `llvm-profdata`/`llvm-cov` (`$(rustc --print sysroot)/lib/rustlib/<target>/bin/`), never system llvm. Profile format versioning is tied to rustc.

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

## Plan-driven development

Work is granularized into plan docs at `.claude/work/plan-NNNN-*.md`. The DAG (deps, status, frontier) lives at `.claude/dag.jsonl` — typed `PlanRow` records, `state.py dag-render` for display. File-collision matrix is derived into `.claude/collisions.jsonl` via `state.py collisions-regen`.

**Every implementation MUST pass `/nixbuild .#ci` before merge.** This is the single gate — it covers build, clippy, nextest, docs, coverage, pre-commit, 2min fuzz, and all VM tests. "Done but CI red" is not done. **NEVER `nix build` locally** — 3 prior machine crashes; ALWAYS `nix-build-remote --no-nom --dev -- -L` (the `/nixbuild` skill wraps this).

### DAG runner workflow

Invoke `/dag-run` to become the coordinator. The loop: frontier → `/implement <N>` (≤10 parallel) → `/dag-tick` (mechanical reflex) → `/validate-impl` → `/review-impl` (post-PASS, advisory) → `/merge-impl` → coverage backgrounded → cadence agents every 5th/7th merge.

**State machine** (`.claude/lib/state.py` — pydantic models + JSONL):

| File | Model | Written by | Consumed by |
|---|---|---|---|
| `dag.jsonl` | `PlanRow` | `rio-planner` via `dag-append`; merger via `dag-set-status` | frontier computation, `/dag-status` |
| `collisions.jsonl` | `CollisionRow` | `state.py collisions-regen` (derived) | `/implement` parallel-safety check |
| `known-flakes.jsonl` | `KnownFlake` | `rio-ci-flake-fixer` (add/remove) | impl `.#ci` retry gate |
| `state/agents-running.jsonl` | `AgentRow` | coordinator via `state.py agent-row` | `/dag-tick` scan |
| `state/merge-queue.jsonl` | `MergeQueueRow` | `/dag-tick` on PASS | coordinator merge ordering |
| `state/coverage-pending.jsonl` | `CoverageResult` | merger step 6 (backgrounded) | `/dag-tick` → test-gap followup |
| `state/followups-pending.jsonl` | `Followup` | `rio-impl-reviewer`, cadence agents | `/plan` promotion to plan docs |

**Agents** (`.claude/agents/`): `rio-implementer`, `rio-impl-validator`, `rio-impl-reviewer`, `rio-impl-merger`, `rio-planner`, `rio-plan-reviewer`, `rio-ci-fixer`, `rio-ci-flake-fixer`, `rio-ci-flake-validator`, `rio-impl-consolidator`, `rio-impl-bughunter`, `rio-backfill-clusterer`.

**Skills** (`.claude/skills/`): `/dag-run`, `/dag-tick`, `/dag-stop`, `/dag-status`, `/implement`, `/validate-impl`, `/review-impl`, `/merge-impl`, `/fix-impl`, `/plan`, `/check`, `/nixbuild`, `/bump-refs`.

**Coverage policy:** `.#ci` gates; `.#coverage-full` is backgrounded non-gating. Merger step 6 fires it in a subshell, writes `CoverageResult` to `coverage-pending.jsonl`. `/dag-tick` consumes: `exit_code≠0` → `test-gap` followup. Regression means "write a test", not "undo the merge."

**Cadence:** merge-count%5 → `rio-impl-consolidator` (duplication across last 5); %7 → `rio-impl-bughunter` (smell accumulation across last 7). Both write to followups sink with `origin` field; don't auto-flush — coordinator reviews.

Historical phase boundaries are git tags (`phase-1a`..`phase-4a`). Backfill plan docs (P0000-P0150, status=DONE) were clustered from these ranges by `rio-backfill-clusterer` — onboarding-grade archaeology, not forward plans.

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
3. Add the target to `fuzzTargets` in `flake.nix` (target name + which `fuzzBuild` + `corpusRoot`)
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
| `tracey query validate` | CI check — broken refs, duplicate IDs (exits 0 currently, CI greps for `0 total error(s)`) |
| `tracey query status` | Overall coverage summary |
| `tracey bump` | Bump version of staged requirements whose text changed (marks existing annotations stale) |

**When adding spec text that describes a new behavior or constraint:** add an `r[...]` marker (standalone paragraph, blank line before, col 0), then annotate the implementing code with `// r[impl ...]` and the test with `// r[verify ...]`. The marker-first discipline means `tracey query uncovered` surfaces unimplemented spec requirements immediately.

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
// TODO(P0286): this path is unreachable under privileged:true. Device
// plugin + hostUsers:false make it live.

// BAD: no plan tag — nothing schedules this
// TODO: fix this later
```

**Finding the right plan:**
- Grep `.claude/work/plan-*.md` for the file/feature — the plan that touches it is usually the owner
- `python3 .claude/lib/state.py dag-render | grep <keyword>` for a quick title scan
- If NO plan exists, the work needs a plan first: write to `followups-pending.jsonl` via `state.py followup`, or `/plan --inline`

**When to write a TODO:**
- You're implementing a stub/placeholder that a scheduled plan will fill in
- You're making a simplifying assumption that a scheduled plan will relax
- You're skipping something that's another plan's responsibility

**When NOT to write a TODO:**
- The deferred behavior is already in a `> **Scheduled:** [P0NNN](...)` block in the design doc — reference that instead
- No plan owns it — write a plan, don't leave an orphan TODO

**Audit periodically:** `grep -rn 'TODO[^(]' rio-*/src/` finds untagged TODOs (should be zero). `grep -rn 'TODO(P0NNN)' rio-*/src/` where the plan is DONE finds stale TODOs the plan should have closed.

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
- [ ] `tracey query rule gw.opcode.<name>` shows both `impl` (in `opcodes_*.rs`) and `verify` (in `wire_opcodes/` + `golden/`)
- [ ] Byte-level test in `wire_opcodes/` (constructs raw wire bytes, no SSH)
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
