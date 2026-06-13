# Fixture for the rio-eval-smoke check: a small drv graph evaluated by
# the REAL eval parent (libexpr + the rio:// store, fork workers) and,
# for parity, by the stock pinned nix-cli. NO nixpkgs, NO network.
#
# `system` is a constant: nothing is built (the smoke asserts frames,
# not builds), and both runs must evaluate identical terms on any host.
let
  src = ./src-dir;

  mkDrv =
    name: extra:
    derivation (
      {
        inherit name src;
        system = "x86_64-linux";
        builder = "/bin/sh";
        args = [
          "-c"
          "cat $src/data.txt > $out"
        ];
      }
      // extra
    );

  leaf = mkDrv "smoke-leaf" { };
in
{
  # Two-node graph: hello depends on leaf (inputDrvs edge → the
  # skeleton's input_drv_digests).
  hello = mkDrv "smoke-hello" { dep = leaf; };

  # Shares the leaf with hello — exercises cross-attr overlap (the
  # coordinator dedups; per-worker frames may resend).
  world = mkDrv "smoke-world" { dep = leaf; };

  # Eval burns CPU for a few seconds before producing the drv — the
  # crash-injection window (the harness SIGKILLs the fork worker
  # mid-eval; the parent must re-queue and complete).
  slow =
    let
      n = builtins.foldl' (a: b: a + b) 0 (builtins.genList (i: i) 20000000);
    in
    mkDrv "smoke-slow-${toString n}" { };
}
