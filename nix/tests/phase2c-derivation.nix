# Phase 2c test derivations.
#
# Default build (`phase2c-derivation.nix`):
#   chain-a → chain-b → chain-c   (sequential, est ~4s total path)
#   solo                          (independent, est ~1s)
#
# With critical-path priority: chain-a has priority ≈ 3×est (sum along
# path), solo has priority ≈ 1×est. So chain-a dispatches BEFORE solo.
#
# `-A bigthing` build:
#   Single derivation with `pname = "rio-2c-bigthing"` in env. The VM
#   test pre-seeds build_history with 120s EMA for that pname, so the
#   estimator picks it up and classify() routes to "large". The pname
#   MUST be in env (not just `name`) — gateway's translate.rs reads
#   pname from `drv.env().get("pname")`.
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
{
  # Default attr: chain + solo. `nix-build phase2c-derivation.nix`.
  all = derivation {
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
  };

  # Size-class routing test: `nix-build phase2c-derivation.nix -A bigthing`.
  # pname MUST be in the env attrset — gateway reads drv.env().get("pname"),
  # NOT the derivation name. Without this, estimator lookup misses and
  # defaults to 30s → classifies "small" → test fails for the wrong reason.
  bigthing = derivation {
    name = "rio-2c-bigthing";
    pname = "rio-2c-bigthing"; # ← this becomes an env var; estimator keys on it
    system = builtins.currentSystem;
    builder = sh;
    args = [
      "-c"
      ''
        ${bb} mkdir -p $out
        ${bb} echo big > $out/mark
      ''
    ];
  };

  # Chunk-backend validation. Writes a 300KiB blob that exceeds
  # INLINE_THRESHOLD (256 KiB) so the chunked PutPath path MUST run
  # (other derivations here are ~100-byte text files → inline path
  # only, chunk backend untested). The VM test asserts chunk file
  # count increases after this build.
  bigblob = derivation {
    name = "rio-2c-bigblob";
    system = builtins.currentSystem;
    builder = sh;
    args = [
      "-c"
      ''
        ${bb} mkdir -p $out
        # 300 KiB of zeros. > INLINE_THRESHOLD → chunked path.
        # dd bs=1024 count=300 = 300 KiB.
        ${bb} dd if=/dev/zero of=$out/blob bs=1024 count=300 2>/dev/null
      ''
    ];
  };
}
