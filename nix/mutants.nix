# cargo-mutants (dev-only) — mutates source (swap < for <=, delete a
# statement, replace a return with Default::default()), reruns the test
# suite, flags mutations that SURVIVE — code paths the tests don't
# actually constrain. Tracey answers "is this spec rule covered";
# mutants answers "does the test that covers it actually catch bugs".
#
# Scoped via .config/mutants.toml to high-signal targets (scheduler
# state machine, wire primitives, ATerm parser, HMAC verify, manifest
# encoding — ~320 mutations). `nix build .#mutants` runs the sweep;
# survived-count is diffed run-over-run. Exit 2 (survived) and 3
# (timeouts only) are EXPECTED and swallowed; everything else
# propagates. A baseline-health jq gate additionally fails the
# derivation if zero mutations were tested. Findings are a trend metric,
# not a gate.
#
# `packages` not `checks`: hours per run, not something `nix flake
# check` should touch. Dev-only (`nix build .#mutants` when wanted);
# no scheduled cron.
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
# too (dev-only, cold cache is acceptable).
{
  pkgs,
  version,
  unfilteredRoot,
  workspaceFileset,
  # Manifests + lockfile only (nix/lib/filesets.nix). Paired with
  # stubTargetFiles so `cargo metadata` works against a source tree
  # that omits non-target-package `.rs` content.
  manifestsFileset,
  stubTargetFiles,
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
      # Full sweep's baseline runs the WHOLE workspace's tests
      # (golden_conformance, postgres, ssh-daemon). Smoke is
      # scoped to one package and skips all that.
      withWorkspaceTestDeps ? true,
      # Source slice to stage. The full sweep mutates files
      # across the workspace and needs `workspaceFileset`;
      # `mutants-smoke` only builds/tests `rio-auth` and stages
      # just its in-tree dep closure + manifests so a `.rs` edit
      # outside rio-auth/rio-common doesn't rehash the ~5min drv.
      srcFileset ? workspaceFileset,
    }:
    pkgs.stdenv.mkDerivation (
      sysCrateEnv.allEnv
      // lib.optionalAttrs withWorkspaceTestDeps {
        PG_BIN = "${pkgs.postgresql_18}/bin";
        inherit (goldenTestEnv)
          RIO_GOLDEN_TEST_PATH
          RIO_GOLDEN_CA_PATH
          RIO_GOLDEN_FORCE_HERMETIC
          ;
      }
      // {
        inherit pname version;

        src = lib.fileset.toSource {
          root = unfilteredRoot;
          fileset = lib.fileset.unions [
            srcFileset
            ../.config/mutants.toml
            ../.config/nextest.toml
          ];
        };

        cargoDeps = rustPlatformStable.importCargoLock {
          lockFile = ../Cargo.lock;
        };

        nativeBuildInputs =
          with pkgs;
          [
            rustStable
            rustPlatformStable.cargoSetupHook
            cargo-mutants
            cargo-nextest
            jq
            pkg-config
            protobuf
            cmake
          ]
          ++ lib.optionals withWorkspaceTestDeps [
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
        NEXTEST_HIDE_PROGRESS_BAR = "1";
        # Same shape as crate2nix.nix's sqlxOffline: no DATABASE_URL
        # exists in the sandbox, so this pins sqlx-macros to the offline
        # diagnostic path ("run cargo sqlx prepare", not "set
        # DATABASE_URL"). SQLX_OFFLINE_DIR can't be a static env var
        # here — the cache lives in the unpacked source, so it's
        # exported in buildPhase once $PWD is known.
        SQLX_OFFLINE = "true";

        # `--in-place`: mutate the unpacked source in $PWD
        # (cargoSetupHook unpacks to a writable tmpdir). Cheaper
        # than the default copy-per-mutation mode when running
        # inside a throwaway sandbox anyway.
        #
        # `--no-shuffle` is the default in current cargo-mutants
        # but kept explicit for the run-over-run diff guarantee.
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
          # When `srcFileset` is narrowed (mutants-smoke), `cargo metadata`
          # still loads the full workspace graph and needs every member's
          # auto-detected target file to exist. Synthesize empty stubs for
          # the absent ones (no-op `touch` for the ones that ARE staged).
          ${stubTargetFiles}
          # Single-channel sqlx contract (rio-buildhash): .sqlx is staged
          # by workspaceFileset (nix/lib/filesets.nix), but without
          # SQLX_OFFLINE_DIR the trackers in rio-{scheduler,store,
          # controller}/build.rs take the Untracked arm — every cargo
          # invocation gets a per-run-unique RIO_SQLX_HASH plus an
          # always-stale watch, force-recompiling those crates and their
          # dependents on each of cargo-mutants' per-mutation build+test
          # invocations (~2 x ~320 mutations, plus the whole-workspace
          # baseline) — pure waste in a sandbox with no rustc-wrapper
          # cache, and 3 cargo:warning lines of spam per invocation.
          # Absolute on purpose: the tracker refuses relative paths.
          # Guarded on existence so it stays inert in mutants-smoke,
          # which stages no .sqlx and compiles no sqlx crate (rio-auth ->
          # rio-common -> workspace-hack); if smoke ever grows an sqlx
          # crate the tracker still fails loud (unkeyed + warning) rather
          # than silently falling through.
          if [ -d "$PWD/.sqlx" ]; then
            export SQLX_OFFLINE_DIR="$PWD/.sqlx"
          fi
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
          # stream for the run-over-run diff. No `|| echo 0`
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
  # .config/mutants.toml. Dev-only (hours).
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
  #
  # srcFileset is narrowed to rio-auth's in-tree dep closure
  # (rio-auth → rio-common → workspace-hack) + manifests so the
  # ~5min cold cargo build only re-runs when one of those crates
  # — or a manifest — changes, not on every workspace `.rs` edit.
  # The closure is hardcoded (cargo can't tell us at eval time);
  # if rio-auth gains a new in-tree dep, cargo errors loudly on
  # the missing `src/lib.rs` — add the crate dir here.
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
    withWorkspaceTestDeps = false;
    srcFileset = lib.fileset.unions [
      manifestsFileset
      ../rio-auth
      ../rio-common
      ../workspace-hack
    ];
  };

  # Post-run report validator on the FULL mutants output. NOT a
  # smoke test — has `${mutants}` as a build input, so building
  # this builds the multi-hour sweep. Dev-only: `nix build .#mutants
  # .#mutants.report-assert`; nix substitutes the mutants output from
  # cache if it was already built, so the assert adds O(seconds).
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
