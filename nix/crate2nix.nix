# crate2nix JSON-mode — per-crate derivation graph.
#
# See .claude/notes/crate2nix-migration-assessment.md for the full
# migration history. Uses the experimental `--format json` output:
# feature/platform resolution happens in Rust (crate2nix generate),
# Nix is a thin consumer that wires pre-resolved crate records to
# pkgs.buildRustCrate. One derivation per crate → touching
# rio-scheduler/src/ rebuilds only rio-scheduler + its dependents,
# not the 400+ transitive deps.
#
# The `Cargo.json` at repo root is produced by:
#   nix develop -c bash -c \
#     'crate2nix generate --format json -o Cargo.json'
# (crate2nix is in the dev shell once the flake input is added.)
#
# It must be regenerated whenever Cargo.lock changes (new deps, version
# bumps). No IFD-based auto-regen — the JSON mode explicitly trades
# that convenience for simpler/faster eval.
{
  pkgs,
  lib,
  # rust-overlay toolchain (edition 2024). nixpkgs' packaged rustc
  # lags; build-from-json.nix plumbs this through to every
  # buildRustCrate invocation. Main workspace + coverage use stable;
  # fuzz uses nightly (libfuzzer-sys + -Zsanitizer).
  rust,
  # The crate2nix flake source tree. We only need
  # lib/build-from-json.nix from it (packages.default is the CLI).
  crate2nixSrc,
  # Workspace root — must match what `crate2nix generate` ran against.
  # Fileset-filtered to avoid rebuilds on .claude/ or doc churn.
  workspaceSrc,
  # Per-crate src isolation. Maps crateName → store path (the result
  # of `lib.fileset.toSource`). Local crates not listed fall back to
  # build-from-json's `workspaceSrc + "/<source.path>"` resolution.
  memberSrcs ? { },
  # Path to the pre-resolved JSON (checked in at repo root).
  resolvedJson ? ../Cargo.json,
  # Whether to strip binaries in workspaceBins/memberBins. Coverage
  # builds set false so __llvm_covfun/__llvm_covmap survive.
  stripBins ? true,
  # sys-crate env-var escape hatches + system libs. Passed from
  # flake.nix's sysCrateEnv — single source of truth so devShell
  # and crate2nix see the same linkage.
  sysCrateEnv,
  # Extra rustc flags injected into EVERY crate in the tree, deps
  # included. A non-empty value forks the whole dep graph into a
  # second derivation set, so nothing uses this today — instrumented
  # trees (coverage, fuzz) restrict their flags to local crates to
  # keep the dep derivations shared with the plain tree. Kept as the
  # escape hatch for a flag that genuinely must reach every crate.
  # Empty = no wrap.
  globalExtraRustcOpts ? [ ],
  # Extra rustc flags applied only to crates listed in `memberSrcs`
  # (= local in-tree crates). Dep derivations stay byte-identical to
  # the uninstrumented tree's, so the store shares them. Two users:
  #
  # - fuzz: sancov+asan. cargo-fuzz instruments EVERYTHING via
  #   RUSTFLAGS, which buildRustCrate can't replicate (no host/target
  #   split, so build-deps and proc-macros would also be instrumented
  #   and then fail to link/load without the asan runtime).
  #   Restricting to local crates is the unit-fuzz compromise —
  #   libFuzzer's coverage signal comes from the code under test
  #   (rio-*), not from serde/tokio.
  # - coverage: -Cinstrument-coverage. The lcov pipeline extracts
  #   rio-* paths only, so dep coverage data would be discarded at
  #   report time anyway.
  localExtraRustcOpts ? [ ],
}:
let
  # ──────────────────────────────────────────────────────────────────
  # Toolchain
  # ──────────────────────────────────────────────────────────────────
  #
  # buildRustCrate uses pkgs.rustc/pkgs.cargo by default. Override to
  # rust-overlay stable so edition 2024 compiles. `rust` is for the buildRustCrate runtime tooling (lib.rs
  # path discovery scripts etc.); `rustc`/`cargo` are the actual
  # compilers.
  #
  # --remap-path-prefix rewrites the toolchain store path to a stable
  # placeholder in everything rustc emits (debug info source paths,
  # panic-location strings, file!() macro expansions). Without this,
  # binaries embed `/nix/store/...-rust-stable-.../lib/rustlib/src/...`
  # literals and Nix's reference scanner pulls the ~2.3GB toolchain
  # into the closure. With remapping, those references never exist —
  # this alone collapses the closure to glibc + gcc-lib + system libs.
  # RUNPATH is unaffected but stdenv's fixupPhase already shrinks it
  # to glibc/lib:gcc-lib/lib (rust-overlay's rustlib doesn't appear).
  #
  # When a crate is compiled with -Cinstrument-coverage (via either
  # opts list), also set LLVM_PROFILE_FILE=/dev/null: its build script
  # and any proc-macro it exports are ALSO instrumented and would
  # otherwise try to write profraws to the RO sandbox CWD when they
  # execute during a build. Gated on the specific flag — not on
  # "localExtraRustcOpts is non-empty" — so (a) the fuzz tree's
  # sancov-only members don't pick up a pointless env var, and (b) a
  # local-only coverage tree leaves its dep derivations byte-identical
  # to the uninstrumented tree's (one extra env var on a dep would
  # change its hash and forfeit the store sharing).
  #
  # The wrap returns a plain `crate_: drv` function — build-from-json.nix's
  # `.override { defaultCrateOverrides }` branch must be skipped for
  # this to work; we arrange that by NOT passing our custom overrides
  # to build-from-json.nix (they're already baked into `base` here).
  remapOpts = [ "--remap-path-prefix=${rust}=/rustc" ];

  # Crates that exist in the workspace ONLY as build-dependencies (their
  # rlibs link into build_script_build binaries, which buildRustCrate
  # compiles WITHOUT extraRustcOpts). Excluded from instrumentation
  # flags in buildRustCrateForPkgs below.
  buildDepOnlyCrates = [ "rio-buildhash" ];

  buildRustCrateForPkgs =
    cratePkgs:
    let
      base = cratePkgs.buildRustCrate.override {
        rustc = rust;
        cargo = rust;
        inherit defaultCrateOverrides;
      };
    in
    crate_:
    let
      # cargo-hakari's job is feature unification at LOCK time. crate2nix
      # reads Cargo.lock directly (features already baked into each dep's
      # `resolvedDefaultFeatures`), so building workspace-hack's 116 deps
      # is pure overhead — every leaf already builds the deps it actually
      # uses, with the unified feature set, from the lock. Stub it to
      # zero deps so per-crate targets don't drag in the whole workspace
      # closure: `.#rio-builder` 491→344 rust drvs, `.#rio-nix` 429→87.
      # docker images consume `memberBins` per-component, so the win
      # carries through to `.#dockerImages.builder` etc.
      #
      # NOTE: this must intercept `crate_` here, not via
      # `defaultCrateOverrides` below — buildRustCrate threads
      # `dependencies`/`buildDependencies`/`src` through makeOverridable
      # defaults from the original `crate_` (build-rust-crate
      # default.nix:506-507), so the crateOverrides merge at
      # default.nix:238 never reaches them.
      #
      # Per-crate src: build-from-json.nix resolves local members as
      # `workspaceSrc + "/<name>"` — a subpath of ONE store hash, so
      # any workspace edit invalidates every member. Replace with the
      # per-member fileset from flake.nix; content-identical, hash-
      # independent. memberFilesets keys must match Cargo.json's
      # source.path (= crate dir name, which == crateName here).
      isLocal = memberSrcs ? ${crate_.crateName};
      # memberSrcs are per-crate fileset.toSource outputs, which are
      # always named `<hash>-source` → stdenv unpacks to
      # $NIX_BUILD_TOP/source/ → buildRustCrate's
      # `--remap-path-prefix=$NIX_BUILD_TOP=/` produces `/source/src/…`
      # in debuginfo/coverage maps, losing the crate name. Remap the
      # crate-specific dir to `/<crateName>` so the existing lcov
      # `s|^/||` + `--extract 'rio-*'` pipeline yields repo-relative
      # paths (`rio-scheduler/src/foo.rs`). extraRustcOpts come AFTER
      # buildRustCrate's baseRustcOpts on the rustc argv, and rustc
      # applies remaps last-match-wins, so this more-specific prefix
      # takes effect for local source while deps still get
      # `<name>-<ver>/…` from the base remap.
      localRemap = "--remap-path-prefix=$NIX_BUILD_TOP/source=/${crate_.crateName}";
      crate_' =
        if crate_.crateName == "workspace-hack" then
          crate_
          // {
            dependencies = [ ];
            buildDependencies = [ ];
            src = memberSrcs.workspace-hack or crate_.src;
          }
        else if isLocal then
          crate_ // { src = memberSrcs.${crate_.crateName}; }
        else
          crate_;
    in
    base (
      crate_'
      // {
        extraRustcOpts =
          remapOpts
          # Crates consumed exclusively as build-dependencies are HOST
          # artifacts: buildRustCrate compiles build_script_build without
          # extraRustcOpts, so instrumentation must not leak into the
          # rlibs those build scripts link. An asan/sancov rlib (fuzz
          # tree's localExtraRustcOpts) fails the build-script link with
          # undefined __asan_*/__sanitizer_cov_* — the binary never gets
          # the sanitizer runtime — and coverage instrumentation is dead
          # weight at best. Same reasoning cargo itself applies by never
          # passing RUSTFLAGS to host units.
          ++ lib.optionals (!lib.elem crate_.crateName buildDepOnlyCrates) (
            globalExtraRustcOpts ++ lib.optionals isLocal localExtraRustcOpts
          )
          ++ lib.optionals isLocal [ localRemap ]
          ++ (crate_'.extraRustcOpts or [ ]);
      }
      //
        lib.optionalAttrs
          (lib.elem "-Cinstrument-coverage" (
            globalExtraRustcOpts ++ lib.optionals isLocal localExtraRustcOpts
          ))
          {
            # Discard build-time profraws. Test runners override at
            # runtime to collect real data.
            LLVM_PROFILE_FILE = "/dev/null";
          }
    );

  # ──────────────────────────────────────────────────────────────────
  # Crate overrides
  # ──────────────────────────────────────────────────────────────────
  #
  # nixpkgs ships pkgs.defaultCrateOverrides which already covers
  # aws-lc-sys, libsqlite3-sys, prost-build, openssl-sys. We extend for
  # crates not in that set and for cross-directory compile-time
  # references that crate2nix's per-crate-src model can't see.
  #
  # `sqlx::migrate!("./migrations")` lives only in `rio-migrations` and
  # reads from inside its own crate dir, so no migration symlink hack
  # is needed — the per-member fileset already includes `migrations/`.
  #
  # sqlx offline query cache — content-addressed JSON per query!(...)
  # callsite. maybeMissing: a fresh clone before the first
  # `cargo xtask regen sqlx` won't have .sqlx/ yet.
  sqlxCacheFileset = pkgs.lib.fileset.toSource {
    root = ../.;
    fileset = pkgs.lib.fileset.maybeMissing ../.sqlx;
  };

  # query! macros read .sqlx/query-*.json instead of connecting to PG at
  # compile time. sqlx-macros-core 0.9.x finds the cache by (in order,
  # short-circuiting — query/mod.rs:97-117 @ 0.9.0):
  #   1. SQLX_OFFLINE_DIR — real env var (or a `.env` at
  #      $CARGO_MANIFEST_DIR when the env var is unset)
  #   2. $CARGO_MANIFEST_DIR/.sqlx
  #   3. workspace_root().join(".sqlx") — spawns `$CARGO metadata`
  # buildRustCrate calls rustc directly (no cargo, no CARGO env var, no
  # workspace Cargo.lock), so (3) would need a fake `cargo` shim. (1) is
  # checked first and short-circuits — set it as a plain derivation env
  # var. This is also THE single-channel contract for rio-buildhash's
  # build scripts (rio-{store,scheduler,controller}/build.rs): both the
  # macros and the RIO_SQLX_HASH tracker read exactly this variable, so
  # they can never disagree about which cache is in play. (The previous
  # postUnpack-written `.env` carried the same value; the env var
  # replaced it so the in-repo tracker needs no dotenv parser.)
  # Applied to every crate with `query!()`/`query_as!()` callsites.
  sqlxOffline = {
    SQLX_OFFLINE = "true";
    SQLX_OFFLINE_DIR = "${sqlxCacheFileset}/.sqlx";
  };

  # Crates whose build.rs invokes `protoc` (directly or via prost-build/
  # tonic-prost-build). nixpkgs' prost-build override sets PROTOC on
  # prost-build itself, but the env var must be on the CONSUMER that
  # runs `tonic_prost_build::configure()`.
  protoCrate = {
    nativeBuildInputs = [ pkgs.protobuf ];
    PROTOC = "${pkgs.protobuf}/bin/protoc";
  };

  # ──────────────────────────────────────────────────────────────────
  # sys-crate policy: system-link over vendored C
  # ──────────────────────────────────────────────────────────────────
  #
  # Sys-crate audit (see assessment doc for full table):
  #
  #   crate           | default      | system-link lever
  #   ----------------+--------------+------------------------------------
  #   aws-lc-sys      | vendored     | (none — aws-lc has no system pkg;
  #                   | (cmake)      |  nixpkgs override supplies cmake)
  #   libsqlite3-sys  | bundled      | LIBSQLITE3_SYS_USE_PKG_CONFIG=1 + pkgs.sqlite
  #   zstd-sys        | vendored     | ZSTD_SYS_USE_PKG_CONFIG=1 + pkgs.zstd
  #   ring            | vendored     | (none — ring is its own library)
  #   fuser           | system       | already uses pkg-config → fuse3
  #
  # libsqlite3-sys: sqlx's `sqlite` feature chain (sqlite → sqlx-sqlite/bundled
  # → libsqlite3-sys/bundled) hard-enables the `bundled` feature, which
  # compiles ~300 KLOC of bundled SQLite C source on every cold build.
  # libsqlite3-sys's build.rs has an env-var escape hatch (build.rs:49-53):
  # when LIBSQLITE3_SYS_USE_PKG_CONFIG is set, it routes through
  # build_linked instead of build_bundled regardless of the feature flag.
  # The resolved `bundled_bindings` feature stays — that just copies
  # precompiled Rust bindings from the crate source (no bindgen needed);
  # SQLite's ABI is stable across 3.x so the bundled bindings work against
  # system libsqlite 3.x. nixpkgs' defaultCrateOverrides already supplies
  # pkg-config + sqlite; we extend with the env var.
  #
  # (Previous note here claimed sqlite was vestigial — wrong. rio-builder
  # uses it for the synthetic Nix store DB and the FUSE LRU cache index.
  # sqlx's `sqlite-unbundled` feature exists but pulls in buildtime_bindgen
  # which needs libclang — heavier than the env-var escape hatch.)
  #
  # zstd-sys: build.rs checks $ZSTD_SYS_USE_PKG_CONFIG; when set it
  # calls pkg_config::probe("libzstd") and skips the `cc` vendored
  # build. The resolved feature set (`legacy,std,zdict_builder`) is
  # compatible with system libzstd 1.5+.
  #
  # aws-lc-sys and ring are cryptographic primitives with no drop-in
  # system-library equivalent (aws-lc-rs is Amazon's BoringSSL fork;
  # ring is Brian Smith's hand-tuned assembly). Vendoring is the only
  # correct option there. nixpkgs' defaultCrateOverrides already
  # supplies cmake for aws-lc-sys.
  defaultCrateOverrides = pkgs.defaultCrateOverrides // {
    # pkg-config + system lib + env-var escape hatch. All three drawn
    # from sysCrateEnv.crates.<name> — same libs the devShell links,
    # same env vars it sets. Changing sysCrateEnv (e.g. sqlite →
    # sqlite_3_45) propagates here automatically.
    fuser = _: {
      nativeBuildInputs = [ pkgs.pkg-config ];
      buildInputs = sysCrateEnv.crates.fuser.libs;
    };
    zstd-sys =
      _:
      sysCrateEnv.crates.zstd-sys.env
      // {
        nativeBuildInputs = [ pkgs.pkg-config ];
        buildInputs = sysCrateEnv.crates.zstd-sys.libs;
      };
    libsqlite3-sys =
      _:
      sysCrateEnv.crates.libsqlite3-sys.env
      // {
        nativeBuildInputs = [ pkgs.pkg-config ];
        buildInputs = sysCrateEnv.crates.libsqlite3-sys.libs;
      };

    rio-proto = _: protoCrate;
    tonic-health = _: protoCrate;
    opentelemetry-proto = _: protoCrate;
    # rio-test-support's build.rs (MockAdmin codegen) decodes
    # rio_proto::FILE_DESCRIPTOR_SET via a [build-dependencies] on
    # rio-proto — no protoc, no cross-directory proto reads, no override.

    # sqlx::query!()/query_as!() callsites — need the offline cache.
    # (sqlx::migrate!() lives only in rio-migrations, which is a leaf
    # crate and needs no override.)
    rio-scheduler = _: sqlxOffline;
    rio-store = _: sqlxOffline;
    # nodeclaim_pool/sketch.rs uses sqlx::query! (offline cache).
    rio-controller = _: sqlxOffline;

    # build.rs compiles libFuzzer's C++ via the `cc` crate. stdenv's
    # g++ (NOT clang — see below) plus -fsanitize=address so the C++
    # internals (FuzzWithFork's merge step in particular) are
    # asan-instrumented and don't trip __interceptor_memset on
    # std::vector ops with negative-size-param. cargo-fuzz only
    # instruments the Rust side, but it cross-compiles with --target
    # which makes rustc link the asan-aware C++ runtime; buildRustCrate
    # has no host/target split, so we instrument the C++ directly.
    # clangStdenv was tried first — its libc++/libstdc++ mix vs the
    # binary's gcc-lib RUNPATH caused the same false positive.
    libfuzzer-sys = _: {
      CXXFLAGS = "-fsanitize=address";
    };
  };

  cargoNix = import "${crate2nixSrc}/lib/build-from-json.nix" {
    inherit pkgs lib;
    inherit (pkgs) stdenv;
    src = workspaceSrc;
    inherit resolvedJson buildRustCrateForPkgs;
    # Intentionally NOT passing our custom defaultCrateOverrides —
    # they're already baked into buildRustCrateForPkgs above. Passing
    # pkgs.defaultCrateOverrides here makes build-from-json.nix's
    # `defaultCrateOverrides != pkgs.defaultCrateOverrides` check
    # evaluate to false, skipping its `.override` call. This is needed
    # for the coverage wrap (globalExtraRustcOpts != []) which returns
    # a plain function without a `.override` method.
    inherit (pkgs) defaultCrateOverrides;
  };

  workspace = cargoNix.allWorkspaceMembers;

  # Binary-only, closure-shrunk. `remapOpts` above already scrubs all
  # toolchain references at compile time (verified: 2.16GB → 56MB,
  # zero rust-default closure refs). RUNPATH is glibc/lib:gcc-lib/lib
  # from stdenv's fixupPhase — no post-processing needed. Stripping is
  # gated on `stripBins`: coverage builds set false so __llvm_covfun/
  # __llvm_covmap sections survive for llvm-cov (binaries ~5× larger,
  # closure unchanged). disallowedReferences guards both modes.
  binSuffix = if stripBins then "" else "-cov";
  scrubBins =
    name: drvBin:
    pkgs.runCommand name { disallowedReferences = [ rust ]; } ''
      mkdir -p $out/bin
      cp -L ${drvBin}/* $out/bin/
      ${lib.optionalString stripBins ''
        chmod -R u+w $out/bin
        ${pkgs.binutils}/bin/strip $out/bin/*
      ''}
    '';
in
{
  inherit cargoNix;

  # Raw symlinkJoin of every built crate's output. Still references
  # the intermediate .rlib tree (per-crate build outputs aren't
  # closure-scrubbed). Use `workspaceBins` for docker/VM tests.
  inherit workspace;

  # Binary-only variant — bin/crdgen bin/rio-cli bin/rio-{controller,
  # gateway,scheduler,store,worker}, closure ~glibc+syslibs. Stripped
  # iff `stripBins`. What docker.nix / nix/tests/ / nix/modules/
  # consume.
  workspaceBins = scrubBins "rio-bins${binSuffix}" "${workspace}/bin";

  # Per-member outputs for fine-grained targets:
  #   nix build .#rio-scheduler
  #   nix build .#rio-common
  # Each is a single buildRustCrate derivation — the whole point of
  # per-crate caching.
  members = lib.mapAttrs (_: m: m.build) cargoNix.workspaceMembers;

  # Per-member scrubbed bins (docker.nix consumer). Same shape as
  # `members` but each is bin/ only — closure ~glibc+syslibs. lib-only
  # members (rio-common, rio-nix, …) have no bin/ and fail at build if
  # referenced — correct, only bin crates belong in image contents.
  memberBins = lib.mapAttrs (
    name: m: scrubBins "${name}-bin${binSuffix}" "${m.build}/bin"
  ) cargoNix.workspaceMembers;
}
