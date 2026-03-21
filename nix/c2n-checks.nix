# crate2nix check backends: clippy, tests, doc, coverage.
#
# Layers on top of nix/crate2nix.nix's per-crate build graph.
# Dependencies are built ONCE with regular rustc (655 cached drvs);
# workspace members are re-built per-check with the appropriate
# driver (clippy-driver, rustc --test, rustdoc, rustc -Cinstrument-coverage).
#
# This is how you get per-crate caching for the CI gate — deps never
# re-clippy, never re-testcompile, never re-doc. Only the 10 workspace
# members get per-check rebuilds, and each of those caches independently.
#
# ┌──────────────────────────────────────────────────────────────────┐
# │                     dep crates (645, cached)                     │
# │                 regular rustc, --cap-lints allow                 │
# └────────────────┬─────────────────────────────────────────────────┘
#                  │ .rlib outputs (shared by all check variants)
#                  ▼
# ┌──────────────────────────────────────────────────────────────────┐
# │               workspace members (10, per-check)                  │
# ├────────────────┬──────────────┬──────────────┬──────────────────┤
# │     build      │    clippy    │    tests     │       doc        │
# │  rustc (base)  │ clippy-driver│ rustc --test │    rustdoc       │
# │                │  -Dwarnings  │  +dev-deps   │  --document-...  │
# └────────────────┴──────────────┴──────────────┴──────────────────┘
#
# Each workspace-member×check is a separate derivation. Touching
# rio-common/src/lib.rs invalidates rio-common's 4 check drvs AND
# downstream members' check drvs (they depend on rio-common's rlib).
# Deps stay cached.
{
  pkgs,
  lib,
  rustStable,
  # Output of nix/crate2nix.nix: { cargoNix, workspace, members }
  c2n,
  # Coverage-instrumented variant of the crate tree (nix/crate2nix.nix
  # re-imported with globalExtraRustcOpts=["-Cinstrument-coverage"]).
  # Used to produce test binaries that emit .profraw files at runtime.
  # null → coverage targets not exposed.
  c2nCov ? null,
  # Runtime inputs for test execution (PG, nix-cli, openssh). Mirrors
  # crane's cargoNextest nativeCheckInputs.
  runtimeTestInputs ? [ ],
  # Env vars set on all test-runner derivations. Mirrors crane's
  # RIO_GOLDEN_* / RIO_GOLDEN_FORCE_HERMETIC env injection.
  testEnv ? { },
}:
let
  inherit (c2n) cargoNix;

  # ──────────────────────────────────────────────────────────────────
  # Clippy wrapper
  # ──────────────────────────────────────────────────────────────────
  #
  # buildRustCrate's lib.sh hardcodes `--cap-lints allow` (line 18 +
  # 55). rustc treats --cap-lints as non-overridable (later
  # occurrences are ignored — verified empirically: `rustc ...
  # --cap-lints allow --cap-lints warn` ≡ `--cap-lints allow`).
  #
  # To run clippy on workspace members without patching lib.sh, we
  # wrap clippy-driver in a fake "rustc" that strips `--cap-lints
  # allow` before forwarding. clippy-driver IS rustc internally (it
  # calls rustc_driver with extra lint passes), so the rlib output is
  # compatible with deps built by regular rustc — same rmeta format,
  # same metadata hash machinery.
  #
  # Dependencies stay on regular rustc (cached, --cap-lints allow —
  # we don't want a transitive dep's clippy pedantry blocking the
  # build). Only workspace members get this wrapper via .override.
  #
  # The wrapper also needs rustc itself in PATH because clippy-driver
  # resolves the sysroot from `rustc --print sysroot` at startup.
  # symlinkJoin bin/ with BOTH rustc (real) and cargo from the stable
  # toolchain, then override the rustc name with the wrapper script.
  # runCommand is simpler than wrapProgram here because we need the
  # arg-filtering logic, not just env injection.
  clippyRustc = pkgs.runCommand "clippy-rustc-wrapper" { } ''
    mkdir -p $out/bin
    # Real binaries first: clippy-driver's sysroot resolution calls
    # `rustc --print sysroot`, and buildRustCrate's configure phase
    # calls `cargo metadata`. Symlinking preserves the $out/lib
    # relative lookup those binaries do.
    ln -s ${rustStable}/bin/* $out/bin/
    # Now override rustc with the wrapper. rm -f in case the star
    # glob already linked a `rustc` (it will — rustStable/bin has it).
    rm -f $out/bin/rustc
    cat > $out/bin/rustc <<'EOF'
    #!${pkgs.runtimeShell}
    # Strip --cap-lints and its argument. buildRustCrate's lib.sh
    # hardcodes `--cap-lints allow`; clippy can't fire through that.
    # We re-inject `--cap-lints warn` at the end so clippy lints fire
    # as warnings (which -Dwarnings in extraRustcOpts then promotes
    # to errors for workspace members only).
    #
    # buildRustCrate also passes a build-script compilation through
    # this same `rustc` — we DON'T want clippy on build.rs (it's
    # build-time throwaway, often auto-generated). Detect via
    # --crate-name build_script_build and fall through to real rustc.
    args=()
    skip_next=0
    is_build_script=0
    for arg in "$@"; do
      if [[ "$skip_next" == 1 ]]; then skip_next=0; continue; fi
      if [[ "$arg" == "--cap-lints" ]]; then skip_next=1; continue; fi
      if [[ "$arg" == "build_script_build" ]]; then is_build_script=1; fi
      args+=("$arg")
    done
    if [[ "$is_build_script" == 1 ]]; then
      # Build scripts: no clippy. Restore the cap-lints allow that
      # lib.sh intended — build.rs often has #[allow]-style noise
      # that's not worth linting.
      exec ${rustStable}/bin/rustc "''${args[@]}" --cap-lints allow
    fi
    # Library/binary: clippy, NO cap. Workspace members get
    # `-Dwarnings` via extraRustcOpts which promotes all warnings
    # (including clippy's) to errors. cap-lints=warn would
    # DOWNGRADE those back to warnings (rc=0 even with lints) —
    # verified empirically. We strip the cap entirely so
    # -Dwarnings takes effect. This wrapper only runs on workspace
    # members (deps use regular rustc, already cap-lints=allow from
    # lib.sh), so no cap = "treat our own code strictly."
    exec ${rustStable}/bin/clippy-driver "''${args[@]}"
    EOF
    chmod +x $out/bin/rustc
  '';

  # Lint flags for workspace members. Matches the crane clippy config:
  # `cargo clippy --all-targets -- --deny warnings`. The workspace
  # Cargo.toml [lints] table is NOT read by buildRustCrate (it's a
  # cargo construct); lints must be passed as rustc flags directly.
  #
  # -Dwarnings promotes all warn-level lints to errors. clippy::all is
  # the default lint group (style, complexity, correctness, perf,
  # suspicious). We don't enable pedantic/nursery — those are
  # advisory-only in the crane check too.
  clippyFlags = [
    "-Dwarnings"
    "-Wclippy::all"
    # Individual lints that the workspace [lints.rust] table would set
    # (buildRustCrate doesn't read Cargo.toml [lints]). Keep in sync
    # with the workspace Cargo.toml lint config if one exists.
  ];

  # ──────────────────────────────────────────────────────────────────
  # rustdoc wrapper
  # ──────────────────────────────────────────────────────────────────
  #
  # Similar arg-filtering rationale as clippy: lib.sh hardcodes some
  # rustc-isms that rustdoc rejects (e.g. --crate-type lib). The
  # wrapper strips incompatible flags and redirects to rustdoc.
  #
  # rustdoc needs the same --extern flags as rustc (to find the dep
  # rlibs), same --edition, same -L search paths. It IGNORES most -C
  # codegen flags (they're no-ops for docs). The outputs go to
  # --out-dir target/doc instead of target/lib.
  #
  # This is the "cheap doc" variant: per-crate docs without the
  # workspace index that `cargo doc` generates. A follow-up
  # symlinkJoin + search-index merge recreates the workspace view.
  rustdocRustc = pkgs.runCommand "rustdoc-rustc-wrapper" { } ''
    mkdir -p $out/bin
    ln -s ${rustStable}/bin/* $out/bin/
    rm -f $out/bin/rustc
    cat > $out/bin/rustc <<'EOF'
    #!${pkgs.runtimeShell}
    # Translate a rustc lib-build invocation into a rustdoc invocation.
    # buildRustCrate's lib.sh calls `rustc --crate-name foo src/lib.rs
    # --out-dir target/lib -L ... --extern ... --crate-type lib ...`.
    # rustdoc wants the same --extern/-L/--edition but rejects
    # --crate-type, -C codegen flags, --emit, and --remap-path-prefix.
    #
    # build_script_build gets real rustc — we need the build script to
    # ACTUALLY run (it produces OUT_DIR files that #[path] or
    # include! directives in the crate source reference, which
    # rustdoc needs to resolve).
    args=()
    skip_next=0
    is_build_script=0
    saw_out_dir=0
    crate_name=""
    for arg in "$@"; do
      if [[ "$skip_next" == 1 ]]; then skip_next=0; continue; fi
      case "$arg" in
        --cap-lints) skip_next=1 ;;
        --crate-type) skip_next=1 ;;
        --emit=*|--emit) [[ "$arg" == "--emit" ]] && skip_next=1 ;;
        -C) skip_next=1 ;;
        -C*) ;; # drop -Copt-level=3 form
        --remap-path-prefix=*) ;;
        --out-dir) skip_next=1; saw_out_dir=1 ;;
        build_script_build) is_build_script=1; args+=("$arg") ;;
        --crate-name) args+=("$arg"); crate_name="NEXT" ;;
        *)
          if [[ "$crate_name" == "NEXT" ]]; then crate_name="$arg"; fi
          args+=("$arg")
          ;;
      esac
    done
    if [[ "$is_build_script" == 1 ]]; then
      exec ${rustStable}/bin/rustc "$@"
    fi
    # bin targets: skip — we only doc the lib. buildRustCrate's
    # build_bin path passes --crate-type bin which we've stripped,
    # but the source file is src/main.rs or bin/*.rs. Detect by
    # checking for a main.rs in args and produce a stub output so
    # buildRustCrate's install phase doesn't fail on missing bin.
    for a in "''${args[@]}"; do
      case "$a" in *main.rs|*/bin/*) mkdir -p target/bin; touch "target/bin/$crate_name"; exit 0 ;; esac
    done
    mkdir -p target/doc
    # --document-private-items is STABLE — no -Z unstable-options
    # needed. Previous version had -Z unstable-options defensively;
    # that broke stable rustdoc. --cap-lints allow: we don't fail
    # docs on lint warnings (crane's cargoDoc uses -Dwarnings; we
    # could match that by passing -Dwarnings here, but then every
    # doc warning in every dep transitively blocks — deps stay
    # cap-lints'd since the wrapper only runs on workspace members).
    exec ${rustStable}/bin/rustdoc "''${args[@]}" \
      --out-dir target/doc \
      --cap-lints allow \
      --document-private-items
    EOF
    chmod +x $out/bin/rustc
  '';

  # ──────────────────────────────────────────────────────────────────
  # Per-check member derivations
  # ──────────────────────────────────────────────────────────────────
  #
  # Each check is a .override on the base member derivation. The deps
  # are UNCHANGED (same list of rlib derivations) — only the member's
  # rustc driver and extraRustcOpts differ. buildRustCrate's override
  # is lib.makeOverridable, so .override { rust; extraRustcOpts; }
  # produces a fresh derivation that REUSES the cached dep rlibs.

  # devDependencies from Cargo.json (injected by scripts/inject-dev-deps.py).
  # buildRustCrate doesn't know about dev-deps natively — the Cargo.nix
  # template mode handles them via a separate dependency-resolution
  # pass. In JSON mode, we inject them into the override's
  # `dependencies` list for the test build variant. They're already
  # built as regular crates in the graph (crate2nix resolves
  # --all-features, which pulls everything the lockfile references).
  devDepsFor =
    name:
    let
      packageId = cargoNix.resolved.workspaceMembers.${name};
      crateInfo = cargoNix.resolved.crates.${packageId};
      devDepRecords = crateInfo.devDependencies or [ ];
    in
    map (d: cargoNix.builtCrates.crates.${d.packageId}) devDepRecords;

  # Clippy check: rebuild the member with clippy-driver as "rustc".
  # The wrapper strips --cap-lints allow, re-injects --cap-lints warn,
  # and -Dwarnings in extraRustcOpts promotes findings to errors.
  # Deps are untouched (regular rustc, cached).
  clippyMember =
    _name: base:
    (base.override {
      rust = clippyRustc;
      extraRustcOpts = clippyFlags;
    }).overrideAttrs
      (old: {
        # Distinguish the clippy derivation from the base build in
        # `nix log` and `nix flake show`. The base is
        # "rust_rio-common-0.1.0"; this is "…-clippy".
        name = "${old.name}-clippy";
      });

  # Test binary build: same rustc, but with --test + dev-deps.
  # buildRustCrate has a `buildTests = true` knob that switches to
  # `rustc --test` (produces a test harness binary per lib.rs/bin).
  # The output's tests/ dir contains one executable per target —
  # those are what nextest or direct execution runs.
  #
  # dev-deps are appended to the dependencies list. buildRustCrate
  # doesn't distinguish normal from dev at the rustc level (both
  # become --extern flags); the distinction only matters for which
  # deps are LINKED. Cargo links dev-deps only for test/bench
  # targets; since we're setting buildTests=true, appending to
  # dependencies is correct.
  testMember =
    name: base:
    (base.override (old: {
      buildTests = true;
      # Append dev-deps. old.dependencies is the normal deps list;
      # dev-deps are resolved via devDepsFor from the injected
      # Cargo.json. If inject-dev-deps.py hasn't run (or the member
      # has no dev-deps), this is a no-op.
      dependencies = old.dependencies ++ devDepsFor name;
    })).overrideAttrs
      (old: {
        name = "${old.name}-test";
      });

  # Doc build: rustdoc instead of rustc. The wrapper translates
  # buildRustCrate's rustc invocation to rustdoc (strips codegen
  # flags, redirects --out-dir). Output is target/doc/crate_name/
  # which the install phase needs to pick up.
  docMember =
    _name: base:
    (base.override {
      rust = rustdocRustc;
    }).overrideAttrs
      (old: {
        name = "${old.name}-doc";
        # rustdoc writes to target/doc/, not target/lib/. Override
        # installPhase to copy docs instead of rlibs. buildRustCrate's
        # default install looks for target/lib/*.rlib which won't
        # exist — it'll fail with "no output".
        installPhase = ''
          runHook preInstall
          mkdir -p $out/share/doc
          if [ -d target/doc ]; then
            cp -r target/doc/* $out/share/doc/
          fi
          # The lib output must exist for downstream linkage metadata
          # (buildRustCrate's outputs = ["out" "lib"]). Stub it.
          mkdir -p $lib/lib
          touch $lib/lib/.doc-stub
          runHook postInstall
        '';
      });

  # ──────────────────────────────────────────────────────────────────
  # Coverage-instrumented test binaries
  # ──────────────────────────────────────────────────────────────────
  #
  # Coverage needs INSTRUMENTED DEPS too for accurate line attribution
  # (an inlined function from a dep shows up in the caller's profile;
  # without instrumented dep rlib, llvm-cov can't map it back). The
  # parallel tree is a second `cargoNix` instantiation (c2nCov) with
  # globalExtraRustcOpts=["-Cinstrument-coverage"] — see crate2nix.nix.
  #
  # devDepsFor here must read from c2nCov's builtCrates, not c2n's —
  # the instrumented rlibs have different metadata hashes (extraRustcOpts
  # contributes to the -C metadata= hash) so mixing trees fails link.
  devDepsForCov =
    name:
    let
      packageId = c2nCov.cargoNix.resolved.workspaceMembers.${name};
      crateInfo = c2nCov.cargoNix.resolved.crates.${packageId};
      devDepRecords = crateInfo.devDependencies or [ ];
    in
    map (d: c2nCov.cargoNix.builtCrates.crates.${d.packageId}) devDepRecords;

  covTestMember =
    name: base:
    (base.override (old: {
      buildTests = true;
      dependencies = old.dependencies ++ devDepsForCov name;
    })).overrideAttrs
      (old: {
        name = "${old.name}-cov-test";
      });

  # ──────────────────────────────────────────────────────────────────
  # Aggregates
  # ──────────────────────────────────────────────────────────────────
  #
  # Each check gets a symlinkJoin of all workspace members' check
  # outputs. For clippy, the join forces all 10 clippy derivations to
  # build (and thus lint); for tests, it collects all test binaries
  # for a subsequent runner derivation.

  members = cargoNix.workspaceMembers;

  clippyDrvs = lib.mapAttrs (name: m: clippyMember name m.build) members;
  testBinDrvs = lib.mapAttrs (name: m: testMember name m.build) members;
  docDrvs = lib.mapAttrs (name: m: docMember name m.build) members;
  covTestBinDrvs = lib.optionalAttrs (c2nCov != null) (
    lib.mapAttrs (name: m: covTestMember name m.build) c2nCov.cargoNix.workspaceMembers
  );

  clippyAll = pkgs.symlinkJoin {
    name = "c2n-clippy-all";
    paths = lib.attrValues clippyDrvs;
  };

  docAll = pkgs.symlinkJoin {
    name = "c2n-doc-all";
    paths = map (d: d.out) (lib.attrValues docDrvs);
  };

  # ──────────────────────────────────────────────────────────────────
  # Test runners (per-crate + aggregate)
  # ──────────────────────────────────────────────────────────────────
  #
  # Direct harness execution (what `cargo test` does under the hood).
  # nextest would require a nextest-archive format with its own
  # metadata requirements (Cargo.toml paths, target-dir layout) —
  # follow-up work. Direct execution proves the mechanism and caches
  # per-crate.
  #
  # Each runner gets the full runtimeTestInputs (PG, nix-cli, openssh)
  # regardless of whether that specific crate needs them — sandbox
  # inputs are cheap and keeping the matrix uniform avoids "forgot
  # to add PG for the new crate" regressions. testEnv injects
  # RIO_GOLDEN_* + PG_BIN.
  #
  # Parameterized on the test-binary derivation set so the coverage
  # variant (covTestBinDrvs) reuses the same runner logic.
  mkTestRunner =
    {
      name,
      testBin,
      extraEnv ? { },
      preRun ? "",
      postRun ? "",
    }:
    pkgs.runCommand "c2n-test-run-${name}"
      (
        testEnv
        // extraEnv
        // {
          nativeBuildInputs = runtimeTestInputs;
          RUST_BACKTRACE = "1";
          # nixbuild.net heuristic allocation can under-provision for
          # test runs (one run got 3.2GB — postgres + 16 tokio test
          # threads OOM'd). Match crane's commonArgs pin.
          NIXBUILDNET_MIN_CPU = "16";
          NIXBUILDNET_MIN_MEM = "16384";
        }
      )
      ''
        set -euo pipefail
        mkdir -p $out
        ${preRun}
        echo "── ${name} ──" | tee -a $out/log

        # ──────────────────────────────────────────────────────────
        # PG bootstrap (wrapper-level, not in-test)
        # ──────────────────────────────────────────────────────────
        #
        # rio-test-support::pg::PgServer::bootstrap spawns postgres
        # with PR_SET_PDEATHSIG(SIGTERM). On Linux, PDEATHSIG fires
        # when the SPAWNING THREAD terminates — not the process.
        # libtest spawns a fresh std::thread for each test fn (then
        # joins it before the next). The first test to reach
        # TestDb::new does the bootstrap on its libtest thread;
        # when that test completes and its thread exits, postgres
        # gets SIGTERM. Subsequent tests see the socket gone →
        # "failed to connect: NotFound".
        #
        # crane's cargoNextest avoids this structurally — nextest
        # runs each test in its OWN process, so each test
        # bootstraps its own PG and the spawning process outlives
        # the test (nextest's worker is the spawner, not the test
        # thread). Direct libtest harness execution has no such
        # isolation.
        #
        # Fix: bootstrap postgres HERE in the bash wrapper (stable
        # parent — lives for the entire derivation build) and hand
        # the socket URL to tests via DATABASE_URL (pg.rs:44-48
        # takes DATABASE_URL as external-PG override, skipping
        # bootstrap). Unix socket only (no TCP) — matches the
        # in-test bootstrap's defaults and works in the sandbox's
        # --private-network namespace.
        pgdata=$TMPDIR/pgdata
        sockdir=$TMPDIR/pgsock
        mkdir -p "$sockdir"
        "$PG_BIN/initdb" -D "$pgdata" --encoding=UTF8 --locale=C \
          -U postgres --auth=trust >/dev/null
        cat >> "$pgdata/postgresql.conf" <<EOF
        listen_addresses = '''
        unix_socket_directories = '$sockdir'
        fsync = off
        synchronous_commit = off
        full_page_writes = off
        max_connections = 500
        EOF
        "$PG_BIN/postgres" -D "$pgdata" 2>$TMPDIR/pg.log &
        pg_pid=$!
        trap 'kill $pg_pid 2>/dev/null || true' EXIT
        # Wait for socket (postgres writes .s.PGSQL.5432 when ready).
        for i in $(seq 100); do
          [ -S "$sockdir/.s.PGSQL.5432" ] && break
          sleep 0.1
        done
        if ! [ -S "$sockdir/.s.PGSQL.5432" ]; then
          echo "postgres failed to start:" >&2
          cat $TMPDIR/pg.log >&2
          exit 1
        fi
        export DATABASE_URL="postgres:///postgres?host=$sockdir&user=postgres&port=5432"
        echo "  postgres up at $sockdir" | tee -a $out/log

        failed=0
        shopt -s nullglob
        bins=(${testBin}/tests/*)
        if [ ''${#bins[@]} -eq 0 ]; then
          echo "  (no test binaries)" | tee -a $out/log
        fi
        for t in "''${bins[@]}"; do
          echo "  running $(basename $t)" | tee -a $out/log
          # --test-threads=8: TestDb::new names databases by
          # SystemTime::now().as_nanos(). With libtest's default
          # NCPU threads (64 on nixbuild.net), two tests can hit
          # the same nanosecond → CREATE DATABASE unique-constraint
          # violation. 8 threads makes collision vanishingly rare
          # and is still plenty for per-crate test parallelism.
          if ! "$t" --test-threads=8 2>&1 | tee -a $out/log; then
            echo "  FAIL: $(basename $t)" | tee -a $out/log
            failed=1
          fi
        done
        ${postRun}
        if [[ "$failed" == 1 ]]; then
          echo "── FAILURES ──" >&2
          cat $TMPDIR/pg.log >&2
          exit 1
        fi
      '';

  testRunDrvs = lib.mapAttrs (name: testBin: mkTestRunner { inherit name testBin; }) testBinDrvs;

  # Aggregate runner: symlinkJoin forces all per-crate runners to
  # build (and thus pass). Simpler than a single mega-runCommand and
  # caches better — a passing crate's runner doesn't re-execute when
  # a sibling's test changes.
  testRunAll = pkgs.symlinkJoin {
    name = "c2n-test-all";
    paths = lib.attrValues testRunDrvs;
  };

  # ──────────────────────────────────────────────────────────────────
  # Coverage: run instrumented tests → profraw → lcov
  # ──────────────────────────────────────────────────────────────────
  #
  # Toolchain-bundled llvm tools — profile format version is tied to
  # the rustc that compiled the instrumented binary. Using system
  # llvm-profdata gives "unsupported profile format version" errors.
  sysroot = "${rustStable}/lib/rustlib/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/bin";

  # Per-crate coverage runner: run instrumented test binaries with
  # LLVM_PROFILE_FILE set, collect profraws into $out/profraw/.
  # Separate from the merge step so profraw collection caches
  # independently of lcov generation (merge options can change
  # without re-running tests).
  covProfrawDrvs = lib.mapAttrs (
    name: testBin:
    mkTestRunner {
      inherit name testBin;
      preRun = ''
        # %m = module signature (one per binary), %p = PID.
        # LLVM_PROFILE_FILE must be an absolute path set BEFORE the
        # test binary exec's — the runtime reads it at startup.
        # $TMPDIR is the sandbox-writable scratch; /build is the
        # nixbuild.net build root but not always writable at
        # arbitrary subdirs.
        mkdir -p $TMPDIR/profraw
        export LLVM_PROFILE_FILE="$TMPDIR/profraw/%m-%p.profraw"
      '';
      postRun = ''
        mkdir -p $out/profraw
        shopt -s nullglob
        cp $TMPDIR/profraw/*.profraw $out/profraw/ 2>/dev/null || true
        echo "  collected $(ls $out/profraw/ | wc -l) profraw files" | tee -a $out/log
      '';
    }
  ) covTestBinDrvs;

  # Merge + export: all profraws → single profdata → lcov. Object
  # files (--object) are the test binaries themselves — llvm-cov
  # reads the __llvm_covfun/__llvm_covmap sections embedded at
  # compile time. Unlike crane's coverage (which reads the main
  # binaries), here we read the TEST binaries directly since they
  # contain both the library code (via --test) and test-only code.
  coverageLcov = pkgs.runCommand "c2n-coverage-lcov" { } ''
    set -euo pipefail
    mkdir -p $TMPDIR/raw $out
    ${lib.concatMapStringsSep "\n" (d: ''
      for f in ${d}/profraw/*.profraw; do
        [ -e "$f" ] && cp "$f" $TMPDIR/raw/ || true
      done
    '') (lib.attrValues covProfrawDrvs)}
    shopt -s nullglob
    profraws=($TMPDIR/raw/*.profraw)
    if [ ''${#profraws[@]} -eq 0 ]; then
      echo "ERROR: no profraws collected from any test binary" >&2
      exit 1
    fi
    echo "merging ''${#profraws[@]} profraw files"
    ${sysroot}/llvm-profdata merge -sparse "''${profraws[@]}" -o $TMPDIR/merged.profdata

    # Object list: every test binary from every cov member.
    # llvm-cov reads coverage-map sections; any binary not present
    # in the profdata is simply reported as 0% (not an error).
    objs=""
    ${lib.concatMapStringsSep "\n" (d: ''
      for t in ${d}/tests/*; do
        [ -x "$t" ] && objs="$objs --object $t"
      done
    '') (lib.attrValues covTestBinDrvs)}

    # Export lcov. buildRustCrate's --remap-path-prefix maps the
    # sandbox build dir to `/`, so source paths in the profile data
    # are `/rio-common/src/lib.rs` (workspace crates) or
    # `/tokio-1.50.0/src/lib.rs` (deps). We can't reliably filter
    # deps at llvm-cov level (no common prefix); filter at lcov
    # level via --extract instead.
    ${sysroot}/llvm-cov export \
      --format=lcov \
      --instr-profile=$TMPDIR/merged.profdata \
      $objs \
      2>/dev/null > $TMPDIR/raw.lcov

    # Strip leading slash so paths are repo-relative
    # (`rio-common/src/lib.rs` not `/rio-common/src/lib.rs`) — same
    # convention as crane's coverage output. Then extract only
    # workspace-crate paths (rio-*/...). --ignore-errors unused:
    # lcov 2.x treats an unmatched --substitute pattern as an error
    # by default; we don't know ahead of time whether every pattern
    # fires (depends on which crates' tests ran).
    ${pkgs.lcov}/bin/lcov \
      --ignore-errors unused \
      --substitute 's|^/||' \
      -a $TMPDIR/raw.lcov -o $TMPDIR/stripped.lcov
    ${pkgs.lcov}/bin/lcov \
      --extract $TMPDIR/stripped.lcov 'rio-*' \
      -o $out/lcov.info

    echo "=== c2n Coverage Summary ==="
    ${pkgs.lcov}/bin/lcov --summary $out/lcov.info
  '';

in
{
  # Per-member check derivations. Exposed for targeted runs:
  #   nix build .#c2n-clippy-rio-scheduler
  clippy = clippyDrvs;
  testBins = testBinDrvs;
  tests = testRunDrvs;
  doc = docDrvs;

  # Coverage (only populated when c2nCov is passed). covTestBins
  # is the instrumented compile; covProfraw runs them + collects raw
  # profile data; coverage is the merged lcov.
  covTestBins = covTestBinDrvs;
  covProfraw = covProfrawDrvs;
  coverage = if c2nCov != null then coverageLcov else null;

  # Aggregate checks (CI entry points). All of these go in checks.*:
  #   checks.c2n-clippy  — fails if any workspace member has clippy warnings
  #   checks.c2n-test    — fails if any test binary exits nonzero
  #   checks.c2n-doc     — fails if rustdoc errors on any member
  clippyCheck = clippyAll;
  testCheck = testRunAll;
  docCheck = docAll;

  # The toolchain wrappers, exposed for debugging / manual invocation:
  #   nix build .#packages.x86_64-linux.c2n-clippy-rustc
  #   ./result/bin/rustc --version   # prints clippy-driver version
  inherit clippyRustc rustdocRustc;
}
