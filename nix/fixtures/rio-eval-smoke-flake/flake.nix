# Hermetic flake fixture for the rio-eval-smoke `--flake` leg: NO
# inputs (so lockFlake never fetches and the build sandbox stays
# offline), one trivial derivation referencing `self`. The smoke and
# vm-build-client are the only callers — kept tiny so a CI failure
# here points at the flake path (parseFlakeRef/lockFlake/callFlake),
# not at fixture complexity.
{
  inputs = { };
  outputs =
    { self }:
    let
      mkDrv =
        name: extra:
        derivation (
          {
            inherit name;
            system = "x86_64-linux";
            builder = "/bin/sh";
            args = [
              "-c"
              "cat $src/data.txt > $out"
            ];
            src = "${self}/src-dir";
          }
          // extra
        );
      leaf = mkDrv "smoke-flake-leaf" { };
    in
    {
      packages.x86_64-linux = {
        hello = mkDrv "smoke-flake-hello" { dep = leaf; };
        world = mkDrv "smoke-flake-world" { dep = leaf; };
      };
    };
}
