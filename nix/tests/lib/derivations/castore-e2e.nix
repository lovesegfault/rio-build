# Input/consumer set for scenarios/castore-e2e.nix (vm-castore-e2e).
#
# Two shared dependencies and four single-purpose consumers, selected
# with `nix-build -A <name>`:
#
#   dep        tiny marker file (≤ stream-threshold → whole-file path)
#   bigdep     300 KiB blob (> RIO_STREAM_THRESHOLD → streaming path, fills /var/rio/chunks)
#   consumer1  cold build: reads both deps, records content/size in $out
#   consumer2  warm build: same reads, distinct output marker
#   consumer3  store-outage build: dispatched while the store route is blocked
#   consumer4  post-mountd-restart build
#
# Each consumer is a distinct derivation (different marker baked into
# the script) so the scheduler never cache-hits one against another,
# while the dep closure {dep, bigdep, busybox} stays identical — that
# identity is what makes the warm-phase "no new promotes" assertion
# meaningful.
{ busybox }:
let
  inherit (import ./_busybox.nix { inherit busybox; }) bb mkDrv;

  dep = mkDrv "rio-castore-dep" ''
    ${bb} mkdir -p $out
    ${bb} echo castore-dep-marker > $out/marker
  '' { };

  bigdep = mkDrv "rio-castore-bigdep" ''
    ${bb} mkdir -p $out
    # 300 KiB of zeros — above the scenario's 64 KiB stream threshold.
    ${bb} dd if=/dev/zero of=$out/blob bs=1024 count=300 2>/dev/null
  '' { };

  mkConsumer =
    name:
    mkDrv name ''
      set -e
      ${bb} mkdir -p $out
      # Read the small dep through the castore lower (whole-file path).
      ${bb} cat ${dep}/marker > $out/summary
      # Read the large dep through the castore lower (streaming path);
      # record the byte count so the test can assert the bytes arrived
      # intact end-to-end.
      ${bb} wc -c < ${bigdep}/blob >> $out/summary
      ${bb} echo ${name} >> $out/summary
    '' { };
in
{
  consumer1 = mkConsumer "rio-castore-consumer1";
  consumer2 = mkConsumer "rio-castore-consumer2";
  consumer3 = mkConsumer "rio-castore-consumer3";
  consumer4 = mkConsumer "rio-castore-consumer4";
}
