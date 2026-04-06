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
  # rust-overlay stable toolchain (edition 2024). nixpkgs' packaged
  # rustc lags; build-from-json.nix plumbs this through to every
  # buildRustCrate invocation.
  rustStable,
  # The crate2nix flake source tree. We only need
  # lib/build-from-json.nix from it (packages.default is the CLI).
  crate2nixSrc,
  # Workspace root — must match what `crate2nix generate` ran against.
  # Fileset-filtered to avoid rebuilds on .claude/ or doc churn.
  workspaceSrc,
  # Path to the pre-resolved JSON (checked in at repo root).
  resolvedJson ? ../Cargo.json,
  # sys-crate env-var escape hatches + system libs. Passed from
  # flake.nix's sysCrateEnv — single source of truth so devShell
  # and crate2nix see the same linkage.
  sysCrateEnv,
  # Extra rustc flags injected into EVERY crate in the tree. Used by
  # the coverage variant (crateBuildCov in flake.nix) to build a
  # parallel instrumented tree with `-Cinstrument-coverage`.
  # Empty = no wrap.
  globalExtraRustcOpts ? [ ],
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
  # When `globalExtraRustcOpts` is non-empty (coverage tree), also
  # set LLVM_PROFILE_FILE=/dev/null (build scripts and proc-macros are
  # ALSO instrumented and would otherwise try to write profraws to the
  # RO sandbox CWD).
  #
  # The wrap returns a plain `crate_: drv` function — build-from-json.nix's
  # `.override { defaultCrateOverrides }` branch must be skipped for
  # this to work; we arrange that by NOT passing our custom overrides
  # to build-from-json.nix (they're already baked into `base` here).
  remapOpts = [ "--remap-path-prefix=${rustStable}=/rustc" ];

  buildRustCrateForPkgs =
    cratePkgs:
    let
      base = cratePkgs.buildRustCrate.override {
        rustc = rustStable;
        cargo = rustStable;
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
      # (docker images currently bundle `workspaceBins` = all members so
      # they don't shrink yet — the win surfaces there once images go
      # per-binary, and immediately on the AMI per-arch build path.)
      #
      # NOTE: this must intercept `crate_` here, not via
      # `defaultCrateOverrides` below — buildRustCrate threads
      # `dependencies`/`buildDependencies` through makeOverridable
      # defaults from the original `crate_` (build-rust-crate
      # default.nix:506-507), so the crateOverrides merge at
      # default.nix:238 never reaches them.
      crate_' =
        if crate_.crateName == "workspace-hack" then
          crate_
          // {
            dependencies = [ ];
            buildDependencies = [ ];
          }
        else
          crate_;
    in
    (base (
      crate_'
      // {
        extraRustcOpts = remapOpts ++ globalExtraRustcOpts ++ (crate_'.extraRustcOpts or [ ]);
      }
      // lib.optionalAttrs (globalExtraRustcOpts != [ ]) {
        # Discard build-time profraws. Test runners override at
        # runtime to collect real data.
        LLVM_PROFILE_FILE = "/dev/null";
      }
    )).overrideAttrs
      (_: {
        # nixbuild.net scheduling hints — route every crate build to
        # machines with ≥8 CPU / ≥16GB. crate2nix builds one derivation
        # per crate, so per-crate hints matter across the whole graph.
        # The heaviest compiles are often deps (aws-lc-sys cmake build,
        # ring's hand-tuned asm, librocksdb-sys), not just the rio-*
        # workspace. overrideAttrs guarantees the env vars land on the
        # mkDerivation call regardless of how buildRustCrate filters
        # its input args.
        NIXBUILDNET_MIN_CPU = "8";
        NIXBUILDNET_MIN_MEM = "16000";
      });

  # ──────────────────────────────────────────────────────────────────
  # Crate overrides
  # ──────────────────────────────────────────────────────────────────
  #
  # nixpkgs ships pkgs.defaultCrateOverrides which already covers
  # aws-lc-sys, libsqlite3-sys, prost-build, openssl-sys. We extend for
  # crates not in that set and for cross-directory compile-time
  # references that crate2nix's per-crate-src model can't see.
  #
  # The big one: `sqlx::migrate!("../migrations")` in rio-scheduler and
  # rio-store — the macro reads SQL files at COMPILE time from a path
  # relative to the crate dir. buildRustCrate's src is just
  # `rio-scheduler/`, so `../migrations` resolves to the nix-store
  # parent, which doesn't exist. Workaround: postUnpack symlinks the
  # migrations dir next to the crate src. Same trick Naersk users apply;
  # crate2nix issue #17 tracks the upstream limitation.
  # ──────────────────────────────────────────────────────────────────
  # Source granularity
  # ──────────────────────────────────────────────────────────────────
  #
  # build-from-json.nix resolves local crate srcs as
  # `workspaceSrc + "/<crate>"` — a subpath of ONE store hash. Touching
  # rio-common/src/lib.rs rehashes workspaceSrc, invalidating all 10
  # workspace members even when only one's content changed.
  #
  # The naive fix (per-crate fileset.toSource via crateOverride.src)
  # runs into buildRustCrate's unpackPhase expecting sourceRoot to be
  # the crate name. Rather than fight that here, we accept the rebuild
  # floor for the PoC and document the fix in the assessment: patch
  # build-from-json.nix to accept a `workspaceMemberSrcs` attrset that
  # maps crate name → fileset, so local paths resolve to independent
  # store paths. ~20-line upstream patch; tracked in the assessment.
  #
  # The `migrations/` symlink below DOES use a dedicated fileset so
  # rio-scheduler/rio-store don't rebuild when a sibling crate's src
  # changes — they'd rebuild anyway (most crates depend on rio-common)
  # but at least the migrations hash is stable.
  migrationsFileset = pkgs.lib.fileset.toSource {
    root = ../migrations;
    fileset = ../migrations;
  };

  # sqlx offline query cache — content-addressed JSON per query!(...)
  # callsite. sqlx-macros-core 0.8.x ALWAYS runs `$CARGO metadata` to
  # find workspace_root (workspace.rs:Metadata::resolve) — even with
  # SQLX_OFFLINE + SQLX_OFFLINE_DIR set, there's no bypass.
  # buildRustCrate calls rustc directly (no cargo, no workspace
  # Cargo.lock) so: without CARGO → "`CARGO` must be set"; with real
  # cargo → "EOF while parsing a value" (cargo metadata fails, no valid
  # workspace). Both → macro expands to dummy type → E0282.
  #
  # Fix: point CARGO at a stub that outputs the minimal metadata JSON
  # sqlx needs (workspace_root + target_directory + empty packages).
  # workspace_root points at the store path containing .sqlx/, so sqlx
  # finds the offline cache there. maybeMissing: a fresh clone before
  # the first `cargo xtask regen sqlx` won't have .sqlx/ yet.
  sqlxCacheFileset = pkgs.lib.fileset.toSource {
    root = ../.;
    fileset = pkgs.lib.fileset.maybeMissing ../.sqlx;
  };
  cargoMetadataStub = pkgs.writeShellScript "cargo-metadata-stub" ''
    # sqlx-macros-core runs `$CARGO metadata --format-version=1`.
    # It only reads .workspace_root (to locate .sqlx/) + .target_directory
    # (unused in offline mode). Minimal valid cargo_metadata::Metadata JSON:
    if [ "$1" = "metadata" ]; then
      echo '{"packages":[],"workspace_members":[],"workspace_default_members":[],"resolve":null,"target_directory":"/tmp","version":1,"workspace_root":"${sqlxCacheFileset}","metadata":null}'
      exit 0
    fi
    echo "cargo-metadata-stub: unexpected invocation: $*" >&2
    exit 1
  '';

  # Cross-crate compile-time reads from rio-test-support: build.rs
  # include!("../rio-test-support/src/metrics_grep.rs") in 5 crates, and
  # include_str!("../../../rio-test-support/golden/...") in rio-scheduler
  # src/state/derivation.rs. buildRustCrate's src is just the crate dir,
  # so ../rio-test-support resolves outside. Narrow filesets per-file so
  # editing golden/ doesn't invalidate metrics_grep-only crates.
  metricsGrepFileset = pkgs.lib.fileset.toSource {
    root = ../rio-test-support;
    fileset = ../rio-test-support/src/metrics_grep.rs;
  };
  goldenFileset = pkgs.lib.fileset.toSource {
    root = ../rio-test-support;
    fileset = ../rio-test-support/golden;
  };
  # build.rs emit_spec_metrics_grep("{manifest}/../docs/src/observability.md")
  # — greps the per-component metrics tables to derive SPEC_METRICS for
  # the spec→describe check. Same cross-directory problem; without this
  # symlink, emit_spec_metrics_grep's ENOENT fallback writes an empty
  # spec_metrics.txt and the test-side floor check ("has only 0 entries
  # — build.rs grep broken?") fails. Narrow fileset keeps the hash
  # stable when unrelated docs change.
  obsMdFileset = pkgs.lib.fileset.toSource {
    root = ../docs;
    fileset = ../docs/src/observability.md;
  };

  # rio-test-support/build.rs runs protoc on ../rio-proto/proto/admin.proto
  # to emit a FileDescriptorSet for MockAdmin codegen. Same cross-directory
  # problem as migrations: buildRustCrate src is just rio-test-support/.
  # Include all .proto files — admin.proto imports types/dag/admin_types,
  # which transitively import build_types. protoc needs the full graph.
  protoFileset = pkgs.lib.fileset.toSource {
    root = ../rio-proto;
    fileset = pkgs.lib.fileset.fileFilter (f: f.hasExt "proto") ../rio-proto/proto;
  };
  # Reconstruct the sibling-dir structure that cross-crate compile-time
  # reads expect. Called from all three override shapes (withMigrations,
  # withMetricsGrep, withHelmFiles) — the golden/ symlink is only USED
  # by rio-scheduler but costs nothing in crates that don't reference it.
  linkMetricsGrep = ''
    mkdir -p $NIX_BUILD_TOP/rio-test-support/src
    ln -sf ${metricsGrepFileset}/src/metrics_grep.rs $NIX_BUILD_TOP/rio-test-support/src/metrics_grep.rs
    ln -sf ${goldenFileset}/golden $NIX_BUILD_TOP/rio-test-support/golden
    mkdir -p $NIX_BUILD_TOP/docs/src
    ln -sf ${obsMdFileset}/src/observability.md $NIX_BUILD_TOP/docs/src/observability.md
  '';

  withMigrations = _: {
    # postUnpack runs after buildRustCrate has unpacked the crate src.
    # CWD is the unpacked crate directory; its parent is
    # $NIX_BUILD_TOP — writable. sqlx::migrate!("../migrations")
    # resolves $CARGO_MANIFEST_DIR/../migrations at compile time; this
    # symlink makes that path resolve to the fileset'd store path.
    # Same for rio-test-support/src/metrics_grep.rs (build.rs include!()).
    postUnpack = ''
      ln -sf ${migrationsFileset} $NIX_BUILD_TOP/migrations
      ${linkMetricsGrep}
    '';
    # query! macros read .sqlx/*.json instead of connecting to PG at
    # compile time. SQLX_OFFLINE_DIR bypasses the workspace-root walk;
    # CARGO points at a stub that outputs minimal `cargo metadata` JSON
    # (sqlx-macros-core 0.8.x always invokes it, no bypass — see
    # cargoMetadataStub above for the full failure chain).
    SQLX_OFFLINE = "true";
    SQLX_OFFLINE_DIR = "${sqlxCacheFileset}/.sqlx";
    CARGO = "${cargoMetadataStub}";
  };

  # rio-builder: only needs the metrics_grep include (no migrations).
  withMetricsGrep = _: {
    postUnpack = linkMetricsGrep;
  };

  # rio-controller's builderpool tests include_str! the seccomp profile
  # from ../../../../infra/helm/rio-build/files/ (4 levels up from
  # src/reconcilers/builderpool/ = repo root). Same cross-directory
  # compile-time-read problem as migrations: buildRustCrate's src is
  # just rio-controller/, so the relative path resolves outside the
  # unpacked sourceRoot. Symlink the files/ dir at $NIX_BUILD_TOP/infra/
  # so the include_str! path resolves. Narrow fileset keeps the hash
  # stable when unrelated helm templates change.
  helmFilesFileset = pkgs.lib.fileset.toSource {
    root = ../infra/helm/rio-build/files;
    fileset = ../infra/helm/rio-build/files;
  };

  withHelmFiles = _: {
    postUnpack = ''
      mkdir -p $NIX_BUILD_TOP/infra/helm/rio-build
      ln -sf ${helmFilesFileset} $NIX_BUILD_TOP/infra/helm/rio-build/files
      ${linkMetricsGrep}
    '';
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
    # from sysCrateEnv.crates.<name> — same libs crane links, same env
    # vars devShell sets. Changing sysCrateEnv (e.g. sqlite →
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

    # ring's build.rs drives `cc` with its own assembly. Needs a
    # working C toolchain which stdenv already provides; on some
    # platforms it also wants `perl` for the asm preprocessor. Linux
    # glibc stdenv has perl in nativeBuildInputs transitively. No-op
    # override kept as a doc marker — ring has no system-lib
    # equivalent, vendoring is by design.
    ring = _: { };

    # rio-proto: PROTOC + proto files are inside rio-proto/proto/, so
    # no cross-directory issue. But prost-build's build.rs needs
    # `protoc` in PATH or $PROTOC set. nixpkgs' prost-build override
    # sets PROTOC on the *prost-build* crate — but that runs at
    # prost-build's build time, not at rio-proto's. The env var must be
    # on the CONSUMER that runs `tonic_prost_build::configure()`.
    rio-proto = _: {
      nativeBuildInputs = [ pkgs.protobuf ];
      PROTOC = "${pkgs.protobuf}/bin/protoc";
    };

    # rio-test-support: build.rs runs protoc --descriptor_set_out on
    # ../rio-proto/proto/admin.proto (MockAdmin codegen). Needs PROTOC
    # same as rio-proto, plus the proto files symlinked into place.
    rio-test-support = _: {
      nativeBuildInputs = [ pkgs.protobuf ];
      PROTOC = "${pkgs.protobuf}/bin/protoc";
      postUnpack = ''
        mkdir -p $NIX_BUILD_TOP/rio-proto
        ln -sf ${protoFileset}/proto $NIX_BUILD_TOP/rio-proto/proto
      '';
    };

    # sqlx::migrate!("../migrations") compile-time file read. See
    # `withMigrations` above. rio-gateway only uses it in tests/
    # (integration-test MIGRATOR static), so its non-test build
    # succeeds without this — but buildTests=true compiles tests/
    # and needs the symlink.
    rio-scheduler = withMigrations;
    rio-store = withMigrations;
    rio-gateway = withMigrations;

    # build.rs include!("../rio-test-support/src/metrics_grep.rs") —
    # compile-time file read crossing crate boundary.
    rio-builder = withMetricsGrep;

    # include_str!("../../../../infra/helm/rio-build/files/...") in
    # builderpool tests + build.rs metrics_grep include — both compile-
    # time file reads crossing crate boundary. See `withHelmFiles` above.
    rio-controller = withHelmFiles;

    # tonic-health ships a bundled .proto and its build.rs compiles it.
    # Same PROTOC need as rio-proto.
    tonic-health = _: {
      nativeBuildInputs = [ pkgs.protobuf ];
      PROTOC = "${pkgs.protobuf}/bin/protoc";
    };

    # opentelemetry-proto also compiles bundled .proto files.
    # nixpkgs has an override but it may not set PROTOC in the form
    # tonic-prost-build expects; belt+braces.
    opentelemetry-proto = _: {
      nativeBuildInputs = [ pkgs.protobuf ];
      PROTOC = "${pkgs.protobuf}/bin/protoc";
    };

    # tonic-prost-build is prost-build plus tonic plumbing; the nixpkgs
    # override on prost-build sets PROTOC but only for prost-build's
    # own build script. tonic-prost-build's build is trivial (no .rs
    # gen) so no override needed — this comment exists because it's the
    # obvious-but-wrong place to put the PROTOC var.
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
  # from stdenv's fixupPhase — no post-processing needed. We just
  # strip for docker image size and guard with disallowedReferences
  # to catch any future remap gap.
  workspaceBins = pkgs.runCommand "rio-bins" { disallowedReferences = [ rustStable ]; } ''
    mkdir -p $out/bin
    cp -L ${workspace}/bin/* $out/bin/
    chmod -R u+w $out/bin
    ${pkgs.binutils}/bin/strip $out/bin/*
  '';

  # Coverage variant — skip strip so __llvm_covfun/__llvm_covmap
  # sections survive for llvm-cov. Debug info stays intact with
  # /rustc paths (remap-path-prefix), which lcov filters cleanly.
  # Same disallowedReferences guard; closure is glibc + syslibs,
  # binaries are ~5× larger from the debug info.
  workspaceBinsCov = pkgs.runCommand "rio-bins-cov" { disallowedReferences = [ rustStable ]; } ''
    mkdir -p $out/bin
    cp -L ${workspace}/bin/* $out/bin/
  '';
in
{
  inherit cargoNix;

  # Raw symlinkJoin of every built crate's output. Still references
  # the intermediate .rlib tree (per-crate build outputs aren't
  # closure-scrubbed). Use `workspaceBins` for docker/VM tests.
  inherit workspace;

  # Stripped binary-only variant — bin/crdgen bin/rio-cli
  # bin/rio-{controller,gateway,scheduler,store,worker}, closure
  # ~glibc+syslibs. What docker.nix, nix/tests/, nix/modules/ consume.
  inherit workspaceBins;

  # Unstripped variant for coverage builds that need __llvm_covfun
  # intact. Same closure as workspaceBins; binaries ~5× larger.
  inherit workspaceBinsCov;

  # Per-member outputs for fine-grained targets:
  #   nix build .#rio-scheduler
  #   nix build .#rio-common
  # Each is a single buildRustCrate derivation — the whole point of
  # per-crate caching.
  members = lib.mapAttrs (_: m: m.build) cargoNix.workspaceMembers;
}
