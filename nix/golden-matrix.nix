# Multi-Nix golden conformance matrix (weekly tier — NOT in .#ci).
#
# Runs `cargo-nextest run -E 'binary(golden_conformance)'` once per daemon
# variant, each pointing at a different nix-daemon binary. Surfaces wire-
# protocol divergences across Nix 2.20 / Nix pinned / Nix master / Lix
# before they bite real clients.
#
# Output: a linkFarm keyed by variant name, each entry → the nextest run
# result for that daemon. `nix build .#golden-matrix && ls result/` shows
# one dir per variant.
#
# This deliberately lives under `packages` (not `checks`) so `nix flake
# check` doesn't build it — evaluating four full Nix source trees is a
# multi-minute eval, and building the non-pinned daemons is 20-30 min each.
# Weekly cron invokes `nix build .#golden-matrix` directly.
#
# crate2nix port: reuses crateChecks.mkNextestRun (reuse-build mode — the
# test binaries are already compiled by buildRustCrate, nextest just runs
# them). nextestMeta is shared across all variants (it only depends on
# testBinDrvs + workspaceSrc). Per-variant cost = one nextest invocation
# with a different nix-cli in PATH + RIO_GOLDEN_DAEMON_BIN env.
{
  pkgs,
  inputs,
  system,
  mkNextestRun,
}:
let
  # Daemon package per variant. Package attr names differ across Nix
  # eras: 2.34+ and master expose `nix-cli` (just bin/, no functional-
  # tests buildInput); 2.20 and Lix only have `.default`/`.nix`. The
  # `or` fallback handles future renames without breaking the matrix.
  daemons = {
    nix-pinned = inputs.nix.packages.${system}.nix-cli or inputs.nix.packages.${system}.default;
    nix-stable =
      inputs.nix-stable.packages.${system}.nix-cli or inputs.nix-stable.packages.${system}.default;
    nix-unstable =
      inputs.nix-unstable.packages.${system}.nix-cli or inputs.nix-unstable.packages.${system}.default;
    lix = inputs.lix.packages.${system}.nix-cli or inputs.lix.packages.${system}.default;
  };

  # One nextest run per daemon. The variant's nix package is PREPENDED
  # to nativeBuildInputs (via mkNextestRun's extraRuntimeInputs) so
  # `nix-store --load-db` / `nix-store --dump` (which the harness
  # shells out to for db seeding and NAR dumping) use the SAME binary
  # set as the daemon — schema/format parity. This shadows the
  # module-level pinned nix-cli.
  mkMatrixRun =
    variant: nixPkg:
    mkNextestRun {
      name = "rio-golden-${variant}";
      extraRuntimeInputs = [ nixPkg ];
      extraEnv = {
        # Absolute daemon path — the harness prefers this over PATH so
        # log output records exactly which binary was exercised.
        RIO_GOLDEN_DAEMON_BIN = "${nixPkg}/bin/nix-daemon";
        RIO_GOLDEN_DAEMON_VARIANT = variant;
      };
      # Only run the golden_conformance binary — the rest of the
      # workspace suite is the per-push nextest check's job. The
      # module-level nextestExtraArgs already supply `--profile ci
      # --no-tests=warn`; this appends the filter.
      extraArgs = [
        "-E"
        "binary(golden_conformance)"
      ];
    };
in
pkgs.linkFarm "rio-golden-matrix" (
  pkgs.lib.mapAttrsToList (variant: nixPkg: {
    name = variant;
    path = mkMatrixRun variant nixPkg;
  }) daemons
)
