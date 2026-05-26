# Nightly-tier differential corpus: the real-stdenv probe.
#
# A genuine `stdenv.mkDerivation` (setup.sh, phases, cc-wrapper,
# fixupPhase) — the one corpus entry that exercises the full stdenv
# environment through the native executor instead of an inline busybox
# script. Far too heavy for the merge gate; runs in the nightly tier:
#
#   nix build .#vm-differential-nightly
#
# Evaluated twice from this same file so the two sides can never drift:
#
#   * host side (scenarios/differential.nix, nightly branch): only to
#     reach `.inputDerivation`, whose closure puts the probe's
#     build-time dependencies (cc-wrapper, binutils, glibc, coreutils,
#     …) into the VM store via `additionalPaths`;
#   * in the VM: `nix-instantiate --impure --arg pkgsPath
#     'builtins.storePath "<nixpkgs>"' -A stdenv-probe <this file>`,
#     after which both the oracle and the native driver build the
#     resulting derivation.
#
# The import below is deliberately pristine (empty config, no overlays)
# so the host- and VM-side evaluations produce the identical derivation
# graph and the dependencies shipped from the host are exactly the ones
# the in-VM instantiation references.
{
  pkgsPath,
  # In-VM instantiation runs impurely and takes the default; the host
  # side evaluates purely and must pass the system explicitly.
  system ? builtins.currentSystem,
}:
let
  pkgs = import pkgsPath {
    inherit system;
    config = { };
    overlays = [ ];
  };
in
{
  stdenv-probe = pkgs.stdenv.mkDerivation {
    name = "rio-diff-stdenv";
    dontUnpack = true;
    buildPhase = ''
      runHook preBuild
      cat > hello.c <<'EOF'
      #include <stdio.h>
      int main(void) { puts("hello from a real stdenv build"); return 0; }
      EOF
      "$CC" -o hello hello.c
      runHook postBuild
    '';
    installPhase = ''
      runHook preInstall
      mkdir -p $out/bin
      install -m755 hello $out/bin/hello
      runHook postInstall
    '';
  };
}
