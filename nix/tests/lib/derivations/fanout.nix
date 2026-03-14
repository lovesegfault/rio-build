# A 5-node fan-out + collect DAG using only statically-linked busybox.
#
# Shape: 4 parallel leaves + 1 collector. With 2 workers at max_builds=2,
# the 4 ready leaves should distribute across both workers (2 each), then
# the collector runs on one worker after all leaves complete.
#
# Critically, busybox is the ONLY external store path referenced. The
# pkgsStatic build is statically linked (empty runtime closure), so seeding
# the rio-store with just that one path is sufficient for FUSE to serve all
# build inputs.
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
          set -ex
          ${bb} mkdir -p $out
          ${bb} echo "${name}" > $out/stamp
          ${builtins.concatStringsSep "\n" (map (d: "${bb} cat ${d}/stamp >> $out/stamp") deps)}
          ${bb} echo "built on $(${bb} hostname || ${bb} echo unknown)" >> $out/stamp
        ''
      ];
    };

  # 4 parallel leaves — no deps, all ready simultaneously.
  leaf1 = mkStep "rio-leaf-1" [ ];
  leaf2 = mkStep "rio-leaf-2" [ ];
  leaf3 = mkStep "rio-leaf-3" [ ];
  leaf4 = mkStep "rio-leaf-4" [ ];

  # Collector — depends on all leaves, becomes ready once they complete.
  root = mkStep "rio-root" [
    leaf1
    leaf2
    leaf3
    leaf4
  ];
in
root
