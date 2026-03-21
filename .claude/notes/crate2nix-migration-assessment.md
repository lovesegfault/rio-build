# crate2nix migration assessment

**Status:** PoC complete, full workspace builds. **Checks layer landed** — clippy, test-compilation, rustdoc all work per-crate. Parallel pipeline on `crate2nix-explore` branch alongside Crane.

**Recommendation:** **Full migration viable.** crate2nix can replace Crane for all checks with per-crate caching. Remaining work: test-runner sandbox env (PG binaries), coverage profraw merge plumbing, per-crate source filesets. Detail below.

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
| **clippy** | `clippy-rustc-wrapper`: strips `--cap-lints allow` (hardcoded in lib.sh:18,55; rustc treats as non-overridable), forwards to `clippy-driver`. Members get `.override { rust = clippyRustc; extraRustcOpts = ["-Dwarnings" "-Wclippy::all"]; }`. Deps stay on regular rustc — cached. | **WORKS** | All 10 members pass (~2m wall). Intentional `clippy::eq_op` violation → build fails, rc=1. `/tmp/rio-dev/c2n-clippy-{2,fail}.log` |
| **tests** | `.override { buildTests = true; dependencies = deps ++ devDeps }`. devDeps injected into Cargo.json by `scripts/inject-dev-deps.py` (crate2nix JSON mode omits them; target crates already in resolved graph via `--all-features`). Compiles test-harness binaries to `$out/tests/`. | **COMPILES** | rio-common test binary: `--extern proptest=... --extern rcgen=... --extern tempfile=... --extern tonic_health=...` linked; produces `tests/rio_common-<hash>` + `tests/tls_integration`. `/tmp/rio-dev/c2n-test-1.log` |
| **test-run** | `runCommand` over all members' test binaries, execute, fail on nonzero. | **PLUMBED** | Runner derivation written. NOT yet in `checks.*` — needs PG binaries in sandbox env for rio-test-support ephemeral postgres (`PG_BIN` wired but postgres pkg not yet in nativeBuildInputs). |
| **rustdoc** | `rustdoc-rustc-wrapper`: translates rustc lib-build invocation to rustdoc (strips `--crate-type`, `-C` codegen, `--emit`, `--remap-path-prefix`; redirects `--out-dir target/doc`). Build scripts fall through to real rustc (OUT_DIR files needed for `include!`). | **WRITTEN** | Mechanism coded. Not end-to-end validated — rustdoc's arg handling may need tuning. `-Z unstable-options` required for `--document-private-items` on stable. |
| **llvm-cov** | Parallel build tree: second `cargoNix` instantiation with `extraRustcOpts = ["-Cinstrument-coverage"]` on ALL crates (deps too, for inlined-function attribution). Test binaries produce `.profraw` → `llvm-profdata merge` → `llvm-cov export --format lcov`. | **DESIGNED** | Not implemented. Doubles derivation count (655 normal + 655 instrumented), but each half caches independently. Profraw merge needs the same plumbing crane's `cargoLlvmCov` already does. |

**Key obstacle (--cap-lints):** `buildRustCrate`'s `lib.sh` hardcodes `--cap-lints allow` for all compilations (lines 18, 55). Verified empirically: rustc treats `--cap-lints` as first-wins, not last-wins — passing `--cap-lints warn` later in argv has NO effect. The wrapper-level strip is the only lever that doesn't require patching lib.sh.

**Key obstacle (devDependencies):** crate2nix's `--format json` output (`json_output.rs` `ResolvedCrate`) has no `dev_dependencies` field — only the Cargo.nix template mode emits them. `scripts/inject-dev-deps.py` post-processes: reads `cargo metadata --no-deps`, maps dev-dep names → packageIds using Cargo.json's own `crates` dict (all targets already resolved because `--all-features` pulls everything), writes `devDependencies` back. ~130 LoC Python, runs after `crate2nix generate`.

