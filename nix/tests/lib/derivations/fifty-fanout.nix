# 50 parallel leaves + 1 collector. Scaled-up fanout.nix for load testing.
#
# Shape: 50 independent leaves all ready at once + 1 collector that cat's
# all 50 stamps. With 4 small slots (scheduling fixture: wsmall1:2 +
# wsmall2:2; no-pname leaves route to "small" class so wlarge sits idle)
# this is ~13 dispatch waves.
#
# Proves the scheduler handles bulk-parallel ready-sets without stalling,
# dropping, or deadlocking. The collector's stamp has exactly 50
# "rio-load-N" lines IFF all 50 leaves built AND PutPath'd correctly —
# content check is strictly stronger than any metric assertion.
#
# Linear chain was the plan doc's first thought but 50 serial builds
# at tickIntervalSecs=2 ≈ 150-200s. Fanout exercises the scheduler's
# bulk-ready handling (the actual load concern) in ~40-60s.
{ busybox }:
let
  sh = "${busybox}/bin/sh";
  bb = "${busybox}/bin/busybox";

  mkStep =
    name: deps:
    derivation {
      inherit name;
      system = builtins.currentSystem;
      builder = sh;
      args = [
        "-c"
        ''
          set -e
          ${bb} mkdir -p $out
          ${bb} echo "${name}" > $out/stamp
          ${builtins.concatStringsSep "\n" (map (d: "${bb} cat ${d}/stamp >> $out/stamp") deps)}
        ''
      ];
    };

  # 50 parallel leaves — genList gives rio-load-0..rio-load-49. Each
  # unique by name → unique drv hash → no DAG dedup. No deps → all
  # Ready immediately on SubmitBuild.
  leaves = builtins.genList (i: mkStep "rio-load-${toString i}" [ ]) 50;

  # Collector depends on all 50. $out/stamp has its own name + 50 leaf
  # names. Reading each leaf via ${d}/stamp also exercises FUSE under
  # load (50 NarFromPath fetches on whichever worker builds this).
  root = mkStep "rio-load-root" leaves;
in
root
