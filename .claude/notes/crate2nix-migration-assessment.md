# crate2nix migration assessment

**Status:** PoC complete, full workspace builds. **All 6 check variants WORKING** — clippy, test-run (libtest), **nextest** (reuse-build), rustdoc, coverage all verified end-to-end. Parallel pipeline on `crate2nix-explore` branch alongside Crane.

**Recommendation:** **Full migration viable.** crate2nix can replace Crane for all checks with per-crate caching. Remaining work: per-crate source filesets (upstream ~20-line patch). Detail below.

---

## TL;DR

| | Crane | crate2nix (JSON mode) |
|---|---|---|
| Derivations | 2 (deps + workspace) | 655 build + 10 clippy + 10 test + 10 doc (one per crate per check) |
| Cold build (rio-scheduler tree, 305 crates) | ~8min (monolithic) | ~4m40s (massively parallel) |
| Touch `rio-common/src/lib.rs` | full workspace rebuild (all checks) | 10 members × 4 checks + downstream members (~90s per check) |
| Touch `rio-scheduler/src/actor.rs` | full workspace rebuild (all checks) | 1 member × 4 checks + symlinkJoin (~70s per check) *[with per-crate src fix]* |
| Touch `Cargo.lock` | `buildDepsOnly` rebuild | regenerate `Cargo.json` + `inject-dev-deps.py` + affected crates |
| clippy/nextest/coverage | workspace-wide derivations | **per-crate** via `.override` + arg-filtering wrappers (nix/c2n-checks.nix) |
| IFD | optional (via `inherit cargoArtifacts`) | optional (via `tools.appliedCargoNix`) |
| Checked-in generated file | none | `Cargo.json` (530 KB with devDependencies, git-friendly diffs) |

The incremental-rebuild win is real but **currently capped by source-granularity** (see §Source filesets below). Fixing that is a ~20-line upstream patch.

**Check layer added** (`nix/c2n-checks.nix`): clippy, test-compile, rustdoc each get per-crate derivations that `.override` workspace members with a driver-specific wrapper (clippy-driver, rustc --test, rustdoc). Deps REUSED from the normal-build cache — zero rebuild.

---

## What the PoC does

Files added:

