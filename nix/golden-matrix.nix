# Multi-Nix golden conformance matrix (weekly tier — NOT in checks).
#
# Runs `cargo-nextest run -E 'binary(golden_conformance)'` once per daemon
# variant, each pointing at a different nix-daemon binary. Surfaces wire-
# protocol divergences across Nix 2.28 / Nix pinned / Nix master / Lix
# before they bite real clients.
#
# Output: a linkFarm keyed by variant name, each entry → the nextest run
# result for that daemon. `nix build .#golden-matrix && ls result/` shows
# one dir per variant.
#
# This deliberately lives under `packages` (not `checks`). Weekly cron
# invokes `nix build .#golden-matrix` directly. Three of the four daemons
# (nix-stable / nix-unstable / lix) come from nixpkgs rather than separate
# flake inputs — drops 16 lock nodes and skips the 20-30 min source builds
# the inputs would require. Trade-off: tracks nixpkgs's snapshot of each
# daemon (days-to-weeks lag vs upstream HEAD), which is fine for a weekly
# wire-protocol conformance check.
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
pkgs.linkFarm "rio-golden-matrix" (
  pkgs.lib.mapAttrsToList (variant: nixPkg: {
    name = variant;
    path = mkMatrixRun variant nixPkg;
  }) daemons
)
