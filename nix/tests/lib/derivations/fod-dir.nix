# Recursive-hash FOD whose output is a DIRECTORY (`mkdir $out`).
#
# The bug this guards: pre-4832e14f, prepare_sandbox mknod'd a chardev
# 0/0 whiteout at the FOD output path so daemon post-fail cleanup got
# fast ENOENT. Works for flat-mode (open(O_CREAT) replaces a whiteout);
# breaks for recursive-mode dir output — overlayfs `mkdir` onto a
# whiteout returns EIO. The whiteout was dropped once JIT FUSE lookup
# (JitClass::NotInput → ENOENT, ops.rs) made it redundant.
#
# busybox is the same FOD as fod-consumer.nix (output already cached
# after the dispatch subtest), so this build's only fresh work is the
# dir-output FOD itself — routes to a fetcher pod (is_fixed_output).
#
# Evaluated IN THE VM via nix-build. Do not reference host-eval paths.
{
  url ? "http://upstream-v4/busybox",
}:
let
  busybox = builtins.derivation {
    name = "busybox";
    builder = "builtin:fetchurl";
    system = "builtin";
    inherit url;
    outputHashMode = "recursive";
    outputHashAlgo = "sha256";
    outputHash = "sha256-QrTEnQTBM1Y/qV9odq8irZkQSD9uOMbs2Q5NgCvKCNQ=";
    executable = true;
    unpack = false;
  };
in
builtins.derivation {
  name = "rio-fod-dir";
  builder = "${busybox}";
  system = "x86_64-linux";
  # busybox-sh has echo/read builtin but not mkdir — invoke the applet
  # via the same multi-call binary path (the only thing on disk).
  args = [
    "sh"
    "-c"
    "${busybox} mkdir $out && echo dir-fod-content > $out/marker"
  ];
  outputHashMode = "recursive";
  outputHashAlgo = "sha256";
  outputHash = "sha256-m9bLSSSEAMY+Y9UVEZxhnYkLLiht2VOZtf6btHi8IL0=";
}