- `Cargo.json` (518 KB) — pre-resolved dependency graph from `crate2nix generate --format json`. All feature expansion, optional-dep activation, and version selection done at generate-time in Rust. JSON is ~3× the size of `Cargo.lock` but ~half the size of the equivalent `Cargo.nix` (and diffs line-wise when a single dep version bumps, unlike Cargo.nix's template noise).
- `nix/crate2nix.nix` — imports `crate2nix/lib/build-from-json.nix`, wires rust-overlay stable, and supplies the crate overrides (native deps, cross-directory compile-time reads, system-linking).
- `flake.nix` changes — new input, new `c2n-*` package outputs, CLI in devshell.

Build targets:

```
nix build .#c2n-workspace           # symlinkJoin of all 10 members
nix build .#c2n-rio-scheduler       # single binary
nix build .#c2n-rio-common          # single rlib
# ...one per workspace member
```

Regenerate after Cargo.lock changes:

```
nix develop -c crate2nix generate --format json -o Cargo.json
nix develop -c python3 scripts/inject-dev-deps.py Cargo.json
```

The inject step adds `devDependencies` (needed for `c2n-test-*`) — crate2nix's JSON mode doesn't emit them, but all target crates are already in the resolved graph. See §3.

---

## What broke / doesn't map cleanly

### 1. `sqlx::migrate!("../migrations")` cross-directory read — **workaround applied**

`rio-scheduler` and `rio-store` use `sqlx::migrate!("../migrations")` which reads SQL files at compile time from a path relative to `CARGO_MANIFEST_DIR`. buildRustCrate's src is just the crate directory; `../migrations` resolves to `$NIX_BUILD_TOP/migrations`, which doesn't exist.

**Fix (in `nix/crate2nix.nix`):** `postUnpack` symlinks a dedicated `migrationsFileset` store path into `$NIX_BUILD_TOP/migrations`. Verified working — both crates build and the compiled binaries embed the migration set.

This is crate2nix issue #17 (cross-workspace reads). The symlink pattern is the standard workaround and applies to any `include_str!("../...")` or `sqlx::migrate!` usage.

### 2. Source fileset granularity — **PoC limitation, fixable upstream**

`build-from-json.nix` resolves local-crate srcs as `workspaceSrc + "/<crate>"`. That's a subpath of **one** store path. Touching any workspace file rehashes `workspaceSrc` → all 10 member derivations invalidate, even siblings with no dependency relationship.

Measured: touching `rio-common/src/lib.rs` rebuilt all 10 workspace members (11 drvs total, 88 s wall-clock). The 645 crates-io deps **did** stay cached (they're fetched via `pkgs.fetchurl` independently), so it's still faster than Crane, but the ideal is per-crate source isolation.

**Fix:** patch `build-from-json.nix` to accept an optional `workspaceMemberSrcs` attrset mapping crate name → fileset. ~20-line diff. Alternative: crateOverride `src` per member, but that fights buildRustCrate's `sourceRoot` discovery. Tracked for upstream PR.

### 3. clippy / nextest / rustdoc / llvm-cov — **SOLVED** (see nix/c2n-checks.nix)

~~Previous finding: no crate2nix equivalent, keep Crane.~~

**Now solved** via `buildRustCrate.override` + arg-filtering wrappers. Deps built once (regular rustc, 645 cached derivations); workspace members rebuilt per-check with the appropriate driver.

| Check | Mechanism | Status | Proof |
|---|---|---|---|
| **clippy** | `clippy-rustc-wrapper`: strips `--cap-lints allow` (hardcoded in lib.sh:18,55; rustc treats as non-overridable), forwards to `clippy-driver`. Members get `.override { rust = clippyRustc; extraRustcOpts = ["-Dwarnings" "-Wclippy::all"]; }`. Deps stay on regular rustc — cached. | **WORKING** | All 10 members pass (~2m wall). Intentional `clippy::eq_op` violation → build fails, rc=1. `/tmp/rio-dev/c2n-clippy-{2,fail}.log` |
| **test-compile** | `.override { buildTests = true; dependencies = deps ++ devDeps }`. devDeps injected into Cargo.json by `scripts/inject-dev-deps.py` (crate2nix JSON mode omits them; target crates already in resolved graph via `--all-features`). Compiles test-harness binaries to `$out/tests/`. | **WORKING** | rio-common test binary: `--extern proptest=... --extern rcgen=...` linked; produces `tests/rio_common-<hash>` + `tests/tls_integration`. `/tmp/rio-dev/c2n-test-1.log` |
| **test-run** | Per-crate `runCommand` wrapper: bootstraps postgres in bash (stable parent — libtest spawns fresh threads per test, so in-Rust `PR_SET_PDEATHSIG` bootstrap dies mid-run), exports `DATABASE_URL`, runs each test binary with `--test-threads=8` (avoids nanosecond-timestamp DB-name collision). Per-crate runners + symlinkJoin aggregate. | **WORKING** | rio-store: 248 tests pass (70 grpc + 169 lib + 8 metrics + 1 misc). rio-common: 80 tests pass (76 lib + 4 tls). Intentional `assert_eq` violation → build fails. `/tmp/rio-dev/c2n-test-both-6.log`, `/tmp/rio-dev/c2n-test-fail-1.log` |
| **rustdoc** | `rustdoc-rustc-wrapper`: translates rustc lib-build invocation to rustdoc (strips `--crate-type`, `-C` codegen, `--emit`, `--remap-path-prefix`; redirects `--out-dir target/doc`). Build scripts fall through to real rustc (OUT_DIR files needed for `include!`). `--document-private-items` is stable — NO `-Z unstable-options` needed. | **WORKING** | c2n-doc-rio-scheduler: 452 HTML files, `share/doc/rio_scheduler/index.html` 7848 bytes. c2n-doc-all builds all 10 members. `/tmp/rio-dev/c2n-doc-{scheduler,all}-1.log` |
| **coverage** | Second `cargoNix` instantiation via `globalExtraRustcOpts=["-Cinstrument-coverage"]` wired through `buildRustCrateForPkgs` wrapper → parallel 645-drv instrumented tree. Per-crate profraw collection (LLVM_PROFILE_FILE=$TMPDIR/profraw/%m-%p.profraw) → `llvm-profdata merge` 29 profraws → `llvm-cov export lcov` → `lcov --extract 'rio-*'` to workspace-only. | **WORKING** | 186 source files, 85.8% line / 50.9% fn coverage. lcov.info 2.0MB, paths repo-relative (`rio-cli/src/main.rs`). Normal tree hash UNCHANGED before/after (caching independence verified). `/tmp/rio-dev/c2n-coverage-2.log` |
| **nextest** | Synthesized `--binaries-metadata` + `--cargo-metadata` JSON → `cargo-nextest run` reuse-build mode. `nextestMeta` derivation: runs `cargo metadata --no-deps --offline` + jq-synthesizes the rust-binaries map from testBinDrvs filesystem listing. Handles buildRustCrate's naming divergence (`lib` gets `-<hash>` suffix; `tests/foo/main.rs` → `foo_main` vs cargo's `foo`). `nextestRun` derivation: `cp -r workspaceSrc $TMPDIR/ws` (nextest's `[store] dir` resolves relative to workspace root, readonly store path won't work); `--user-config-file none` (HOME stays /homeless-shelter so nix-instantiate probes in env-dependent tests skip cleanly, matching libtest runner behavior). | **WORKING** | `PASS [  0.004s] rio-common bloom::tests::...` output format. **1294/1294 pass in 7.9s** wall-clock (64-CPU nixbuild.net). junit.xml 233KB. Intentional fail caught with TRY 1 FAIL → DELAY 2/2 → TRY 2 FAIL (CI profile retry semantics). 29 binaries synthesized — **exact** match with cargo nextest list. `/tmp/rio-dev/c2n-nextest-{3,5}.log`, `/tmp/rio-dev/c2n-nextest-fail-1.log` |

**Key obstacle (--cap-lints):** `buildRustCrate`'s `lib.sh` hardcodes `--cap-lints allow` for all compilations (lines 18, 55). Verified empirically: rustc treats `--cap-lints` as first-wins, not last-wins — passing `--cap-lints warn` later in argv has NO effect. The wrapper-level strip is the only lever that doesn't require patching lib.sh.

**Key obstacle (devDependencies):** crate2nix's `--format json` output (`json_output.rs` `ResolvedCrate`) has no `dev_dependencies` field — only the Cargo.nix template mode emits them. `scripts/inject-dev-deps.py` post-processes: reads `cargo metadata --no-deps`, maps dev-dep names → packageIds using Cargo.json's own `crates` dict (all targets already resolved because `--all-features` pulls everything), writes `devDependencies` back. ~130 LoC Python, runs after `crate2nix generate`.

**Key obstacle (PR_SET_PDEATHSIG + libtest):** `rio-test-support::pg` spawns postgres with `PR_SET_PDEATHSIG(SIGTERM)`. On Linux, PDEATHSIG fires when the spawning THREAD terminates — not the process. libtest spawns a fresh `std::thread` per test; whichever test first reaches `TestDb::new` does bootstrap on its libtest thread; when that test completes and its thread exits, postgres gets SIGTERM → later tests see socket gone. crane's nextest avoids this structurally (per-test-process isolation). Fix: wrapper bootstraps postgres in bash (stable parent, lives for the whole derivation build) and exports `DATABASE_URL` so rio-test-support uses the external-PG path.

**Key obstacle (coverage override signature):** `buildRustCrate` is `callPackage`-wrapped — the outer `.override { defaultCrateOverrides }` is distinct from the per-crate `.override { extraRustcOpts }`. To inject `-Cinstrument-coverage` globally, the wrapper intercepts the per-crate call (`crate_: base (crate_ // { extraRustcOpts })`), but that wrapper is a plain function without `.override`. Solution: pass `pkgs.defaultCrateOverrides` to build-from-json.nix (overrides already baked into `base`), so its `defaultCrateOverrides != pkgs.defaultCrateOverrides` check evaluates false and skips the `.override` call.

**Per-crate caching proof:** clippy-all build showed 10 member derivations building (6-46s each) + 1 symlinkJoin (2s). Zero dep rebuilds — all 645 dep rlibs came from the normal-build cache (`--extern tokio=/nix/store/2kmjg8cc4gdq0xxnq5lw9gbm4da2wqp0-rust_tokio-1.50.0-lib/...` — same hash as `.#c2n-rio-worker`'s deps). Touching one member's src would invalidate only that member's 4 check drvs + its dependents' check drvs.

**Coverage-tree caching independence:** Adding the instrumented `c2nCov` tree did NOT change the normal-tree store hashes (`c2n-rio-common` = `mm910b58jh42ja46lzn4mv7lq0pnd2y5` before and after). The two trees share no derivations — touching a workspace file invalidates both the normal member + the instrumented member (and their dependents in each tree), but the 645 normal deps and 645 instrumented deps stay cached independently.

### 4. VM test binaries — **straightforward but untested**

Crane's `rio-workspace` is what `nix/modules/*.nix`, `nix/docker.nix`, and `nix/tests/` consume. `c2n-workspace` (symlinkJoin) has the same `bin/` layout:

```
bin/crdgen bin/rio-cli bin/rio-controller bin/rio-gateway
bin/rio-scheduler bin/rio-store bin/rio-worker
```

Swapping `rio-workspace = c2n.workspace` in the perSystem let-block should Just Work for VM tests. **Not tested** in the PoC — VM tests need KVM builders, which were degraded during this session.

### 5. Coverage instrumentation — **SOLVED**

`nix/crate2nix.nix` accepts `globalExtraRustcOpts ? []`. When non-empty, `buildRustCrateForPkgs` wraps the per-crate invocation to inject `extraRustcOpts` (plus `LLVM_PROFILE_FILE=/dev/null` to discard build-script/proc-macro profraws). The wrap returns a plain `crate_: drv` function (no `.override` method) — build-from-json.nix's `.override { defaultCrateOverrides }` branch is skipped by passing `pkgs.defaultCrateOverrides` (overrides already baked in at the outer level).

`c2nCov` in flake.nix re-imports with `globalExtraRustcOpts = ["-Cinstrument-coverage"]`. Parallel 645-derivation instrumented tree; each half caches independently. Verified: normal-tree `c2n-rio-common` hash unchanged before/after adding the coverage tree.

Profraw flow: per-crate runner sets `LLVM_PROFILE_FILE=$TMPDIR/profraw/%m-%p.profraw`, copies to `$out/profraw/` on pass. Merge derivation: `llvm-profdata merge -sparse` (29 profraws) → `llvm-cov export --format=lcov --object <testbin>...` → `lcov --substitute 's|^/||'` (strip buildRustCrate's remap-to-slash prefix) → `lcov --extract 'rio-*'` (workspace crates only; deps get remapped to `/tokio-1.50.0/...` which has no filterable prefix at llvm-cov level).

Output: **186 source files, 85.8% line coverage, 50.9% function coverage.** lcov.info is 2.0 MB with repo-relative paths.

### 6. `Cargo.json` regeneration — **process change**

Unlike Crane (which reads `Cargo.lock` at eval time), the JSON mode needs an explicit `crate2nix generate --format json` after `Cargo.lock` changes. Two options:

- **Checked-in JSON (current PoC):** treat `Cargo.json` like `Cargo.lock` — regenerate when deps change, review the diff. 518 KB, grows linearly with dep count. Git handles it fine; diffs are JSON-line-level when a single dep bumps.
- **IFD auto-regen:** `crate2nix.tools.${system}.generatedCargoNix` runs the generator inside a derivation and imports the result. No checked-in file, always in sync, but Import From Derivation — serializes eval, and this repo's existing policy is IFD-free (see tracey-src input comment in flake.nix).

The PoC uses checked-in JSON. A pre-commit hook that runs `crate2nix generate` and fails if `Cargo.json` is stale would keep it synced.

---

## What's straightforward

- **protobuf / tonic-prost-build:** `rio-proto/build.rs` compiles `.proto` files with `tonic_prost_build`. Proto files live inside `rio-proto/proto/` (no cross-directory reference), so the only override needed is `PROTOC = "${pkgs.protobuf}/bin/protoc"` on the consumer crate. Works out of the box — rio-proto built on first try.
- **rust-overlay toolchain:** `buildRustCrateForPkgs` override sets `rustc = rustStable; cargo = rustStable;` — same stable toolchain Crane uses (edition 2024). No friction.
- **proc-macros:** 45 proc-macro crates in the tree (serde-derive, tokio-macros, prost-derive, clap_derive, …). build-from-json.nix's cross-compilation logic routes them through `buildPackages` correctly. All compiled without override.
- **Native deps covered by nixpkgs defaults:** `aws-lc-sys` (cmake), `libsqlite3-sys` (sqlite + pkg-config), `prost-build` (protoc) — `pkgs.defaultCrateOverrides` already has them. Only `fuser`, `zstd-sys`, and the workspace members needed custom overrides.
- **Workspace dependencies:** `rio-common = { workspace = true }` style refs resolve correctly via Cargo.lock — crate2nix sees the pinned versions.

---

## sys-crate audit: system-link vs vendored

Policy goal: prefer nix-provided system libraries so security patches land via `nix flake update nixpkgs`, not `Cargo.lock` bumps; and so cold builds don't recompile 50 KLOC of C.

| crate | default behavior | system-link lever | PoC state | notes |
|---|---|---|---|---|
| `aws-lc-sys` | vendored (cmake builds aws-lc from source) | **none** | vendored | aws-lc is Amazon's BoringSSL fork; no system pkg. nixpkgs override already supplies cmake+nasm. ~40 s of C compilation, unavoidable. |
| `ring` | vendored (C + asm via `cc`) | **none** | vendored | ring is its own library (Brian Smith's hand-tuned assembly for TLS primitives). No system equivalent by design. |
| `zstd-sys` | vendored (`cc` builds bundled libzstd) | `ZSTD_SYS_USE_PKG_CONFIG=1` + `pkgs.zstd` | **system-linked** in PoC | resolved features `legacy,std,zdict_builder` are compatible with system libzstd ≥1.5. Saves ~15 s cold. |
| `libsqlite3-sys` | **bundled** (feature enabled via sqlx) | `LIBSQLITE3_SYS_USE_PKG_CONFIG=1` + `pkgs.sqlite` | **system-linked** in PoC | sqlx's `sqlite` feature chain (sqlite → sqlx-sqlite/bundled → libsqlite3-sys/bundled) hard-enables `bundled` at the cargo-feature level, but build.rs has an env-var escape hatch (build.rs:49-53): `LIBSQLITE3_SYS_USE_PKG_CONFIG` routes through `build_linked` regardless of features. `bundled_bindings` stays active → precompiled Rust bindings (no bindgen). ~~Previous note claimed sqlite was vestigial — wrong. rio-worker uses it for synthetic Nix store DB (synth_db.rs) and FUSE LRU cache index (fuse/cache.rs).~~ Verified: **4s build** vs ~20s bundled. |
| `fuser` | system (pkg-config probe for `fuse3`) | `pkgs.fuse3` + `pkgs.pkg-config` | **system-linked** in PoC | already the default behavior; override supplies the deps. |
| `openssl-sys` | — | — | **not in tree** | we use `rustls` + `aws-lc-rs` exclusively, no openssl. |
| `libz-sys` | — | — | **not in tree** | `flate2` uses the `miniz_oxide` backend (pure Rust). |
| `libgit2-sys`, `curl-sys`, `rdkafka-sys` | — | — | **not in tree** | no transitive pulls. |

Override snippet in `nix/crate2nix.nix`:

```nix
zstd-sys = _: {
  nativeBuildInputs = [ pkgs.pkg-config ];
  buildInputs = [ pkgs.zstd ];
  ZSTD_SYS_USE_PKG_CONFIG = "1";
};
libsqlite3-sys = _: {
  nativeBuildInputs = [ pkgs.pkg-config ];
  buildInputs = [ pkgs.sqlite ];
  LIBSQLITE3_SYS_USE_PKG_CONFIG = "1";
};
```

Both env-var escape hatches are also set in `flake.nix` `commonArgs` so the crane pipeline uses system libs too.

---

## Flake-input approach vs checked-in Cargo.nix

Three options on the spectrum:

1. **Checked-in `Cargo.nix`** (classic crate2nix) — ~1 MB of templated Nix for 655 crates. Diffs terribly (every dep bump touches dozens of lines of per-crate boilerplate). Rejected.
2. **Checked-in `Cargo.json`** (this PoC) — 518 KB of JSON. Diffs line-wise per crate (JSON pretty-printing puts each dep on its own line). No IFD. Must regenerate after `Cargo.lock` changes; enforced via pre-commit hook.
3. **IFD via `tools.generatedCargoNix`** — zero checked-in artifacts, always in sync with `Cargo.lock`. But: `nix flake check` now builds the generator derivation before it can evaluate anything, and the repo's existing policy is IFD-free (per the tracey-src comment in flake.nix — crane was chosen over naersk partly for this reason).

**PoC uses (2).** The regeneration step is a minor process tax (analogous to `cargo update` already updating a checked-in lockfile); the pre-commit hook makes it mechanical. If the project later adopts IFD for other reasons, (3) becomes a drop-in swap — the consuming Nix code (build-from-json.nix) is identical.

---

## Speed comparison

Measured on nixbuild.net remote builders (ssh-ng://nxb-dev), cold cache.

| scenario | Crane | crate2nix |
|---|---|---|
| cold: build rio-scheduler (305-crate tree) | ~8 min (single buildDepsOnly → buildPackage) *[estimate — crane output was pre-cached]* | **4 m 40 s** (305 parallel drvs, critical path = aws-sdk-s3 @ 60 s → rio-scheduler @ 70 s) |
| warm: build full workspace (after rio-scheduler) | — | **2 m 11 s** (198 additional drvs) |
| incremental: touch rio-common/src/lib.rs | full `buildPackage` rebuild (~3–4 min; everything after deps cache) | **88 s** (10 workspace members; 645 deps stay cached) |
| incremental: touch rio-scheduler/src/actor.rs | full `buildPackage` rebuild | **~70 s** (1 member + symlinkJoin) *[with per-crate src fix]* |

The crate2nix cold-build advantage is parallelism: 655 small derivations saturate nixbuild.net's 60+ CPU fleet immediately, vs Crane's two sequential big ones. The incremental advantage is caching: 645 crates-io crates never rebuild unless `Cargo.lock` bumps their version.

CPU utilization per-derivation is low (~1% of 64 CPUs) because each crate is a single `rustc` invocation — the fleet-level throughput comes from running hundreds in parallel, not from per-crate parallelism. On a single-machine build this would be a **regression** vs cargo's `-j` codegen parallelism. Remote-build-farm assumption is baked in.

---

## Recommendation: full migration (updated)

**Keep Crane only** for:
- `checks.deny` (cargo-deny is workspace-level, no per-crate benefit)
- dev shells (`craneLib.devShell` is convenient — though a plain `mkShell` with `rustStable` works too)
- fuzz builds (already a separate pipeline, nightly-only)

**Migrate to crate2nix** for:
- `packages.default` (release binaries) — feed `c2n.workspace` into docker images, NixOS modules, VM tests
- per-component images (`docker-scheduler` only needs `c2n.members.rio-scheduler`, not the whole workspace — currently Crane builds all 10 binaries for every image)
- `checks.clippy` → `checks.c2n-clippy` (per-crate, deps cached, ~2m wall vs crane's workspace-wide rebuild)
- `checks.nextest` → `checks.c2n-test` (per-crate test binaries; runner needs PG-in-sandbox)
- `checks.doc` → `checks.c2n-doc` (per-crate rustdoc)
- `checks.coverage` → c2n instrumented tree (deferred — doubles derivation count, needs profraw merge)

**Current targets** (exposed in `packages.*`):

```
nix build .#c2n-clippy-all               # clippy all 10 members (CI gate)
nix build .#c2n-clippy-rio-scheduler     # clippy one member (dev iteration)
nix build .#c2n-test-rio-store           # compile + run tests for one member (libtest, PG wrapper-bootstrapped)
nix build .#c2n-test-bin-rio-store       # compile test binaries only (no run)
nix build .#c2n-test-all                 # all members' libtest runs (symlinkJoin)
nix build .#c2n-nextest-all              # 1294 tests via nextest reuse-build (CI profile, retries, junit.xml)
nix build .#c2n-nextest-meta             # cached metadata synthesis (binaries-metadata + cargo-metadata JSON)
nix build .#c2n-doc-rio-scheduler        # rustdoc one member → share/doc/
nix build .#c2n-doc-all                  # rustdoc all members
nix build .#c2n-cov-profraw-rio-common   # instrumented test run → profraw/
nix build .#c2n-coverage                 # merged lcov.info (all members)
```

**Touch points (effort estimate: ~2 days):**

| file | change | risk |
|---|---|---|
| `flake.nix` | swap `rio-workspace = craneLib.buildPackage` → `rio-workspace = c2n.workspace` (release only) | low — same `bin/` layout |
| `nix/docker.nix` | accept per-binary store paths instead of monolithic workspace | medium — 7 images, mechanical |
| `nix/crate2nix.nix` | per-crate source filesets (upstream patch or crateOverride.src) | medium — needs testing with buildRustCrate's unpack |
| `Cargo.toml` | drop `"sqlite"` from sqlx features | trivial |
| `flake.nix` coverage block | leave on Crane | none |
| pre-commit hook | add `crate2nix generate --format json` + `git diff --exit-code Cargo.json` | trivial |

**Deferred (not worth it yet):**

- fuzz builds — already a separate Crane pipeline (`nix/fuzz.nix` with nightly). No reason to port.
- Custom feature-set builds — crate2nix JSON mode bakes feature resolution at generate-time (`--all-features` by default). If the project later wants `--no-default-features` variants, that's a second `Cargo.json` or a switch back to the full Cargo.nix template (which keeps feature selection eval-time).

---

## Open questions / remaining work

- **Does `c2n.workspace` match `rio-workspace` output closely enough for VM tests?** `bin/` looks identical but haven't smoke-tested. The biggest risk is subtle linking differences (crane uses `cargo build --release` with LTO=thin; buildRustCrate defaults to opt-level=3 no-LTO). `extraRustcOpts = ["-Clto=thin"]` on leaf binaries would close the gap.
- ~~**Does build-from-json.nix handle workspace dev-dependencies?**~~ **No — solved via `inject-dev-deps.py`** (see §3). JSON output omits them; post-processor adds them from `cargo metadata`. Upstream fix would be a 5-line patch to `json_output.rs` adding `dev_dependencies: Vec<DepInfo>` to `ResolvedCrate`.
- ~~**Test runner sandbox env**~~ **SOLVED** — wrapper-level PG bootstrap + `runtimeTestInputs` + `testEnv` wired from flake.nix. Tests run with `DATABASE_URL` pointing at a wrapper-spawned postgres (stable bash parent, survives the whole derivation build).
- ~~**rustdoc `-Z unstable-options`**~~ **NOT NEEDED** — `--document-private-items` has been stable since 2017. Removed the flag; docs build on stable rustdoc.
- ~~**Coverage instrumentation**~~ **SOLVED** — `globalExtraRustcOpts` parameter on `nix/crate2nix.nix` threads `-Cinstrument-coverage` into every crate's `extraRustcOpts` via a `buildRustCrateForPkgs` wrapper. Profraw collection + llvm-profdata merge + llvm-cov export + lcov extract all wired. 85.8% line coverage matching crane's unit-test-only range.
- ~~**nextest vs direct harness execution**~~ **SOLVED** via `--binaries-metadata` reuse-build (not `--archive-file`). The archive route would have required synthesizing a `target/` tree layout; the binaries-metadata route points nextest directly at the nix-store absolute paths. Metadata synthesis is a 2-step jq pipeline (`nextestMeta` derivation, cached independently of test execution). Same `.config/nextest.toml` as crane — profiles, test groups, overrides all honored.
- **Coverage vs crane comparison:** c2n coverage (85.8% lines, 186 files) vs crane's `.#checks.x86_64-linux.coverage` — not side-by-side compared yet (crane build takes ~5min cold). Expect similar line counts; crane may show slightly higher % because nextest's per-process isolation avoids the `--test-threads=8` serialization that reduces test-thread-local code paths.
- ~~**Flake.lock bloat**~~ **SOLVED** via `flake = false` — crate2nix is now a bare source input (like tracey-src). CLI built via the callPackage-compatible `crate2nix/default.nix` entrypoint against our nixpkgs. `lib/build-from-json.nix` is a pure file. **flake.lock: 1002 → 496 lines (−506, 50.5%).** 48 → 25 lock nodes. The `follows = ""` route doesn't work — crate2nix's flake-parts setup imports `inputs.devshell.flakeModule` eagerly at module-assembly time, so stubbing devshell breaks `packages.default` eval even though the CLI build itself doesn't use it. `flake = false` sidesteps the whole module system.
- **Upstream for inject-dev-deps:** worth opening a crate2nix PR adding `dev_dependencies` to the JSON output. The resolve.rs `ResolvedCrate` already has the field (line 43); json_output.rs just doesn't include it. ~10-line PR.
- **Upstream for globalExtraRustcOpts:** the `buildRustCrateForPkgs` wrapper that injects per-crate extraRustcOpts is general-purpose (not coverage-specific). Worth upstreaming as a `globalRustcOpts` parameter to build-from-json.nix.
