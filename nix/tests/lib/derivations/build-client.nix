# Fixture graph for vm-build-client-standalone: evaluated IN THE VM by
# the REAL eval parent (rio-eval, `--file` mode — `rio build` cannot
# pass `--arg`, so the input paths are absolute literals the testScript
# stages first):
#
#   /tmp/work/bb   static busybox copied OUT of the client's store (a
#                  source already at a store path would bypass ingest)
#   /tmp/work/src  small directory source with a known marker file
#
# Deliberate constraints (current coordinator limitations):
#   - source roots are DIRECTORIES (file/symlink roots are a TODO in
#     rio-build-cli upload.rs)
#   - no builtins.toFile (streamed content is skipped at upload with a
#     stats counter, rio-evalstore store.rs)
#   - system is a constant: the VM and its worker are x86_64-linux
#
# dep -> consumer gives a 2-node inputDrvs chain, so the submission
# exercises digest-derived edges and the worker executes both in
# dependency order.
let
  bb = /tmp/work/bb;
  src = /tmp/work/src;
  sh = "${bb}/bin/sh";
  bbx = "${bb}/bin/busybox";
  mkDrv =
    name: script: extra:
    derivation (
      {
        inherit name;
        system = "x86_64-linux";
        builder = sh;
        args = [
          "-c"
          script
        ];
      }
      // extra
    );
  dep = mkDrv "rio-bc-dep" ''
    ${bbx} mkdir -p $out
    ${bbx} cat $src/data.txt > $out/data
    ${bbx} echo rio-bc-dep-built >> $out/data
  '' { inherit src; };
in
{
  consumer = mkDrv "rio-bc-consumer" ''
    ${bbx} mkdir -p $out
    ${bbx} cat ${dep}/data > $out/summary
    ${bbx} echo rio-bc-consumer-built >> $out/summary
  '' { };
}
