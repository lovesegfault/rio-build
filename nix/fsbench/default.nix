# fsbench (P0594): the castore-FUSE micro-bench as two derivations —
# the seed-keyed dataset and the bench run that consumes it through the
# production mount path.
#
# Seed plumbing: `cargo xtask k8s fsbench` sets FSBENCH_SEED on its
# (local, --impure) evaluation, which bakes the seed into the drvs
# before they cross the gateway. The seed is FIXED by default
# (xtask's DEFAULT_SEED, per workload version) so the dataset drv —
# and its ~2.3 GiB of generated content — is built and uploaded once,
# then reused as a remote cache hit on every later run. Pure eval
# (CI's gen-matrix, `nix flake show`) sees getEnv "" and falls back to
# the literal "UNSEEDED" — the attrs evaluate cleanly everywhere, and
# the parser refuses a run whose echoed seed is UNSEEDED, so an
# accidentally seedless submission can never produce a "cold" result
# that was actually warm.
#
# The nonce keys ONLY the bench-run drv: with a stable seed both drvs
# would otherwise hash identically run-over-run, the previous run's
# output would already be valid remotely, and nix would skip executing
# the benchmark. Pure-eval fallback "STATIC" keeps CI eval clean.
{ pkgs, fsbenchBins }:
let
  seed =
    let
      e = builtins.getEnv "FSBENCH_SEED";
    in
    if e == "" then "UNSEEDED" else e;
  runNonce =
    let
      e = builtins.getEnv "FSBENCH_RUN_NONCE";
    in
    if e == "" then "STATIC" else e;
  fsbench-dataset = pkgs.callPackage ./dataset.nix { inherit fsbenchBins seed; };
  fsbench-run = pkgs.callPackage ./bench.nix {
    inherit fsbenchBins seed runNonce;
    dataset = fsbench-dataset;
  };
in
{
  inherit fsbench-dataset fsbench-run;
}