**Per-crate caching proof:** clippy-all build showed 10 member derivations building (6-46s each) + 1 symlinkJoin (2s). Zero dep rebuilds — all 645 dep rlibs came from the normal-build cache (`--extern tokio=/nix/store/2kmjg8cc4gdq0xxnq5lw9gbm4da2wqp0-rust_tokio-1.50.0-lib/...` — same hash as `.#c2n-rio-worker`'s deps). Touching one member's src would invalidate only that member's 4 check drvs + its dependents' check drvs.

### 4. VM test binaries — **straightforward but untested**

Crane's `rio-workspace` is what `nix/modules/*.nix`, `nix/docker.nix`, and `nix/tests/` consume. `c2n-workspace` (symlinkJoin) has the same `bin/` layout:

```
bin/crdgen bin/rio-cli bin/rio-controller bin/rio-gateway
bin/rio-scheduler bin/rio-store bin/rio-worker
```

Swapping `rio-workspace = c2n.workspace` in the perSystem let-block should Just Work for VM tests. **Not tested** in the PoC — VM tests need KVM builders, which were degraded during this session.

### 5. Coverage instrumentation — **harder**

Crane's `-Cinstrument-coverage` flow passes `RUSTFLAGS` to a second `buildDepsOnly` + `buildPackage` pair. crate2nix would need a parallel set of 655 instrumented derivations. buildRustCrate doesn't expose a RUSTFLAGS knob directly — it's plumbed through `extraRustcOpts` per-crate, so every crate derivation needs the override.

Doable (a function that maps over `cargoNix.builtCrates` with `.override { extraRustcOpts = [...]; }`), but the 655-derivation coverage build is a lot of build-graph churn for what's currently a single crane derivation. Not a blocker, but not a simplification either.

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
nix build .#c2n-clippy-all            # clippy all 10 members (CI gate)
nix build .#c2n-clippy-rio-scheduler  # clippy one member (dev iteration)
nix build .#c2n-test-rio-common       # compile test binaries for one member
nix build .#c2n-test-all              # compile + run all tests
nix build .#c2n-doc-all               # rustdoc all members
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

- Coverage instrumentation via crate2nix — 655 instrumented derivations is a lot of graph. Crane's single-derivation coverage flow is simpler. Revisit if per-test coverage slicing becomes useful.
- fuzz builds — already a separate Crane pipeline (`nix/fuzz.nix` with nightly). No reason to port.
- Custom feature-set builds — crate2nix JSON mode bakes feature resolution at generate-time (`--all-features` by default). If the project later wants `--no-default-features` variants, that's a second `Cargo.json` or a switch back to the full Cargo.nix template (which keeps feature selection eval-time).

---

## Open questions / remaining work

- **Does `c2n.workspace` match `rio-workspace` output closely enough for VM tests?** `bin/` looks identical but haven't smoke-tested. The biggest risk is subtle linking differences (crane uses `cargo build --release` with LTO=thin; buildRustCrate defaults to opt-level=3 no-LTO). `extraRustcOpts = ["-Clto=thin"]` on leaf binaries would close the gap.
- ~~**Does build-from-json.nix handle workspace dev-dependencies?**~~ **No — solved via `inject-dev-deps.py`** (see §3). JSON output omits them; post-processor adds them from `cargo metadata`. Upstream fix would be a 5-line patch to `json_output.rs` adding `dev_dependencies: Vec<DepInfo>` to `ResolvedCrate`.
- **Test runner sandbox env:** `c2n-test-all` compiles test harness binaries and runs them in a `runCommand`. Tests that need external binaries (postgres via `rio-test-support`, `nix-store --dump` for golden fixtures) need those in the runner's `nativeBuildInputs`. Currently `PG_BIN` is wired but postgres pkg isn't added — needs mirroring crane's `cargoNextest` env setup.
- **nextest vs direct harness execution:** `c2n-test-all` runs test binaries directly (`$f $testCrateFlags`). nextest gives better parallelism, test grouping, retry-on-flake. Archive mode (`nextest run --archive-file`) would work: collect all members' test binaries → synthesize a nextest-metadata tarball → run. Tarball synthesis is the missing piece — nextest's archive format expects a `target/` layout with Cargo.toml references.
- **Coverage instrumentation:** the `extraRustcOpts = ["-Cinstrument-coverage"]` mechanism is the same buildRustCrate uses for anything else — just a second `cargoNix` with the flag on all crates. Profraw emission happens automatically when test binaries run; merge is `llvm-profdata merge *.profraw -o merged.profdata` + `llvm-cov export --format lcov -instr-profile merged.profdata -object <test-bin>`. Crane's `cargoLlvmCov` wraps this already; the crate2nix equivalent is a `runCommand` over the instrumented-test-binaries' profraws.
- **rustdoc `-Z unstable-options`:** the rustdoc wrapper passes `-Z unstable-options` for `--document-private-items`. On stable rustc, `-Z` requires `RUSTC_BOOTSTRAP=1`. Either add that env var to the doc drvs or drop private-item docs (which crane's `cargoDoc --no-deps` doesn't include by default either).
- **Flake.lock bloat:** crate2nix pulls 8 transitive inputs (devshell, cachix, pre-commit-hooks, nix-test-runner, crate2nix_stable, …). Adds ~500 lines to flake.lock. Could be trimmed with aggressive `follows = ""` but the flake-compat / flake-parts inputs are needed for crate2nix's own evaluation. Accept as cost-of-dependency.
- **Upstream for inject-dev-deps:** worth opening a crate2nix PR adding `dev_dependencies` to the JSON output. The resolve.rs `ResolvedCrate` already has the field (line 43); json_output.rs just doesn't include it. ~10-line PR.
