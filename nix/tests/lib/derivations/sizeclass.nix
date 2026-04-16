# Phase 2c test derivations.
#
# Default build (`phase2c-derivation.nix`):
#   chain-a → chain-b → chain-c   (sequential, est ~4s total path)
#   solo                          (independent, est ~1s)
#
# With critical-path priority: chain-a has priority ≈ 3×est (sum along
# path), solo has priority ≈ 1×est. So chain-a dispatches BEFORE solo.
#
# Each build sleeps briefly so the dispatch order is observable (without
# the sleep, all four would complete too fast to distinguish). Keep it
# short (1s) — VM tests are slow enough already.
{ busybox }:
let
  inherit (import ./_busybox.nix { inherit busybox; }) bb mkDrv;

  mkStep =
    name: dep:
    mkDrv name ''
      set -e
      ${bb} sleep 1
      ${bb} mkdir -p $out
      ${
        if dep == null then
          ''${bb} echo "root" > $out/mark''
        else
          ''${bb} cat ${dep}/mark > $out/mark && ${bb} echo "${name}" >> $out/mark''
      }
    '' { };

  chainA = mkStep "rio-2c-chain-a" null;
  chainB = mkStep "rio-2c-chain-b" chainA;
  chainC = mkStep "rio-2c-chain-c" chainB;
  solo = mkStep "rio-2c-solo" null;
in
{
  # Default attr: chain + solo. `nix-build phase2c-derivation.nix`.
  all = mkDrv "rio-2c-all" ''
    ${bb} mkdir -p $out
    ${bb} cp ${chainC}/mark $out/chain
    ${bb} cp ${solo}/mark $out/solo
  '' { };

  # Chunk-backend validation. Writes a 300KiB blob that exceeds
  # INLINE_THRESHOLD (256 KiB) so the chunked PutPath path MUST run
  # (other derivations here are ~100-byte text files → inline path
  # only, chunk backend untested). The VM test asserts chunk file
  # count increases after this build.
  bigblob = mkDrv "rio-2c-bigblob" ''
    ${bb} mkdir -p $out
    # 300 KiB of zeros. > INLINE_THRESHOLD → chunked path.
    # dd bs=1024 count=300 = 300 KiB.
    ${bb} dd if=/dev/zero of=$out/blob bs=1024 count=300 2>/dev/null
  '' { };
}
