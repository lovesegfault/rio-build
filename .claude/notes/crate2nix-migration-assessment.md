# crate2nix migration assessment

**Status:** PoC complete, full workspace builds. Parallel pipeline landed on `crate2nix-explore` branch alongside Crane.

**Recommendation:** **Hybrid.** Migrate the release build + docker images to crate2nix; keep Crane for clippy/nextest/coverage/docs. Detail below.

---

## TL;DR

| | Crane | crate2nix (JSON mode) |
|---|---|---|
| Derivations | 2 (deps + workspace) | 655 (one per crate) |
| Cold build (rio-scheduler tree, 305 crates) | ~8min (monolithic) | ~4m40s (massively parallel) |
| Touch `rio-common/src/lib.rs` | full workspace rebuild | 10 workspace members + symlinkJoin (~90s) |
| Touch `rio-scheduler/src/actor.rs` | full workspace rebuild | 1 crate + symlinkJoin (~70s) *[with per-crate src fix]* |
| Touch `Cargo.lock` | `buildDepsOnly` rebuild | regenerate `Cargo.json` + affected crates |
| clippy/nextest/coverage | first-class wrappers | no equivalent (buildRustCrate is rustc-direct) |
| IFD | optional (via `inherit cargoArtifacts`) | optional (via `tools.appliedCargoNix`) |
| Checked-in generated file | none | `Cargo.json` (518 KB, git-friendly diffs) |

The incremental-rebuild win is real but **currently capped by source-granularity** (see §Source filesets below). Fixing that is a ~20-line upstream patch.

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
```

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

### 3. clippy / nextest / rustdoc / llvm-cov — **no crate2nix equivalent**

Crane's `craneLib.cargoClippy`, `cargoNextest`, `cargoDoc`, `cargoLlvmCov` are thin wrappers that run `cargo <tool>` inside a derivation with the prebuilt `cargoArtifacts` as a seed. They get you the full cargo diagnostic experience (error spans, `--fix` hints, nextest test grouping).

buildRustCrate invokes `rustc` directly — no cargo, no `[lints]` table, no nextest. Crates compile with `--cap-lints allow` (deliberate: you don't want a transitive dep's clippy pedantry blocking your build). There's no obvious place to run clippy on workspace members only.

**Implication:** crate2nix is a *build* backend, not a *check* backend. The hybrid recommendation keeps Crane's check wrappers.

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
| `libsqlite3-sys` | **bundled** (feature enabled via sqlx) | drop `bundled` feature (→ pkg-config probe) + `pkgs.sqlite` | vendored (see note) | sqlx's `sqlite` feature hard-enables `libsqlite3-sys/bundled`. Unbundling requires dropping `sqlite` from the workspace sqlx features — **we only use postgres anyway** (sqlite is a vestigial leftover from phase-1 prototyping). Cleaner fix: remove `"sqlite"` from `Cargo.toml` sqlx features, regenerate `Cargo.json`. |
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
```

**Followup:** drop `"sqlite"` from workspace `sqlx` features (one-line `Cargo.toml` change + `cargo update -p sqlx` + regenerate `Cargo.json`). Removes `libsqlite3-sys` from the tree entirely, saving ~20 s of bundled sqlite compilation per cold build.

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

## Recommendation: hybrid

**Keep Crane** for:
- `checks.clippy`, `checks.nextest`, `checks.doc`, `checks.coverage` — no crate2nix equivalent, and these want full-cargo-workspace semantics anyway
- `checks.deny` (cargo-deny is workspace-level)
- dev shells (`craneLib.devShell` is convenient)

**Migrate to crate2nix** for:
- `packages.default` (release binaries) — feed `c2n.workspace` into docker images, NixOS modules, VM tests
- per-component images (`docker-scheduler` only needs `c2n.members.rio-scheduler`, not the whole workspace — currently Crane builds all 10 binaries for every image)

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

## Open questions

- **Does `c2n.workspace` match `rio-workspace` output closely enough for VM tests?** `bin/` looks identical but haven't smoke-tested. The biggest risk is subtle linking differences (crane uses `cargo build --release` with LTO=thin; buildRustCrate defaults to opt-level=3 no-LTO). `extraRustcOpts = ["-Clto=thin"]` on leaf binaries would close the gap.
- **Does build-from-json.nix handle workspace dev-dependencies?** Probably — test fixtures compiled — but nextest-style workspace-test-runner doesn't map cleanly. The hybrid recommendation sidesteps this by keeping Crane for tests.
- **Flake.lock bloat:** crate2nix pulls 8 transitive inputs (devshell, cachix, pre-commit-hooks, nix-test-runner, crate2nix_stable, …). Adds ~500 lines to flake.lock. Could be trimmed with aggressive `follows = ""` but the flake-compat / flake-parts inputs are needed for crate2nix's own evaluation. Accept as cost-of-dependency.
