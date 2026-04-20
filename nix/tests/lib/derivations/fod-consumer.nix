# FOD + consumer pair for the fetcher-split scenario.
#
# Same shape as cold-bootstrap.nix (builtin:fetchurl FOD → raw-derivation
# consumer), but parameterized for the fetcher-split VM test: url defaults
# to the fixture's `upstream-v4` node — reached from a v6-only fetcher pod
# via CoreDNS dns64 + Jool NAT64. The 64:ff9b::/96 synthesised address is
# outside any Cilium cluster identity → passes fetcher-egress's world:80
# allow, fails builder-egress (no world rule).
#
# One nix-build pulls BOTH through the scheduler — the FOD routes to a
# kind=Fetcher pod (hard_filter at assignment.rs gates on is_fixed_output
# XOR ExecutorKind::Fetcher), the consumer routes to a kind=Builder pod.
# Both completing proves sched.dispatch.fod-to-fetcher end-to-end.
#
# Evaluated IN THE VM via nix-build. Do not reference host-eval paths.
{
  tag ? "split",
  sleepSecs ? 2,
  # Scenario serves the pre-fetched busybox at upstream-v4:/busybox.
  # Fetcher pod resolves `upstream-v4` via CoreDNS dns64 → 64:ff9b::<v4>
  # → routes via host's 64:ff9b::/96 → edge Jool. fetcher-egress
  # toEntities:[world]:80 fires; builder-egress has NO world rule → a
  # misrouted FOD on a builder pod fails the fetch.
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
