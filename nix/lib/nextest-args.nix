# nextest runtime/source args for nix/checks.nix.
#
# These are the per-member runtime inputs, env vars, source filesets,
# and nextest CLI flags that the crate2nix test harness needs to run
# `cargo-nextest` against pre-built test binaries. flake.nix merges
# this attrset with the pass-through bindings (pkgs, crateBuild,
# crateBuildCov, ...) it already has in scope before importing
# nix/checks.nix.
#
# Extracted from flake.nix's `crateChecks` import args — paths are
# rebased onto `unfilteredRoot` (see nix/lib/filesets.nix for the
# same rule).
{
  pkgs,
  # Workspace root path (`./.` from flake.nix).
  unfilteredRoot,
  # Workspace fileset union (nix/lib/filesets.nix).
  workspaceFileset,
  # Manifests + lockfile only (nix/lib/filesets.nix). Underlies
  # nextestRunSrc; paired with stubTargetFiles.
  manifestsFileset,
  # Per-member full filesets — WIDE (tests/ + proptest-regressions/).
  memberFilesets,
  # Bash snippet that creates empty stub target files so cargo
  # metadata works against a manifests-only source (nix/lib/filesets.nix).
  stubTargetFiles,
  # RIO_GOLDEN_* env vars for golden conformance tests.
  goldenTestEnv,
  # Nix CLI without its self-test gate — what test runtimes spawn.
  nixForTests,
}:
let
  # Crates whose test runtime needs the nix CLI / a
  # postgres server in PATH. Shared by runtimeTestInputs
  # (what's installed) AND testEnv (what env vars are
  # set), so a new PG-using crate only needs adding to
  # one place — diverging lists would give a test PG_BIN
  # but no `postgres` binary, or vice versa.
  needsNix = [
    "rio-store"
    "rio-scheduler"
    "rio-builder"
    "rio-gateway"
    "rio-test-support"
    "rio-cli"
    "xtask"
  ];
  needsPg = [
    "rio-store"
    "rio-scheduler"
    "rio-builder"
    "rio-gateway"
    "rio-controller"
    "rio-migrations"
    "rio-test-support"
  ];
  # Helm templates whose expr:/query: blocks the alert-parity tests
  # (tests/alert_metrics.rs in rio-scheduler and rio-store) parse at
  # test runtime.
  alertTemplates = pkgs.lib.fileset.unions [
    (unfilteredRoot + "/infra/helm/rio-build/templates/prometheusrule.yaml")
    (unfilteredRoot + "/infra/helm/rio-build/templates/store-scaledobject.yaml")
    (unfilteredRoot + "/infra/helm/rio-build/templates/gateway-scaledobject.yaml")
  ];
in
{
  # Runtime inputs for test execution, keyed by member (null =
  # aggregate run = union). Per-member so nextest-rio-crds
  # doesn't drag postgres+nix-daemon into its closure.
  #   nix-cli — golden conformance (nix-store --dump,
  #     nix-instantiate); rio-builder spawns nix-daemon --stdio
  #   postgresql — rio-test-support ephemeral PG bootstrap
  #   openssh — rio-gateway SSH accept tests
  runtimeTestInputs =
    member:
    pkgs.lib.optional (member == null || builtins.elem member needsNix) nixForTests
    ++ pkgs.lib.optional (member == null || builtins.elem member needsPg) pkgs.postgresql_18
    ++ pkgs.lib.optional (member == null || member == "rio-gateway") pkgs.openssh;
  # Env vars for test runners, keyed by member. PG_BIN so
  # rio-test-support finds initdb/postgres; RIO_GOLDEN_* so
  # golden tests don't try to `nix build` their fixture
  # in-sandbox. The PG-less members get neither.
  testEnv =
    member:
    pkgs.lib.optionalAttrs (member == null || builtins.elem member needsPg) (
      goldenTestEnv
      // {
        PG_BIN = "${pkgs.postgresql_18}/bin";
      }
    );
  # nextest reuse-build runner. Synthesizes --cargo-metadata
  # and --binaries-metadata JSON from the crate2nix test
  # binaries; runs with the `ci` profile (retries, test
  # groups from .config/nextest.toml). Per-test-process
  # isolation — no PDEATHSIG/libtest thread race, so
  # wrapper-level PG bootstrap not needed. `--no-tests=warn`
  # because rio-cli has zero tests (bin-only crate).
  # Full workspace source for the aggregate nextest run (used
  # when member == null — golden-matrix, coverage). Per-member
  # runs use nextestRunSrc + overlay instead.
  workspaceSrc = pkgs.lib.fileset.toSource {
    root = unfilteredRoot;
    fileset = pkgs.lib.fileset.unions [
      workspaceFileset
      (unfilteredRoot + "/.config/nextest.toml")
      (unfilteredRoot + "/docs/gen/metrics.json")
      alertTemplates
    ];
  };
  # Fileset for the shared cargo-metadata drv and the
  # per-member nextest --workspace-remap base. Manifests +
  # config only — NO source content. cargo metadata only
  # needs target-file EXISTENCE (not content) to discover
  # autotests/autobins, and stubTargetFiles synthesizes
  # those at build time from eval-time pathExists/readDir
  # facts. Keeping tests/ out of this fileset means editing
  # rio-gateway/tests/foo.rs does not rehash the SHARED
  # cargoMetadataJson drv → nextest-rio-store stays cached.
  # Per-member overlays (memberRuntimeSrcs.<member> below)
  # supply real source for the target member's runtime
  # reads.
  nextestRunSrc = pkgs.lib.fileset.toSource {
    root = unfilteredRoot;
    fileset = pkgs.lib.fileset.unions [
      manifestsFileset
      (unfilteredRoot + "/.config/nextest.toml")
      # metrics_registered tests grep the per-component
      # metric set at runtime (rio-test-support
      # grep_spec_names reads ../docs/gen/metrics.json via
      # fs::read_to_string). Lives outside any crate dir,
      # so the per-member overlay never supplies it.
      (unfilteredRoot + "/docs/gen/metrics.json")
      # alert_metrics parity tests (rio-scheduler/rio-store)
      # read the PrometheusRule/ScaledObject templates at
      # runtime — without these in the sandbox fileset the
      # check passes locally and fails sandboxed (or worse,
      # the reverse).
      alertTemplates
    ];
  };
  # Per-member full src/ for the runtime overlay (mkNextestRun
  # cp's the target member's real source into $ws/<member>/ so
  # tests that scan their own crate dir — grep_emitted_names,
  # proptest-regressions replays — see real content). Rooted
  # at the member dir. Derived from the WIDE memberFilesets
  # (tests/ + proptest-regressions/) — DISTINCT from the
  # bin-only `memberSrcs` that feeds crate2nix; this is the
  # only variant that needs tests/ at runtime.
  memberRuntimeSrcs = pkgs.lib.mapAttrs (
    name: fs:
    pkgs.lib.fileset.toSource {
      root = unfilteredRoot + "/${name}";
      fileset = fs;
    }
  ) memberFilesets;
  # Pass through the empty-target-file stub script so checks.nix's
  # cargoMetadataJson can pair it with nextestRunSrc. Defined in
  # nix/lib/filesets.nix alongside manifestsFileset (both are
  # source-tree-slice concerns, shared with deny / mutants-smoke).
  inherit stubTargetFiles;
  nextestExtraArgs = [
    "--profile"
    "ci"
    "--no-tests=warn"
  ];
}
