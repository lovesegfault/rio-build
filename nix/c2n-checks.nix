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
  # Per-member test dependencies. Populated from Cargo.json's injected
  # devDependencies (see scripts/inject-dev-deps.py). Each value is a
  # list of packageIds from the resolved crates dictionary.
  # Consumed by the test variant's .override { dependencies = ... }.
  # Defaults to empty for members without dev-deps.
  testInputs ? { },
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
    exec ${rustStable}/bin/rustdoc "''${args[@]}" \
      --out-dir target/doc \
      --cap-lints allow \
      -Z unstable-options \
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

  # Coverage-instrumented build variant. Unlike clippy/doc which only
  # override members, coverage needs INSTRUMENTED DEPS too for accurate
  # line attribution (an inlined function from a dep shows up in the
  # caller's profile; without instrumented dep rlib, coverage tools
  # can't map it back). This means a PARALLEL 655-derivation tree
  # with -Cinstrument-coverage on everything.
  #
  # Not implemented here yet — it's a second `cargoNix` instantiation
  # with a modified buildRustCrateForPkgs that injects extraRustcOpts
  # globally. See assessment doc §5 for the design. Deferred to a
  # follow-up commit because it doubles the derivation count and
  # needs profraw merge plumbing.
  # coverageMember = name: base: …;

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
  testDrvs = lib.mapAttrs (name: m: testMember name m.build) members;
  docDrvs = lib.mapAttrs (name: m: docMember name m.build) members;

  clippyAll = pkgs.symlinkJoin {
    name = "c2n-clippy-all";
    paths = lib.attrValues clippyDrvs;
  };

  docAll = pkgs.symlinkJoin {
    name = "c2n-doc-all";
    paths = map (d: d.out) (lib.attrValues docDrvs);
  };

  # Test runner: collects all members' test binaries and runs them.
  # Output is a text log of pass/fail per test. Exit code 1 if any
  # test fails — this is the CI-gate derivation.
  #
  # This is NOT nextest — it's direct execution of the test harness
  # binaries (what `cargo test` does under the hood). nextest would
  # require a nextest-archive format which has its own metadata
  # requirements (Cargo.toml paths, target-dir layout). For a first
  # cut, direct harness execution proves the mechanism; nextest
  # integration is a follow-up (collect test binaries → synthesize a
  # nextest-archive tarball → `nextest run --archive-file`).
  testRunner =
    pkgs.runCommand "c2n-test-run"
      {
        nativeBuildInputs = lib.attrValues testDrvs ++ (testInputs.shared or [ ]);
        # PG_BIN for rio-test-support's ephemeral postgres bootstrap.
        # Mirrors commonArgs in flake.nix.
        PG_BIN = "${pkgs.postgresql_18}/bin";
        # No sandbox-network for tests — unit tests don't hit external
        # services (integration tests that need network are VM-test only).
        __noChroot = false;
      }
      ''
        set -euo pipefail
        export RUST_BACKTRACE=1
        mkdir -p $out

        # Each member's test derivation puts harness binaries in
        # $drv/tests/. Collect and run in declaration order (alphabetical
        # by member name — lib.attrValues sorts by attr name).
        failed=0
        ${lib.concatMapStringsSep "\n" (
          d:
          let
            n = d.crateName;
          in
          ''
            echo "── ${n} ──" >> $out/log
            for t in ${d}/tests/*; do
              echo "  running $(basename $t)" >> $out/log
              if ! "$t" >> $out/log 2>&1; then
                echo "  FAIL: $(basename $t)" >> $out/log
                failed=1
              fi
            done
          ''
        ) (lib.attrValues testDrvs)}

        if [[ "$failed" == 1 ]]; then
          echo "── FAILURES ──" >&2
          cat $out/log >&2
          exit 1
        fi
        echo "all tests passed" >> $out/log
      '';

in
{
  # Per-member check derivations. Exposed for targeted runs:
  #   nix build .#c2n-clippy-rio-scheduler
  clippy = clippyDrvs;
  tests = testDrvs;
  doc = docDrvs;

  # Aggregate checks (CI entry points). All of these go in checks.*:
  #   checks.c2n-clippy  — fails if any workspace member has clippy warnings
  #   checks.c2n-test    — fails if any test binary exits nonzero
  #   checks.c2n-doc     — fails if rustdoc errors on any member
  clippyCheck = clippyAll;
  testCheck = testRunner;
  docCheck = docAll;

  # The toolchain wrappers, exposed for debugging / manual invocation:
  #   nix build .#packages.x86_64-linux.c2n-clippy-rustc
  #   ./result/bin/rustc --version   # prints clippy-driver version
  inherit clippyRustc rustdocRustc;
}
