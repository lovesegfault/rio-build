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
  # Extra rustc flags injected into EVERY crate in the tree. Used by
  # the coverage variant (c2nCov in flake.nix) to build a parallel
  # instrumented tree with `-Cinstrument-coverage`. Empty = no wrap.
  globalExtraRustcOpts ? [ ],
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
  #
  # When `globalExtraRustcOpts` is non-empty (coverage tree), wrap the
  # per-crate call to inject extraRustcOpts + LLVM_PROFILE_FILE=/dev/null
  # (build scripts and proc-macros are ALSO instrumented and would
  # otherwise try to write profraws to the RO sandbox CWD). The wrap
  # returns a plain `crate_: drv` function — build-from-json.nix's
  # `.override { defaultCrateOverrides }` branch must be skipped for
  # this to work; we arrange that by NOT passing our custom overrides
  # to build-from-json.nix (they're already baked into `base` here).
  buildRustCrateForPkgs =
    cratePkgs:
    let
      base = cratePkgs.buildRustCrate.override {
        rustc = rustStable;
        cargo = rustStable;
        inherit defaultCrateOverrides;
      };
    in
    if globalExtraRustcOpts == [ ] then
      base
    else
      crate_:
      base (
        crate_
        // {
          extraRustcOpts = globalExtraRustcOpts ++ (crate_.extraRustcOpts or [ ]);
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
  # callsite. The macro walks up from CARGO_MANIFEST_DIR looking for
  # `.sqlx/` when SQLX_OFFLINE=1. Same cross-directory problem as
  # migrations: buildRustCrate's src is just the crate dir, so `../`
  # resolves into $NIX_BUILD_TOP. maybeMissing: a fresh clone before
  # the first `just sqlx-prepare` won't have the dir yet.
  sqlxCacheFileset = pkgs.lib.fileset.toSource {
    root = ../.;
    fileset = pkgs.lib.fileset.maybeMissing ../.sqlx;
  };

  # build.rs include!("../rio-test-support/src/metrics_grep.rs") —
  # shared metrics-spec grep logic extracted to an include!()-only file
  # so 5 crates' build.rs don't duplicate 30 lines of regex. Same
  # cross-directory problem: buildRustCrate's src is just the crate dir.
  metricsGrepFileset = pkgs.lib.fileset.toSource {
    root = ../rio-test-support;
    fileset = ../rio-test-support/src/metrics_grep.rs;
  };
  linkMetricsGrep = ''
    mkdir -p $NIX_BUILD_TOP/rio-test-support/src
    ln -sf ${metricsGrepFileset}/src/metrics_grep.rs $NIX_BUILD_TOP/rio-test-support/src/metrics_grep.rs
  '';

  withMigrations = _: {
    # postUnpack runs after buildRustCrate has unpacked the crate src.
    # CWD is the unpacked crate directory; its parent is
    # $NIX_BUILD_TOP — writable. sqlx::migrate!("../migrations")
    # resolves $CARGO_MANIFEST_DIR/../migrations at compile time; this
    # symlink makes that path resolve to the fileset'd store path.
    # Same for .sqlx/ (query! macro walks up from CARGO_MANIFEST_DIR)
    # and rio-test-support/src/metrics_grep.rs (build.rs include!()).
    postUnpack = ''
      ln -sf ${migrationsFileset} $NIX_BUILD_TOP/migrations
      ln -sf ${sqlxCacheFileset}/.sqlx $NIX_BUILD_TOP/.sqlx
      ${linkMetricsGrep}
    '';
    # query! macros read .sqlx/*.json instead of connecting to PG at
    # compile time. Without this, the build fails with "set DATABASE_URL
    # ... or run cargo sqlx prepare".
    SQLX_OFFLINE = "true";
  };

  # rio-worker: only needs the metrics_grep include (no migrations).
  withMetricsGrep = _: {
    postUnpack = linkMetricsGrep;
  };

  # rio-controller's workerpool tests include_str! the seccomp profile
  # from ../../../../infra/helm/rio-build/files/ (4 levels up from
  # src/reconcilers/workerpool/ = repo root). Same cross-directory
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
    rio-worker = withMetricsGrep;

    # include_str!("../../../../infra/helm/rio-build/files/...") in
    # workerpool tests + build.rs metrics_grep include — both compile-
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

  # Binary-only, stripped. buildRustCrate's default `out` output
  # references the full rust toolchain via debug info (RUNPATH +
  # --remap-path-prefix partial — `strings` on the unstripped
  # binary shows /nix/store/...rust-default-1.93.1 paths).
  # `allWorkspaceMembers` therefore has a ~2.3 GB closure, vs
  # crane's ~300 MB rio-workspace.
  #
  # For VM tests and docker images we only want the executables.
  # This runCommand:
  #   1. copies bin/* from allWorkspaceMembers (only members with
  #      binaries are symlinked there; library-only crates have
  #      empty bin/)
  #   2. strips debug info — removes the toolchain-path strings
  #   3. patchelf --shrink-rpath — drops RUNPATH entries to
  #      rustlib/lib (rustc std .so's aren't runtime deps of the
  #      final binary; everything's statically linked)
  #
  # Result closure is glibc + gcc-lib + system libs (fuse3,
  # sqlite, zstd) — same order as crane's rio-workspace.
  #
  # strip + patchelf --shrink-rpath alone is insufficient:
  # buildRustCrate sets an RPATH entry for
  # <rust>/lib/rustlib/<target>/lib (libstd-<hash>.so). The
  # binaries link libstd statically (rust's default), so the
  # RPATH is dead — but --shrink-rpath won't drop it because
  # the binary also pulls in libgcc_s from the same path.
  # removeReferencesTo does a blunt string-replace of the store
  # path with a placeholder, which is safe here: no runtime
  # rust .so's are actually loaded. The remaining gcc-lib
  # reference resolves via the direct gcc-lib entry.
  workspaceBins =
    pkgs.runCommand "rio-c2n-bins"
      {
        nativeBuildInputs = [
          pkgs.patchelf
          pkgs.removeReferencesTo
        ];
        # disallowedReferences fails the build if any output
        # path still references the toolchain — cheap guardrail
        # against a future buildRustCrate change that bakes in a
        # new toolchain reference we don't scrub.
        disallowedReferences = [ rustStable ];
      }
      ''
        mkdir -p $out/bin
        for f in ${workspace}/bin/*; do
          # Symlinks → real copies so strip/patchelf can mutate.
          cp -L "$f" $out/bin/
        done
        chmod -R u+w $out/bin
        for f in $out/bin/*; do
          ${pkgs.binutils}/bin/strip "$f"
          patchelf --shrink-rpath "$f"
          # String-replace any remaining rust-toolchain path.
          # Safe: rust stdlib is statically linked; the only
          # reference is a dead RPATH entry (libstd-*.so
          # doesn't exist in the installed set for static
          # builds, but the entry is baked in unconditionally).
          remove-references-to -t ${rustStable} "$f"
        done
      '';

  # Coverage variant — shrinks the closure WITHOUT stripping.
  # `strip` removes __llvm_covfun/__llvm_covmap sections that
  # llvm-cov needs for report generation. Instead, only:
  #   - patchelf --shrink-rpath (safe, doesn't touch sections)
  #   - remove-references-to (string-replace store paths, also
  #     doesn't touch ELF sections — the debug-info paths it
  #     clobbers are only used for std source resolution which
  #     lcov filters out anyway)
  # Binaries stay fat (debug info intact) but the Nix closure
  # collapses to glibc+syslibs, same as workspaceBins. The
  # docker image needs only the closure, so this fits the
  # k3s containerd tmpfs (previously 2.3GB image → ENOSPC).
  workspaceBinsCov =
    pkgs.runCommand "rio-c2n-bins-cov"
      {
        nativeBuildInputs = [
          pkgs.patchelf
          pkgs.removeReferencesTo
        ];
        disallowedReferences = [ rustStable ];
      }
      ''
        mkdir -p $out/bin
        for f in ${workspace}/bin/*; do
          cp -L "$f" $out/bin/
        done
        chmod -R u+w $out/bin
        for f in $out/bin/*; do
          patchelf --shrink-rpath "$f"
          remove-references-to -t ${rustStable} "$f"
        done
      '';
in
{
  inherit cargoNix;

  # Mirror the shape crane consumers expect: a flat workspace build
  # plus per-member derivations. `allWorkspaceMembers` is a symlinkJoin
  # of every built crate's output — roughly equivalent to crane's
  # rio-workspace but without tests/clippy bundled.
  #
  # NOTE: closure is ~2.3 GB (references full rust toolchain via
  # debug info). Use `workspaceBins` for docker/VM tests.
  inherit workspace;

  # Stripped binary-only variant. Same bin/ layout as crane's
  # rio-workspace (bin/crdgen bin/rio-cli bin/rio-{controller,
  # gateway,scheduler,store,worker}), closure ~glibc+syslibs.
  # This is what docker.nix, nix/tests/, nix/modules/ consume in
  # the c2n pipeline.
  inherit workspaceBins;

  # Unstripped but closure-scrubbed — for coverage builds that
  # need __llvm_covfun intact. Closure ~glibc+syslibs (same as
  # workspaceBins); binaries are ~5× larger (debug info kept).
  inherit workspaceBinsCov;

  # Per-member outputs for fine-grained targets:
  #   nix build .#c2n-rio-scheduler
  #   nix build .#c2n-rio-common
  # Each is a single buildRustCrate derivation — the whole point of
  # per-crate caching.
  members = lib.mapAttrs (_: m: m.build) cargoNix.workspaceMembers;
}
