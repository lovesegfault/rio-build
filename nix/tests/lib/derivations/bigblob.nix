# Chunk-backend validation. Writes a 300KiB blob that exceeds
# INLINE_THRESHOLD (256 KiB) so the chunked PutPath path MUST run
# (other test derivations are ~100-byte text files → inline path
# only, chunk backend untested). The VM test asserts chunk file
# count increases after this build.
{ busybox }:
let
  inherit (import ./_busybox.nix { inherit busybox; }) bb mkDrv;
in
mkDrv "rio-2c-bigblob" ''
  ${bb} mkdir -p $out
  # 300 KiB of zeros. > INLINE_THRESHOLD → chunked path.
  # dd bs=1024 count=300 = 300 KiB.
  ${bb} dd if=/dev/zero of=$out/blob bs=1024 count=300 2>/dev/null
'' { }
