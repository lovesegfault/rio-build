# FUSE chroot store metadata canonicality probe.
#
# rio's FUSE store filesystem (`stat_to_attr`) MUST present canonical Nix
# store-path metadata (mtime=1, perm 444/555) regardless of the on-disk
# state of the cache files `restore_path_streaming` writes. If either
# layer regresses to passing through `mtime≈now`, every build sees the
# fetch wall-clock as the mtime of its inputs. Nixpkgs'
# `set-source-date-epoch-to-latest.sh` `postUnpackHook` then raises
# `SOURCE_DATE_EPOCH` to that value, and any FOD that bakes
# `SOURCE_DATE_EPOCH` into its output (the `tar --mtime` archives
# `fetchPnpmDeps`/`fetchYarnDeps`/`fetchNpmDeps` produce) becomes
# non-deterministic — `rio-dashboard-pnpm-deps` produced 4 distinct
# hashes over 4 builds before this was caught.
#
# `${busybox}` is on the chroot store's FUSE lower layer; `stat` goes
# overlay → FUSE getattr → `stat_to_attr`. A canonical store path gives
# `1 555` (1s past Epoch, exec/dir perm). The Python subtest reads `$out`
# and asserts the literal — no log parsing, the build IS the probe.
#
# uid/gid intentionally NOT in the manifest: they're remapped through
# the build's user namespace per its `uid_map`. Including them would
# couple the probe to a deployment knob, not a code property.
#
# Evaluated IN THE VM via nix-build. Do not reference host-eval paths.
{ busybox }:
let
  inherit (import ./_busybox.nix { inherit busybox; }) bb mkDrv;
in
mkDrv "rio-canonical-meta-probe" ''
  ${bb} stat -c '%Y %a' ${bb} > $out
'' { }
