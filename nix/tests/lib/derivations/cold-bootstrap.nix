# Cold-store bootstrap derivation: builtin:fetchurl FOD + consumer.
#
# Pattern lifted from infra/eks/smoke-test.sh:210-232. This is the ONLY
# shape that works on a completely empty rio store:
#   - builtin:fetchurl needs system="builtin" (worker must advertise it —
#     see commit 5786f82 rio-worker fix)
#   - The FOD is content-addressed → deterministic → one-time build; after
#     the first build it's a cache hit forever
#   - The consumer is a raw derivation (no stdenv) with a 2-path closure
#     total — no shared DAG nodes with other builds, avoids poison cascade
#
# Bootstrap busybox is ultra-minimal (sh/ash/mkdir only — no sleep, no
# touch, no $((arith))). `read -t N < /dev/zero` times out after N seconds
# (the Nix build sandbox provides /dev/zero). `echo > $out` creates the
# output via shell redirect.
#
# Evaluated IN THE VM via nix-build. Do not reference host-eval paths.
{
  # Marker embedded in the derivation name AND echoed to stdout.
  # The protocol-cold test uses this to assert wopQueryMissing returns
  # the correct willBuild set (exact match on .drv names).
  tag ? "cold",
  # How long the consumer builder sleeps. >=2 so worker's 1Hz cgroup
  # cpu.stat poll fires at least once (completion.rs:202 guard skips
  # the DB write if peak_cpu_cores=0).
  sleepSecs ? 2,
}:
let
  busybox = builtins.derivation {
    name = "busybox";
    builder = "builtin:fetchurl";
    system = "builtin";
    url = "http://tarballs.nixos.org/stdenv/x86_64-unknown-linux-gnu/82b583ba2ba2e5706b35dbe23f31362e62be2a9d/busybox";
    outputHashMode = "recursive";
    outputHashAlgo = "sha256";
    outputHash = "sha256-QrTEnQTBM1Y/qV9odq8irZkQSD9uOMbs2Q5NgCvKCNQ=";
    executable = true;
    unpack = false;
  };
in
builtins.derivation {
  name = "rio-cold-${tag}";
  system = "x86_64-linux";
  builder = "${busybox}";
  args = [
    "sh"
    "-c"
    "echo ${tag}; read -t ${toString sleepSecs} x < /dev/zero || true; echo ok > $out"
  ];
}
