# Multi-Nix golden conformance matrix (weekly tier — NOT in .#ci).
#
# Runs `cargo nextest run -E 'binary(golden_conformance)'` once per daemon
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
{
  pkgs,
  inputs,
  system,
  craneLib,
  commonArgs,
  cargoArtifacts,
  goldenTestPath,
  goldenCaPath,
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

  # One nextest run per daemon. The variant's nix package goes first in
  # nativeCheckInputs so `nix-store --load-db` / `nix-store --dump`
  # (which the harness shells out to for db seeding and NAR dumping)
  # use the SAME binary set as the daemon — schema/format parity.
  mkMatrixRun =
    variant: nixPkg:
    craneLib.cargoNextest (
      commonArgs
      // {
        inherit cargoArtifacts;
        pname = "rio-golden-${variant}";
        # Only run the golden_conformance binary — the rest of the
        # workspace suite is the per-push nextest check's job.
        cargoNextestExtraArgs = "--no-tests=warn --profile ci -E 'binary(golden_conformance)'";
        nativeCheckInputs = with pkgs; [
          nixPkg
          openssh
          postgresql_18
        ];
        # Absolute daemon path — the harness prefers this over PATH so
        # log output records exactly which binary was exercised.
        RIO_GOLDEN_DAEMON_BIN = "${nixPkg}/bin/nix-daemon";
        RIO_GOLDEN_DAEMON_VARIANT = variant;
        RIO_GOLDEN_TEST_PATH = "${goldenTestPath}";
        RIO_GOLDEN_CA_PATH = "${goldenCaPath}";
        RIO_GOLDEN_FORCE_HERMETIC = "1";
        NEXTEST_HIDE_PROGRESS_BAR = "1";
      }
    );
in
pkgs.linkFarm "rio-golden-matrix" (
  pkgs.lib.mapAttrsToList (variant: nixPkg: {
    name = variant;
    path = mkMatrixRun variant nixPkg;
  }) daemons
)
