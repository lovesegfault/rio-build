# Shared busybox-derivation primitives for the in-VM nix-build targets.
#
# Evaluated IN THE VM via `import ./_busybox.nix { inherit busybox; }` —
# works because derivations.nix exports its path-literal attrs as
# `"${./derivations}/<name>.nix"`, copying this whole directory into the
# store so sibling imports resolve.
#
# `mkDrv name script extra` builds `derivation { name; builder=sh;
# args=["-c" script]; system=currentSystem; } // extra`. Every chain/
# fanout step is this shape; only the script body and (for ca-chain)
# extra attrs vary.
{ busybox }:
rec {
  sh = "${busybox}/bin/sh";
  bb = "${busybox}/bin/busybox";

  mkDrv =
    name: script: extra:
    derivation (
      {
        inherit name;
        system = builtins.currentSystem;
        builder = sh;
        args = [
          "-c"
          script
        ];
      }
      // extra
    );
}
