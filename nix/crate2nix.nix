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
  # Extra rustc flags injected into every crate in the tree, deps
  # included — EXCEPT the buildDepOnlyCrates closure, whose rlibs link
  # into uninstrumented build_script_build binaries and therefore skip
  # both opts lists (see effectiveTreeOpts). A non-empty value forks
  # the dep graph into a second derivation set, so nothing uses this
  # today — instrumented trees (coverage, fuzz) restrict their flags
  # to local crates to keep the dep derivations shared with the plain
  # tree. Kept as the escape hatch for a flag that genuinely must
  # reach (almost) every crate. Empty = no wrap.
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
  # opts list), also set LLVM_PROFILE_FILE=/dev/null: any proc-macro it
  # exports is ALSO instrumented and would otherwise try to write
  # profraws to the RO sandbox CWD when it executes during a dependent
  # build. (Build scripts are NOT instrumented — buildRustCrate compiles
  # build_script_build without extraRustcOpts; see buildDepOnlyCrates.) Gated on the specific flag — not on
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

  # Parsed resolved crate graph — shared by buildDepOnlyCrates (the
  # instrumentation exemption + the sanitizer dual-use guard) and
  # sqlxQueryCrates (the offline-cache override set), hoisted so the
  # consumers cannot parse divergent copies of the same json.
  resolved = builtins.fromJSON (builtins.readFile resolvedJson);
  allCrates = lib.attrValues resolved.crates;
  localCrates = lib.filter (c: (c.source.type or "") == "local") allCrates;
  localNames = map (c: c.crateName) localCrates;
  depNames = kind: lib.unique (lib.concatMap (c: map (d: d.name) (c.${kind} or [ ])) allCrates);
  buildNames = depNames "buildDependencies";
  runtimeNames = depNames "dependencies" ++ depNames "devDependencies";

  # Crates that exist in the workspace ONLY as build-dependencies (their
  # rlibs link into build_script_build binaries, which buildRustCrate
  # compiles WITHOUT extraRustcOpts). Excluded from instrumentation
  # flags in buildRustCrateForPkgs below.
  #
  # DERIVED from the resolved Cargo.json instead of hand-listed: a hand
  # list drifts both ways — the next build-dep-only crate silently
  # reintroduces the fuzz-tree asan build-script link failure, and a
  # crate gaining a runtime consumer would silently stay exempt from
  # instrumenting runtime-reachable code.
  #
  # Scanned over ALL local crates' incoming edges, not just workspace
  # members': the fuzz sub-workspaces have a single member (the fuzz
  # crate) and reach rio-* as local path deps, so a members-only scan
  # would derive an EMPTY set there and silently drop the exemption
  # exactly where it matters most. Membership = a local crate referenced
  # via [build-dependencies] by some crate in this tree and via NO
  # crate's [dependencies]/[dev-dependencies], EXTENDED to the
  # transitive closure over local [dependencies] edges of those roots
  # (an exempt crate's runtime deps link into the same uninstrumented
  # build_script_build binaries). A closure member that is ALSO
  # runtime/dev-consumed by a non-exempt local crate would have to be
  # simultaneously instrumented and uninstrumented — impossible with
  # one derivation per crate — so that conflict fails EVAL with a named
  # error instead of an undecipherable undefined-__asan_* link failure.
  # Exported from the return attrset so consumers (the coverage-matrix
  # exclusion in flake.nix) derive from this binding instead of
  # hand-mirroring the list.
  buildDepOnlyCrates =
    let
      crateByName = lib.listToAttrs (
        map (c: {
          name = c.crateName;
          value = c;
        }) localCrates
      );
      roots = lib.filter (n: lib.elem n buildNames && !lib.elem n runtimeNames) localNames;
      closure = builtins.genericClosure {
        startSet = map (n: { key = n; }) roots;
        operator =
          item:
          map (d: { key = d.name; }) (
            lib.filter (d: lib.elem d.name localNames) ((crateByName.${item.key} or { }).dependencies or [ ])
          );
      };
      exempt = map (i: i.key) closure;
      conflicted = lib.filter (
        n:
        lib.any (
          c:
          !lib.elem c.crateName exempt
          && lib.any (d: d.name == n) ((c.dependencies or [ ]) ++ (c.devDependencies or [ ]))
        ) localCrates
      ) exempt;
      # DUAL-USE: local crates consumed BOTH via [build-dependencies]
      # and via [dependencies]/[dev-dependencies]. By construction not
      # in `roots` (those exclude runtime-consumed crates), so the
      # `conflicted` throw above — which polices only the exempt
      # closure — misses any dual-use crate not reached through that
      # closure (one reachable via an exempt root's runtime deps IS
      # caught there first; both throws are conservative): such a
      # crate is built
      # INSTRUMENTED (for its runtime consumers) and its rlib also
      # links into uninstrumented build_script_build binaries. Fine
      # for -Cinstrument-coverage (rustc injects profiler_builtins
      # from the sysroot into every instrumented crate, so the rlib
      # links into uninstrumented binaries — verified against the
      # pinned stable toolchain AND by the green cov-smoke closure,
      # where instrumented rio-proto links into rio-test-support's
      # plain build script), but fatal for -Zsanitizer: the runtime
      # lives outside the sysroot dependency graph, so the link dies
      # with undefined __asan_*/__sanitizer_cov_* deep in a build log.
      # Guard at eval instead. Today: root/coverage trees have
      # dualUse=[rio-proto] (rio-test-support's build.rs decodes its
      # FILE_DESCRIPTOR_SET) but no -Zsanitizer flag; both fuzz trees
      # carry -Zsanitizer=address but dualUse=[] (rio-test-support is
      # in neither fuzz tree; rio-buildhash is build-dep-only). The
      # guard catches both future regressions: rio-test-support
      # entering a fuzz tree (rio-proto becomes dual-use under asan)
      # AND rio-buildhash gaining a runtime consumer (its roots entry
      # collapses out of the exempt set, leaving its build-dep edges
      # dual-use). Scope limit, documented not widened: a hypothetical
      # sancov-only tree (only -Cpasses/-Cllvm-args flags, no
      # -Zsanitizer=*) would slip past the prefix check and still fail
      # at link.
      dualUse = lib.filter (n: lib.elem n buildNames && lib.elem n runtimeNames) localNames;
      sanitizerActive = lib.any (lib.hasPrefix "-Zsanitizer") (
        globalExtraRustcOpts ++ localExtraRustcOpts
      );
    in
    if conflicted != [ ] then
      throw "crate2nix.nix: crate(s) [${toString conflicted}] are in the build-dep-only instrumentation-exempt closure but also runtime-consumed by non-exempt local crates. A crate cannot be both instrumented (for runtime consumers) and uninstrumented (for the build scripts that link it) — restructure the workspace, e.g. split the shared code out of the build-dependency crate."
    else if sanitizerActive && dualUse != [ ] then
      throw "crate2nix.nix: -Zsanitizer flags are active in this tree but local crate(s) [${toString dualUse}] are consumed both via [build-dependencies] and via [dependencies]/[dev-dependencies]. buildRustCrate compiles build_script_build uninstrumented, so the sanitized rlib the runtime consumers need would also link into build scripts and fail with undefined __asan_*/__sanitizer_cov_*. (-Cinstrument-coverage trees are exempt: rustc resolves profiler_builtins from the sysroot, so an instrumented rlib links into uninstrumented binaries — verified against the pinned stable toolchain.) Split the build.rs-consumed code into its own crate, or drop the crate from the sanitized tree."
    else
      exempt;

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
      # Tree-level instrumentation/extra opts for THIS crate, post
      # buildDepOnlyCrates exclusion — bound once so the extraRustcOpts
      # payload and the LLVM_PROFILE_FILE gate below cannot diverge.
      effectiveTreeOpts = lib.optionals (!lib.elem crate_.crateName buildDepOnlyCrates) (
        globalExtraRustcOpts ++ lib.optionals isLocal localExtraRustcOpts
      );
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
          ++ effectiveTreeOpts
          ++ lib.optionals isLocal [ localRemap ]
          ++ (crate_'.extraRustcOpts or [ ]);
      }
      //
        # Gate keyed on the SAME post-exclusion list as the payload above
        # — a gate on the pre-exclusion list forked the coverage tree's
        # rio-buildhash drv from the plain tree's byte-identical compile
        # via a no-op env var, forfeiting exactly the store sharing the
        # comment below describes.
        lib.optionalAttrs (lib.elem "-Cinstrument-coverage" effectiveTreeOpts) {
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
  # callsite. The cache is COMMITTED (a fresh clone has it);
  # maybeMissing keeps eval green in the only reachable no-.sqlx
  # states — `rm -rf .sqlx` recovery and mid-regen swaps — so the
  # failure surfaces at compile with sqlx's own diagnostic instead
  # of at eval.
  sqlxCacheFileset = pkgs.lib.fileset.toSource {
    root = ../.;
    fileset = pkgs.lib.fileset.maybeMissing ../.sqlx;
  };

  # query! macros read .sqlx/query-*.json instead of connecting to PG at
  # compile time. sqlx-macros-core 0.9.0 resolves the cache PER QUERY at
  # the FILE level: query/mod.rs:97-101 builds the candidate list
  #   1. SQLX_OFFLINE_DIR — real env var (or a `.env` at
  #      $CARGO_MANIFEST_DIR when the env var is unset)
  #   2. $CARGO_MANIFEST_DIR/.sqlx
  #   3. workspace_root().join(".sqlx") — spawns `$CARGO metadata`
  # and :107-108 joins query-<hash>.json onto each candidate, taking the
  # first FILE that exists — an earlier dir does NOT mask later ones for
  # files it lacks. buildRustCrate calls rustc directly (no cargo, no
  # CARGO env var, no workspace Cargo.lock), so (3) would need a fake
  # `cargo` shim; in the sandbox the fallthrough is structurally
  # impossible anyway — per-crate sources stage no .sqlx, so (2)/(3)
  # don't exist. Set (1) as a plain derivation env var. This is also THE
  # single-channel contract for rio-buildhash's build scripts
  # (rio-{store,scheduler,controller}/build.rs): both the macros and the
  # RIO_SQLX_HASH tracker read exactly this variable, and the tracker
  # unkeys any context where a divergent fallthrough cache DOES exist,
  # so they can never disagree about which cache is in play. (The
  # previous postUnpack-written `.env` carried the same value; the env
  # var replaced it so the in-repo tracker needs no dotenv parser.)
  # Applied to every crate with `query!()`/`query_as!()` callsites.
  sqlxOffline = {
    SQLX_OFFLINE = "true";
    SQLX_OFFLINE_DIR = "${sqlxCacheFileset}/.sqlx";
  };

  # Local crates wired to rio-buildhash::track_sqlx() in build.rs —
  # exactly the crates whose query!()/query_as!() macros read the
  # offline cache. DERIVED from the tracker wiring rather than
  # hand-listed or inferred from manifests: "has sqlx in [dependencies]
  # AND rio-buildhash in [build-dependencies]" is the WRONG membership
  # rule — rio-migrations matches it yet must NOT get the env (its
  # tracker is track_migrations; sqlx::migrate! reads migrations/,
  # never .sqlx/). The macro/tracker pair is one contract: a crate gets
  # SQLX_OFFLINE_DIR exactly when its build script hashes that dir into
  # the kache key, and the pre-commit sqlx-prepare-check enforces the
  # converse direction (real query! callsites without track_sqlx refuse
  # at commit). Fuzz-tree-only local names (rio-{nix,store}-fuzz) have
  # no <repo-root>/<name>/ dir, fail pathExists, and drop out by
  # construction; like memberSrcs (below), this relies on
  # crateName == repo-root dir name.
  sqlxQueryCrates = lib.filter (
    n:
    builtins.pathExists (../. + "/${n}/build.rs")
    && lib.hasInfix "track_sqlx" (builtins.readFile (../. + "/${n}/build.rs"))
  ) localNames;

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
  defaultCrateOverrides =
    pkgs.defaultCrateOverrides
    // {
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
    }
    # sqlx::query!()/query_as!() callsites — need the offline cache.
    # Membership derived per tree from the build.rs tracker wiring (see
    # sqlxQueryCrates above); rio-migrations' sqlx::migrate!() reads
    # migrations/, never .sqlx/, and deliberately gets NO override.
    // lib.genAttrs sqlxQueryCrates (_: (_: sqlxOffline));

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
    # for the instrumented wraps (localExtraRustcOpts != []) which return
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

  # Instrumentation-exempt crate names (build-dep-only closure) — the
  # single source for every consumer that must agree with the exemption
  # (coverage-matrix exclusion in flake.nix). See the buildDepOnlyCrates
  # comment above for membership semantics.
  instrumentationExemptCrates = buildDepOnlyCrates;
}
