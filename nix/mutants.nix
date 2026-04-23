# cargo-mutants weekly tier — mutates source (swap < for <=, delete a
# statement, replace a return with Default::default()), reruns the test
# suite, flags mutations that SURVIVE — code paths the tests don't
# actually constrain. Tracey answers "is this spec rule covered";
# mutants answers "does the test that covers it actually catch bugs".
#
# Scoped via .config/mutants.toml to high-signal targets (scheduler
# state machine, wire primitives, ATerm parser, HMAC verify, manifest
# encoding — ~320 mutations). Weekly cron invokes `nix build .#mutants`;
# survived-count is diffed week-over-week. Exit 2 (survived) and 3
# (timeouts only) are EXPECTED and swallowed; everything else
# propagates. A baseline-health jq gate additionally fails the
# derivation if zero mutations were tested. Findings are a trend metric,
# not a gate.
#
# `packages` not `checks` (same as golden-matrix): hours per run, not
# something `nix flake check` should touch.
#
# crate2nix port: cargo-mutants fundamentally needs a writable cargo
# workspace (it mutates source in-place and re-invokes `cargo build` +
# `cargo nextest run` per mutation). crate2nix's per-crate-drv model
# doesn't map to that workflow — so this derivation BYPASSES crate2nix
# entirely and uses the same stdenv.mkDerivation + importCargoLock +
# cargoSetupHook pattern as the `deny` check and nix/fuzz.nix. The
# vendored dep tree is cached (same Cargo.lock as the main build), so
# the only per-invocation cost is the baseline cargo build +
# per-mutation rebuilds — same as it ever was under crane. No dep-level
# caching across invocations, but that was true of crane's buildDepsOnly
# too (weekly-tier, cold cache each cron run is acceptable).
{
  pkgs,
  version,
  unfilteredRoot,
  workspaceFileset,
  rustStable,
  rustPlatformStable,
  sysCrateEnv,
  goldenTestEnv,
  # inputs.nix.packages.${system}.nix — test-time dep (baseline run hits
  # the whole workspace; rio-builder spawns `nix-daemon --stdio`).
  nixPkg,
}:
let
  inherit (pkgs) lib;

  mkMutants =
    {
      pname,
      mutantsArgs,
      assertCaught ? false,
    }:
    pkgs.stdenv.mkDerivation (
      sysCrateEnv.allEnv
      // {
        inherit pname version;

        src = lib.fileset.toSource {
          root = unfilteredRoot;
          fileset = lib.fileset.unions [
            workspaceFileset
            ../.config/mutants.toml
            ../.config/nextest.toml
          ];
        };

        cargoDeps = rustPlatformStable.importCargoLock {
          lockFile = ../Cargo.lock;
        };

        nativeBuildInputs = with pkgs; [
          rustStable
          rustPlatformStable.cargoSetupHook
          cargo-mutants
          cargo-nextest
          jq
          pkg-config
          protobuf
          cmake
          # Test-time deps (baseline run hits the whole workspace).
          # Same set as crateChecks' runtimeTestInputs.
          nixPkg
          openssh
          postgresql_18
        ];

        buildInputs =
          with pkgs;
          [
            openssl
            llvmPackages.libclang.lib
          ]
          ++ sysCrateEnv.allLibs;

        # cmake is in nativeBuildInputs for aws-lc-sys's build.rs,
        # not for this derivation's configurePhase. The cmake setup
        # hook would otherwise look for CMakeLists.txt at source
        # root — there isn't one.
        dontUseCmakeConfigure = true;

        PROTOC = "${pkgs.protobuf}/bin/protoc";
        LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
        PG_BIN = "${pkgs.postgresql_18}/bin";
        # Golden fixture paths — the baseline run hits
        # golden_conformance (workspace-wide). `inherit` from
        # the shared goldenTestEnv attrset so new golden
        # fixture vars propagate here automatically.
        inherit (goldenTestEnv)
          RIO_GOLDEN_TEST_PATH
          RIO_GOLDEN_CA_PATH
          RIO_GOLDEN_FORCE_HERMETIC
          ;
        NEXTEST_HIDE_PROGRESS_BAR = "1";

        # `--in-place`: mutate the unpacked source in $PWD
        # (cargoSetupHook unpacks to a writable tmpdir). Cheaper
        # than the default copy-per-mutation mode when running
        # inside a throwaway sandbox anyway.
        #
        # `--no-shuffle` is the default in current cargo-mutants
        # but kept explicit for the week-over-week diff guarantee.
        #
        # `--output $out`: cargo-mutants creates mutants.out/
        # INSIDE the given dir, so result/mutants.out/outcomes.json.
        #
        # Exit-code contract (mutants.rs/exit-codes.html): 0 = all
        # caught, 2 = mutants survived (EXPECTED — no codebase is
        # 100% mutation-killed), 3 = timeouts only, 4 = baseline
        # failed, 1 = usage/internal error. We swallow only 2 and
        # 3 (expected non-zero), propagate everything else, AND
        # belt-and-braces jq-check that the mutation phase
        # actually ran (non-baseline outcomes > 0).
        buildPhase = ''
          runHook preBuild
          mkdir -p $out
          cargo mutants \
            --in-place --no-shuffle \
            ${lib.escapeShellArgs mutantsArgs} \
            --output $out \
            || { rc=$?; [ $rc -eq 2 ] || [ $rc -eq 3 ] || exit $rc; }

          # Baseline-health gate: if outcomes.json has zero
          # MUTATION outcomes (everything that isn't the baseline
          # Success/Failure entry), the baseline failed and the
          # run is void. Fail loud — cat debug.log so the build
          # log shows the actual nextest failure. Catches the
          # case where cargo-mutants exits 0 with an empty
          # outcomes list (graceful baseline-skip) as well as
          # the file-missing case (jq → stderr → tested=0).
          tested=$(jq '[.outcomes[] | select(.summary != "Success" and .summary != "Failure")] | length' \
            $out/mutants.out/outcomes.json 2>/dev/null || echo 0)
          if [ "$tested" -eq 0 ]; then
            echo "mutants baseline failed — zero mutations tested" >&2
            cat $out/mutants.out/debug.log >&2 2>/dev/null || true
            exit 1
          fi
          runHook postBuild
        '';

        installPhase = ''
          runHook preInstall
          # Extract caught/missed counts from the JSON outcome
          # stream for the weekly-diff step. No `|| echo 0`
          # fallback: the baseline-health gate above already
          # fails if outcomes.json is missing — a jq failure
          # here is a real error (malformed JSON).
          jq '[.outcomes[] | select(.summary == "CaughtMutant")] | length' \
            $out/mutants.out/outcomes.json > $out/caught-count
          jq '[.outcomes[] | select(.summary == "MissedMutant")] | length' \
            $out/mutants.out/outcomes.json > $out/missed-count
          ${lib.optionalString assertCaught ''
            if [ "$(cat $out/caught-count)" -eq 0 ]; then
              echo "FAIL: smoke run tested $tested mutations but caught zero — kill detection broken?" >&2
              exit 1
            fi
          ''}
          runHook postInstall
        '';
      }
    );

  # Full sweep — ~320 mutations across high-signal targets via
  # .config/mutants.toml. Weekly tier (hours).
  mutants = mkMutants {
    pname = "rio-mutants";
    mutantsArgs = [
      "--config"
      ".config/mutants.toml"
      "--timeout-multiplier"
      "2.0"
    ];
  };

  # Bounded smoke — proves the mutate→rebuild→retest→classify
  # pipeline works end-to-end. One small file in rio-auth (no
  # postgres/golden test deps in its baseline), 30s per mutation.
  # Dominant cost is the cold cargo build of rio-auth's dep tree
  # (~5min); the mutations themselves add seconds. Asserts ≥1
  # tested AND ≥1 caught.
  mutants-smoke = mkMutants {
    pname = "rio-mutants-smoke";
    mutantsArgs = [
      "--package"
      "rio-auth"
      "--file"
      "rio-auth/src/jwt.rs"
      "--timeout"
      "30"
    ];
    assertCaught = true;
  };

  # Post-run report validator on the FULL mutants output. NOT a
  # smoke test — has `${mutants}` as a build input, so building
  # this builds the multi-hour sweep. Weekly cron sequences `nix
  # build .#mutants .#mutants-report-assert`; nix substitutes the
  # mutants output from cache, so this adds O(seconds).
  # Belt-and-braces with the baseline-health gate inside `mutants`
  # itself — if that gate is relaxed, this still catches a void run.
  mutants-report-assert =
    pkgs.runCommand "mutants-report-assert"
      {
        nativeBuildInputs = [ pkgs.jq ];
      }
      ''
        tested=$(jq '[.outcomes[] | select(.summary != "Success" and .summary != "Failure")] | length' \
          ${mutants}/mutants.out/outcomes.json)
        echo "mutants-report-assert: $tested mutations tested" >&2
        if [ "$tested" -eq 0 ]; then
          echo "FAIL: mutants baseline failed — zero mutations tested" >&2
          cat ${mutants}/mutants.out/debug.log >&2 2>/dev/null || true
          exit 1
        fi
        caught=$(cat ${mutants}/caught-count)
        missed=$(cat ${mutants}/missed-count)
        echo "mutants-report-assert: caught=$caught missed=$missed" >&2
        echo "$tested" > $out
      '';
in
{
  inherit mutants mutants-smoke mutants-report-assert;
}
