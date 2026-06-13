# VM test wiring — mkVmTests.
#
# mkVmTests builds the `vm-*` attrset for a given (workspace,
# dockerImages, coverage) triple. flake.nix calls it twice: once for
# the normal build (`vmTests`) and once for the
# coverage-instrumented build (`vmTestsCov`, see common.nix's
# LLVM_PROFILE_FILE wiring).
#
# Extracted from flake.nix's perSystem `let` block. Lives next to
# nix/tests/default.nix (the `import ./.` callee).
{
  pkgs,
  system,
  inputs,
  # The rio-eval eval-parent binary (nix/rio-eval.nix). Same
  # uninstrumented C++ binary in both normal and coverage mode — the
  # instrumented half of the client pair is the coordinator (bin/rio,
  # a workspace member, so rio-workspace-cov covers it).
  rioEval,
}:
{
  mkVmTests =
    {
      rio-workspace,
      dockerImages,
      coverage,
    }:
    import ./. {
      inherit
        pkgs
        rio-workspace
        dockerImages
        system
        coverage
        rioEval
        ;
      rioModules = inputs.self.nixosModules;
      inherit (inputs) nixhelm;
      # Lix-client VM test (vm-protocol-warm-lix-standalone).
      # nixpkgs-packaged (substitutable from cache.nixos.org)
      # rather than building lix from source via a flake input.
      lixPackage = pkgs.lix;
    };
}
