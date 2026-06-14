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

  # Attrset installable (`rio build .#checks` shape): the worker expands
  # it into one WorkItem per derivation child instead of failing. Keyed
  # by the eval system so the smoke exercises the system-descent step;
  # `grouped` takes the recurseForDerivations branch, `plain` and the
  # all-digit name are skipped with a warning. Only the KEY uses
  # currentSystem — the drvs keep the constant system, and stock nix and
  # rio-eval evaluate in the same sandbox, so drvPath parity still holds.
  checks = {
    ${builtins.currentSystem} = {
      alpha = mkDrv "smoke-check-alpha" { };
      beta = mkDrv "smoke-check-beta" { dep = leaf; };
      grouped = {
        recurseForDerivations = true;
        gamma = mkDrv "smoke-check-gamma" { };
      };
      plain = {
        delta = mkDrv "smoke-check-delta" { };
      };
      # All-digit name: findAlongAttrPath would parse the resulting attr
      # path component as a list index, so the expansion must skip it
      # rather than report a child that can never re-resolve.
      "404" = mkDrv "smoke-check-numeric" { };
    };
  };

  # Attrset with no derivation children anywhere — expansion must fail
  # the attr (hard eval error), not silently produce nothing.
  emptyset = {
    docs = {
      readme = "not a derivation";
    };
  };

  # Eval burns CPU for a few seconds before producing the drv — the
  # crash-injection window (the harness SIGKILLs the fork worker
  # mid-eval; the parent must re-queue and complete).
  slow =
    let
      n = builtins.foldl' (a: b: a + b) 0 (builtins.genList (i: i) 20000000);
    in
    mkDrv "smoke-slow-${toString n}" { };
}
