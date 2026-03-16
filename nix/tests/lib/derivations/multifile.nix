# Overlay-readdir correctness probe.
#
# dep writes 5 files; consumer `ls`'s ${dep}/ FIRST — no prior `cat` of any
# child name — then counts lines. When the consumer runs, nix-daemon has
# bind-mounted ${dep} from the overlay merge into the build sandbox. The
# overlay dcache for ${dep} has the directory dentry itself (from the
# bind-mount's path resolution) but NONE of its children — nothing has
# looked up file-a/b/c/d/e by name yet.
#
# Outcomes:
#   count=5  overlayfs reads the full listing from the FUSE lower (either
#            via FUSE_READDIR delegation or some kernel-side shortcut).
#            Correct. If FUSE readdir() is still 0 hits, overlay is
#            reading the lower through a path that doesn't cross
#            /dev/fuse — coverage gap only, not a correctness bug.
#   count<5  overlayfs serves readdir from its own dcache (H1).
#            CORRECTNESS BUG: builds that ls a FUSE-served dep see
#            only the entries they'd already touched by name.
{ busybox }:
let
  sh = "${busybox}/bin/sh";
  bb = "${busybox}/bin/busybox";

  dep = derivation {
    name = "rio-multifile-dep";
    system = builtins.currentSystem;
    builder = sh;
    args = [
      "-c"
      ''
        ${bb} mkdir -p $out
        ${bb} echo a > $out/file-a
        ${bb} echo b > $out/file-b
        ${bb} echo c > $out/file-c
        ${bb} echo d > $out/file-d
        ${bb} echo e > $out/file-e
      ''
    ];
  };
in
derivation {
  name = "rio-multifile-consumer";
  system = builtins.currentSystem;
  builder = sh;
  args = [
    "-c"
    ''
      # ls BEFORE any cat — forces overlayfs to enumerate the FUSE
      # lower with a cold dcache for ${dep}'s children.
      count=$(${bb} ls ${dep}/ | ${bb} wc -l)
      ${bb} echo "$count" > $out
    ''
  ];
}
