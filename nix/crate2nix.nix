# crate2nix JSON-mode PoC — parallel to the crane pipeline.
#
# See .claude/notes/crate2nix-migration-assessment.md for the full
# comparison. This file exercises the experimental `--format json`
# output: feature/platform resolution happens in Rust (crate2nix
# generate), Nix is a thin consumer that wires pre-resolved crate
# records to pkgs.buildRustCrate. One derivation per crate → touching
# rio-scheduler/src/ doesn't rebuild the 400+ transitive deps that
# crane's buildDepsOnly lumps together with the workspace rebuild.
#
# The `Cargo.json` at repo root is produced by:
#   nix develop -c bash -c \
#     'crate2nix generate --format json -o Cargo.json'
# (crate2nix is in the dev shell once the flake input is added.)
#
# It must be regenerated whenever Cargo.lock changes (new deps, version
# bumps). Unlike crane, there's no IFD-based auto-regen here — the JSON
# mode explicitly trades that convenience for simpler/faster eval.
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
  # flake.nix's sysCrateEnv — single source of truth so crane,
  # devShell, and crate2nix all see the same linkage.
  sysCrateEnv,
}:
let
  # ──────────────────────────────────────────────────────────────────
  # Toolchain
  # ──────────────────────────────────────────────────────────────────
  #
  # buildRustCrate uses pkgs.rustc/pkgs.cargo by default. Override to
  # the same rust-overlay stable that crane uses, so edition 2024
  # compiles. `rust` is for the buildRustCrate runtime tooling (lib.rs
  # path discovery scripts etc.); `rustc`/`cargo` are the actual
  # compilers.
  buildRustCrateForPkgs =
    cratePkgs:
    cratePkgs.buildRustCrate.override {
      rustc = rustStable;
      cargo = rustStable;
      inherit defaultCrateOverrides;
    };

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

  withMigrations = _: {
    # postUnpack runs after buildRustCrate has unpacked the crate src.
    # CWD is the unpacked crate directory; its parent is
    # $NIX_BUILD_TOP — writable. sqlx::migrate!("../migrations")
    # resolves $CARGO_MANIFEST_DIR/../migrations at compile time; this
    # symlink makes that path resolve to the fileset'd store path.
    postUnpack = ''
      ln -sf ${migrationsFileset} $NIX_BUILD_TOP/migrations
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
  # (Previous note here claimed sqlite was vestigial — wrong. rio-worker
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
    # pkg-config + fuse3 headers. nixpkgs has no override for fuser.
    # The build script does `pkg_config::probe_library("fuse3")`.
    fuser = _: {
      nativeBuildInputs = [ pkgs.pkg-config ];
      buildInputs = [ pkgs.fuse3 ];
    };

    # System libzstd instead of the vendored copy. Saves ~15s of C
    # compilation per cold build and inherits nixpkgs security patches.
    # Env var value drawn from `sysCrateEnv` (flake.nix) — single
    # source of truth so crane, devShell, and crate2nix stay in sync.
    zstd-sys = _: {
      nativeBuildInputs = [ pkgs.pkg-config ];
      buildInputs = [ pkgs.zstd ];
      inherit (sysCrateEnv.env) ZSTD_SYS_USE_PKG_CONFIG;
    };

    # System libsqlite3 instead of the bundled amalgamation. Saves ~20s
    # of C compilation per cold build. nixpkgs' defaultCrateOverrides
    # already sets nativeBuildInputs=[pkg-config] buildInputs=[sqlite] —
    # but that's insufficient: sqlx enables the `bundled` feature which
    # makes build.rs compile the amalgamation unconditionally UNLESS
    # this env var is set (build.rs:49-53 escape hatch). With it set,
    # build.rs routes through build_linked → pkg-config probe → system
    # libsqlite3. `bundled_bindings` stays active, copying precompiled
    # Rust bindings from crate source (no bindgen); SQLite 3.x ABI
    # stability makes those bindings work against any 3.x system lib.
    # Env var value drawn from `sysCrateEnv` (flake.nix).
    libsqlite3-sys = _: {
      nativeBuildInputs = [ pkgs.pkg-config ];
      buildInputs = [ pkgs.sqlite ];
      inherit (sysCrateEnv.env) LIBSQLITE3_SYS_USE_PKG_CONFIG;
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

    # sqlx::migrate!("../migrations") compile-time file read. See
    # `withMigrations` above.
    rio-scheduler = withMigrations;
    rio-store = withMigrations;

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
    inherit resolvedJson buildRustCrateForPkgs defaultCrateOverrides;
  };
in
{
  inherit cargoNix;

  # Mirror the shape crane consumers expect: a flat workspace build
  # plus per-member derivations. `allWorkspaceMembers` is a symlinkJoin
  # of every built crate's output — roughly equivalent to crane's
  # rio-workspace but without tests/clippy bundled.
  workspace = cargoNix.allWorkspaceMembers;

  # Per-member outputs for fine-grained targets:
  #   nix build .#c2n-rio-scheduler
  #   nix build .#c2n-rio-common
  # Each is a single buildRustCrate derivation — the whole point of
  # per-crate caching.
  members = lib.mapAttrs (_: m: m.build) cargoNix.workspaceMembers;
}
