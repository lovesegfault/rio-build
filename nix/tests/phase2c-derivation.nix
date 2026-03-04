# Phase 2c test derivations: chain for critical-path + standalone.
#
# Layout:
#   chain-a → chain-b → chain-c   (sequential, est ~4s total path)
#   solo                          (independent, est ~1s)
#
# With critical-path priority: chain-a has priority ≈ 3×est (sum along
# path), solo has priority ≈ 1×est. So chain-a should dispatch BEFORE
# solo even though solo is shorter. The VM test greps dispatch order in
# scheduler logs to verify.
#
# Each build sleeps briefly so the dispatch order is observable (without
# the sleep, all four would complete too fast to distinguish). Keep it
# short (1s) — VM tests are slow enough already.
{ busybox }:
let
  sh = "${busybox}/bin/sh";
  bb = "${busybox}/bin/busybox";

  mkStep =
    name: dep:
    derivation {
      inherit name;
      system = builtins.currentSystem;
      builder = sh;
      args = [
        "-c"
        ''
          set -e
          ${bb} sleep 1
          ${bb} mkdir -p $out
          ${
            if dep == null then
              ''${bb} echo "root" > $out/mark''
            else
              ''${bb} cat ${dep}/mark > $out/mark && ${bb} echo "${name}" >> $out/mark''
          }
        ''
      ];
    };

  chainA = mkStep "rio-2c-chain-a" null;
  chainB = mkStep "rio-2c-chain-b" chainA;
  chainC = mkStep "rio-2c-chain-c" chainB;
  solo = mkStep "rio-2c-solo" null;
in
# Build both — a wrapper that references chainC and solo so nix-build
# builds the whole graph. The VM test asserts on what reaches workers,
# not on this wrapper's output.
derivation {
  name = "rio-2c-all";
  system = builtins.currentSystem;
  builder = sh;
  args = [
    "-c"
    ''
      ${bb} mkdir -p $out
      ${bb} cp ${chainC}/mark $out/chain
      ${bb} cp ${solo}/mark $out/solo
    ''
  ];
}
