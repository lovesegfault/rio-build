# Flat-hash FOD whose origin serves the WRONG content: the
# fetcher-split scenario's upstream-v4 node returns 200 for /bad-hash
# with a body that does not match the declared outputHash, and no
# hashed-mirror entry exists for that hash. The fetch itself succeeds
# (builtin:fetchurl does no content verification — see
# rio-builder/src/builtin_fetchurl.rs); the FOD hash gate in the result
# path (verify_fod_hashes, before upload) must reject the output as
# OutputRejected, and the output path must never appear in the store.
# Drives the fetcher-split fod-bad-hash subtest.
#
# Evaluated IN THE VM via nix-build. Do not reference host-eval paths.
{
  url ? "http://upstream-v4/bad-hash",
}:
builtins.derivation {
  name = "rio-bad-hash-probe";
  builder = "builtin:fetchurl";
  system = "builtin";
  inherit url;
  outputHashMode = "flat";
  outputHashAlgo = "sha256";
  # The scenario serves "rio-bad-hash-actual\n" at /bad-hash; this hash
  # is over a different literal, so the mismatch is guaranteed.
  outputHash = builtins.hashString "sha256" "rio-bad-hash-expected\n";
  unpack = false;
}
