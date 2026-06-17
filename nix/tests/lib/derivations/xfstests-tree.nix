# xfstests-port fixture tree (castore-FUSE edition).
#
# `dep` materializes a store path whose layout exercises every node
# kind and edge case the xfstests ports (vm-castore-xfstests,
# scenarios/castore-fuse-xfstests.nix) assert through a per-build
# castore-FUSE mount. The path is built in-VM through the gateway, so
# its NAR is ingested into rio-store (Directory DAG + blobs) — the
# content-addressed store is the oracle; every byte/size/mode/name
# below is a constant the assertions can recompute.
#
#   bin/tool          executable regular file
#   data/tool.sh      byte-identical to bin/tool but NOT executable (exec-bit inode split)
#   data/big.bin      1300003-byte blob (odd size, > stream threshold → streaming path)
#   data/small.txt    known short content (≤ threshold → whole-file path)
#   data/empty        zero-byte file
#   dir200/f1..f200   200 entries → multi-round-trip readdir (generic/257)
#   names/...         space, NFC/NFD lookalikes, 255-byte NAME_MAX name (generic/453)
#   links/rel         relative symlink to ../data/small.txt
#   links/longtarget  symlink with a 900-byte target (generic/360)
#   links/dangling    symlink to a nonexistent path
#   links/loop1+loop2 mutual symlink loop (generic/005 ELOOP)
#   dup-a,dup-b/same.txt  identical content in two dirs → one shared inode
#   nest/p1,p2/...    content-identical `shared/` dirs under two DIFFERENT,
#                     non-identical parents (p1/p2 carry distinct marker
#                     files). This is the aliased-directory shape behind
#                     the GNU find fts ENOENT escape: a lookup of one
#                     alias re-parents the other's dentry mid-walk.
#
# `consumer` depends on dep — building it dispatches a real build to a
# worker whose per-build castore-FUSE mount (the overlay lowerdir)
# serves dep, which is the production stack the scenario's direct
# serve-castore mount cannot reach. consumer's $out is three lines: dep
# store path, then the dir200 and names/ entry counts it saw through
# the overlay with a cold dcache.
{ busybox }:
let
  inherit (import ./_busybox.nix { inherit busybox; }) bb mkDrv;

  dep = mkDrv "rio-xfstests-dep" ''
    set -e
    ${bb} mkdir -p $out/bin $out/data $out/dir200 $out/names $out/links $out/dup-a $out/dup-b

    # Executable regular file + a byte-identical non-executable twin.
    ${bb} printf '#!/bin/sh\necho tool-ok\n' > $out/bin/tool
    ${bb} chmod +x $out/bin/tool
    ${bb} printf '#!/bin/sh\necho tool-ok\n' > $out/data/tool.sh

    # Deterministic odd-sized blob (1300003 bytes). No pipefail here, so
    # yes exiting via SIGPIPE when head closes the pipe is fine.
    ${bb} yes rio-xfstests-payload-0123456789abcdef | ${bb} head -c 1300003 > $out/data/big.bin

    ${bb} printf 'rio-xfstests-small\n' > $out/data/small.txt
    ${bb} touch $out/data/empty

    # 200 single-line files for the multi-batch readdir port.
    i=1
    while [ $i -le 200 ]; do
      ${bb} echo "$i" > $out/dir200/f$i
      i=$((i+1))
    done

    # Name edge cases: space, NFC e-acute, NFD e + combining acute,
    # and a 255-byte (NAME_MAX) name.
    ${bb} printf 'space-name\n' > "$out/names/a b"
    ${bb} printf 'nfc-content\n' > "$out/names/$(${bb} printf 'caf\303\251')"
    ${bb} printf 'nfd-content\n' > "$out/names/$(${bb} printf 'cafe\314\201')"
    long=""
    i=0
    while [ $i -lt 255 ]; do
      long="''${long}n"
      i=$((i+1))
    done
    ${bb} printf 'longname-content\n' > "$out/names/$long"

    # Symlinks: resolvable relative, long target, dangling, ELOOP pair.
    ${bb} ln -s ../data/small.txt $out/links/rel
    target=""
    i=0
    while [ $i -lt 900 ]; do
      target="''${target}x"
      i=$((i+1))
    done
    ${bb} ln -s "$target" $out/links/longtarget
    ${bb} ln -s /rio-xfstests-no-such-target $out/links/dangling
    ${bb} ln -s loop2 $out/links/loop1
    ${bb} ln -s loop1 $out/links/loop2

    # Finite 41-link chain (chain0 -> ... -> chain40 -> small.txt):
    # resolving chain0 traverses 41 symlinks, one past the kernel's
    # MAXSYMLINKS=40, so it must ELOOP even though the chain terminates;
    # chain1 (40 traversals) must resolve (generic/005 depth leg).
    i=0
    while [ $i -lt 40 ]; do
      ${bb} ln -s chain$((i+1)) $out/links/chain$i
      i=$((i+1))
    done
    ${bb} ln -s ../data/small.txt $out/links/chain40

    # Identical content in two directories → content-addressed dedup.
    ${bb} printf 'rio-xfstests-dedup\n' > $out/dup-a/same.txt
    ${bb} printf 'rio-xfstests-dedup\n' > $out/dup-b/same.txt

    # Content-identical shared/ dirs under two distinct parents (the
    # parents differ via marker files, so only shared/ is deduped).
    # Exercises directory-inode identity and fts ascent under aliasing.
    ${bb} mkdir -p $out/nest/p1/shared $out/nest/p2/shared
    ${bb} printf 'rio-xfstests-nested-dedup\n' > $out/nest/p1/shared/payload.txt
    ${bb} printf 'rio-xfstests-nested-dedup\n' > $out/nest/p2/shared/payload.txt
    ${bb} printf 'p1-marker\n' > $out/nest/p1/only-p1.txt
    ${bb} printf 'p2-marker\n' > $out/nest/p2/only-p2.txt
  '' { };
in
{
  inherit dep;

  consumer = mkDrv "rio-xfstests-consumer" ''
    set -e
    # ls BEFORE any cat — overlayfs must enumerate the castore-FUSE
    # lower with a cold dcache for dir200's children (multifile.nix
    # only proves this at 5 entries / one READDIR batch) and for the
    # NFC/NFD/space/NAME_MAX names.
    count200=$(${bb} ls ${dep}/dir200 | ${bb} wc -l)
    countnames=$(${bb} ls ${dep}/names | ${bb} wc -l)
    ${bb} echo "${dep}" > $out
    ${bb} echo "$count200" >> $out
    ${bb} echo "$countnames" >> $out
  '' { };
}
