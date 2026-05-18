# Multi-Nix golden conformance matrix.
#
# Runs `cargo-nextest run -E 'binary(golden_conformance)'` once per daemon
# variant, each pointing at a different nix-daemon binary. Surfaces wire-
# protocol divergences across Nix 2.28 / Nix pinned / Nix master / Lix
# before they bite real clients.
#
# Exposed as `checks.golden-<variant>` (4 entries). Three of the four
# daemons (nix-stable / nix-unstable / lix) come from nixpkgs rather than
# separate flake inputs — drops 16 lock nodes and substitutes from
# cache.nixos.org instead of source-building. Only nix-pinned builds from
# source (inputs.nix, already cached for the dev shell). Per-variant cost
# is one nextest invocation with a different nix-cli in PATH +
# RIO_GOLDEN_DAEMON_BIN env; gen-matrix's cache-filter skips all four on
# any PR that doesn't touch the conformance binary's closure.
#
# crate2nix port: reuses crateChecks.mkNextestRun (reuse-build mode — the
# test binaries are already compiled by buildRustCrate, nextest just runs
# them). nextestMeta is shared across all variants (it only depends on
# testBinDrvs + workspaceSrc).
{
  pkgs,
  inputs,
  system,
  mkNextestRun,
}:
let
  # Daemon package per variant. nix-pinned is the flake's explicitly
  # pinned `inputs.nix` (built from source at the tagged ref). The rest
  # are nixpkgs-packaged binaries (substitutable from cache.nixos.org).
  #
  # nix-stable uses the oldest nixVersions.nix_2_* still in nixpkgs
  # (2.20 was dropped). Oldest-protocol-minor coverage is provided by
  # the lix variant (frozen at 1.35 = rio's MIN_CLIENT_VERSION).
  daemons = {
    nix-pinned = inputs.nix.packages.${system}.nix-cli or inputs.nix.packages.${system}.default;
    nix-stable = pkgs.nixVersions.nix_2_28;
    nix-unstable = pkgs.nixVersions.git;
    inherit (pkgs) lix;
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
{
  runs = pkgs.lib.mapAttrs mkMatrixRun daemons;
}
