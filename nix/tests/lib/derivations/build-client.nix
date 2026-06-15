# Fixture graph for vm-build-client-standalone: evaluated IN THE VM by
# the REAL eval parent (rio-eval, `--file` mode — `rio build` cannot
# pass `--arg`, so the input paths are absolute literals the testScript
# stages first):
#
#   /tmp/work/bb          static busybox copied OUT of the client's
#                         store (a source already at a store path would
#                         bypass ingest)
#   /tmp/work/src         small directory source with a known marker
#   /tmp/work/note.patch  single-file source root
#   /tmp/work/link        symlink source root (dangling on purpose; the
#                         build only reads its target string)
#
# Deliberate constraints:
#   - system is a constant: the VM and its worker are x86_64-linux
#
# dep -> consumer gives a 2-node inputDrvs chain, so the submission
# exercises digest-derived edges and the worker executes both in
# dependency order; dep's inputs cover directory, single-file and
# symlink source roots end-to-end (upload → castore FUSE on the
# worker).
let
  bb = /tmp/work/bb;
  src = /tmp/work/src;
  note = /tmp/work/note.patch;
  link = /tmp/work/link;
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
    ${bbx} cat $note >> $out/data
    ${bbx} readlink $link >> $out/data
    ${bbx} echo rio-bc-dep-built >> $out/data
  '' { inherit src note link; };
  # Failure-replay fixtures (cached-failure-replay subtest): a dep that
  # prints 30 recognizable lines and exits 1, so the SECOND-and-later
  # submissions of failingConsumer fail-fast on the poisoned dep and the
  # client must replay this output; and a dep that fails with NO output,
  # so the client must fall back to the persisted reason text.
  failingDep = mkDrv "rio-bc-fail-dep" ''
    ${bbx} seq 1 30 | ${bbx} sed 's/^/rio-bc-fail-marker line /'
    exit 1
  '' { };
  silentDep = mkDrv "rio-bc-fail-silent" ''
    exec ${bbx} false
  '' { };
in
{
  consumer = mkDrv "rio-bc-consumer" ''
    ${bbx} mkdir -p $out
    ${bbx} cat ${dep}/data > $out/summary
    ${bbx} echo rio-bc-consumer-built >> $out/summary
  '' { };
  failingConsumer = mkDrv "rio-bc-fail-consumer" ''
    ${bbx} cat ${failingDep}/never > $out
  '' { };
  silentConsumer = mkDrv "rio-bc-silent-consumer" ''
    ${bbx} cat ${silentDep}/never > $out
  '' { };
}
