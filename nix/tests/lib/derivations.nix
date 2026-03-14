# Shared test-derivation factory.
#
# Two kinds of things live here:
#
# 1. Path literals to `.nix` files that get evaluated IN THE VM via
#    `nix-build --arg busybox '(builtins.storePath ...)' <file>`.
#    These take `{ busybox }:` at the top and use `builtins.currentSystem`.
#    Keep them as separate files — `pkgs.writeText` would obscure the code
#    in VM-side errors (store path instead of filename).
#
# 2. `pkgs.writeText` factories for parameterized derivations where each
#    scenario needs a different marker/sleep. These produce a store path
#    that `nix-build` in the VM can read.
{ pkgs }:
{
  # ── Path literals (in-VM nix-build targets) ─────────────────────────

  # 4 parallel leaves + 1 collector. Exercises fanout distribution and
  # FUSE fetch across workers. Phase2a pattern: `nix-build fanout.nix` →
  # rio-root output contains 4 "rio-leaf-N" lines in its stamp file.
  fanout = ./derivations/fanout.nix;

  # A → B → C sequential chain. Each step echoes PHASE2B-LOG-MARKER to
  # stderr → validates the worker LogBatcher → gateway STDERR_NEXT chain.
  # Also does `ls -la ${dep}/` to exercise FUSE readdir.
  chain = ./derivations/chain.nix;

  # Multi-attr set: `all` (chain+solo for critical-path), `bigthing`
  # (pname in env for estimator lookup), `bigblob` (300KiB → chunked).
  sizeclass = ./derivations/sizeclass.nix;

  # builtin:fetchurl busybox FOD + raw consumer. Cold-store only.
  # Takes `{ tag, sleepSecs }` at nix-build time via `--arg`.
  coldBootstrap = ./derivations/cold-bootstrap.nix;

  # ── Parameterized factories (pkgs.writeText) ────────────────────────

  # Single trivial leaf. busybox builder, echoes marker to $out.
  # Each scenario can mint its own distinct derivation so builds don't
  # DAG-dedup to the first scenario's result.
  mkTrivial =
    {
      marker,
      sleepSecs ? 0,
    }:
    let
      sleepCmd = pkgs.lib.optionalString (sleepSecs > 0) ''
        ''${busybox}/bin/busybox sleep ${toString sleepSecs}
      '';
    in
    pkgs.writeText "drv-${marker}.nix" ''
      { busybox }:
      derivation {
        name = "rio-test-${marker}";
        builder = "''${busybox}/bin/sh";
        args = [ "-c" '''
          ${sleepCmd}
          echo ${marker} > $out
        ''' ];
        system = builtins.currentSystem;
      }
    '';
}
