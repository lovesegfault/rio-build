# Flat-hash FOD that ALWAYS fails: origin URL 404s and the declared
# outputHash has no entry under {mirror}/sha256/{hex} either (the
# fetcher-split scenario only seeds the hashed-mirror for the
# fod-dead-origin probe).
#
# P0308 regression guard: a failing FOD's daemon post-build
# `deletePath($out)` must propagate quickly. JIT FUSE lookup classifies
# the output basename as NotInput → ENOENT without contacting the store,
# so the daemon's stat returns immediately and BuildResult{PermanentFailure}
# reaches the client. The subtest asserts failure within a time bound;
# a hang means lookup fell through to gRPC.
#
# Evaluated IN THE VM via nix-build. Do not reference host-eval paths.
{
  url ? "http://upstream-v4/nonexistent",
}:
builtins.derivation {
  name = "rio-fod-fail";
  builder = "builtin:fetchurl";
  system = "builtin";
  inherit url;
  outputHashMode = "flat";
  outputHashAlgo = "sha256";
  outputHash = builtins.hashString "sha256" "rio-fod-fail-never-served\n";
  unpack = false;
}
