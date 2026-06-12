# Wide-touch eval fixture (ADR-024 P1 acceptance, review C24): a recursive
# readDir walk over an ingested copy of a large source tree, visiting every
# directory through the eval store's readback path (>=20k distinct dirs on
# a nixpkgs checkout). Forces full decoded-dir residency — the regression
# that fixture choice on the small-mixed trace alone could hide (full
# residency costs ~518ms against the 92ms warm trace budget).
#
# Invoked manually with the pinned nix-cli, same wiring as the
# evalstore-parity check:
#
#   nix eval --file fixture.nix --argstr srcPath /path/to/tree \
#     [--eval-store "rio://?cas=..."] result --json
#
# srcPath must NOT already be a store path (that would skip addToStore and
# bypass the ingest under test).
{ srcPath }:
let
  src = builtins.path {
    path = srcPath;
    name = "widetouch-src";
  };
  walk =
    p:
    let
      entries = builtins.readDir p;
    in
    1
    + builtins.foldl' (
      acc: name: acc + (if entries.${name} == "directory" then walk "${p}/${name}" else 0)
    ) 0 (builtins.attrNames entries);
in
{
  result = {
    source = "${src}";
    dirsVisited = walk src;
  };
}
