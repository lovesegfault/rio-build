# FOD + consumer pair for the fetcher-split scenario.
#
# Same shape as cold-bootstrap.nix (builtin:fetchurl FOD → raw-derivation
# consumer), but parameterized for the fetcher-split VM test: url defaults
# to the TEST-NET-3 "public" origin the scenario sets up (203.0.113.1:80,
# outside RFC1918 → passes fetcher-egress NetPol, fails builder-egress).
#
# One nix-build pulls BOTH through the scheduler — the FOD routes to a
# FetcherPool pod (hard_filter at assignment.rs gates on is_fixed_output
# XOR ExecutorKind::Fetcher), the consumer routes to a BuilderPool pod.
# Both completing proves sched.dispatch.fod-to-fetcher end-to-end.
#
# Evaluated IN THE VM via nix-build. Do not reference host-eval paths.
{
  tag ? "split",
  sleepSecs ? 2,
  # Scenario sets up `python3 -m http.server 80` on 203.0.113.1 serving
  # the pre-fetched busybox at /busybox. TEST-NET-3 (RFC5737) is NOT in
  # the fetcher-egress NetPol's `except` list (RFC1918 + link-local +
  # loopback only) — so the 0.0.0.0/0:80 allow fires. Builder-egress has
  # NO such allow → a misrouted FOD on a builder pod fails the fetch.
  url ? "http://203.0.113.1/busybox",
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
  name = "rio-split-${tag}";
  system = "x86_64-linux";
  builder = "${busybox}";
  # cat-equiv: echo the FOD's output path then write a marker. The FOD
  # itself is the builder binary (busybox) so `${busybox}` IS the src.
  # Trivial non-FOD: proves builder pod can execute FOD-sourced code
  # (FUSE-serves the FOD output from rio-store).
  args = [
    "sh"
    "-c"
    "echo ${tag}; read -t ${toString sleepSecs} x < /dev/zero || true; echo ok > $out"
  ];
}
