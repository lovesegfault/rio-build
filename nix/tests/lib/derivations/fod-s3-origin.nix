# Flat-hash FOD whose origin URL uses the unsupported s3:// transport.
# Build succeeds ONLY if builtin:fetchurl applies the s3 limitation per
# CANDIDATE (skip + log) instead of per fetch: the hashed mirror serves
# the same probe body at {mirror}/sha256/{hex}, so a correct fetcher
# never needs the s3 transport at all. A regression to the whole-fetch
# bail (round-16 bug_067) fails this build before the mirror is tried.
#
# Reuses fod-dead-origin's probe literal so the scenario's single
# server-side /sha256/{hex} entry serves both subtests; the output
# *name* is distinct for failure attribution.
#
# Evaluated IN THE VM via nix-build. Do not reference host-eval paths.
{
  url ? "s3://rio-test-bucket/never-fetched",
}:
builtins.derivation {
  name = "rio-s3-mirror-probe";
  builder = "builtin:fetchurl";
  system = "builtin";
  inherit url;
  outputHashMode = "flat";
  outputHashAlgo = "sha256";
  outputHash = builtins.hashString "sha256" "rio-hashed-mirror-probe\n";
  unpack = false;
}
