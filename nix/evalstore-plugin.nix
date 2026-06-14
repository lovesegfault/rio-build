# rio:// eval-store plugin (ADR-024 M0).
#
# Builds the C++ shim against the FLAKE-PINNED nix (`inputs.nix`) headers
# and links the crate2nix-built rio-evalstore staticlib into a single
# plugin .so. Pin contract: the .so resolves its nix symbols at dlopen
# against the loading binary's libnixstore, so it must only ever be
# loaded into binaries built from the same `inputs.nix` derivation set —
# never into the ambient dev-shell nix (a host shim) or any other nix
# build. All checks here therefore invoke `inputs.nix`'s nix-cli.
#
# Deliberately NOT linked with -lnixstore: undefined symbols are left
# for the dynamic loader, which is how nix plugins are expected to bind
# (the host binary's already-loaded libnixstore satisfies them).
{
  pkgs,
  lib,
  inputs,
  system,
  # crate2nix buildRustCrate derivation for rio-evalstore; the staticlib
  # lands in its `lib` output with a metadata-suffixed name.
  evalstoreCrate,
}:
let
  nixPkgs = inputs.nix.packages.${system};
  nixCli = nixPkgs.nix-cli;
  nixStoreDev = lib.getDev nixPkgs.nix-store;
  nixUtilDev = lib.getDev nixPkgs.nix-util;

  plugin = pkgs.stdenv.mkDerivation {
    pname = "rio-evalstore-plugin";
    version = "0.1.0";
    src = ../rio-evalstore/shim;

    nativeBuildInputs = [ pkgs.pkg-config ];
    buildInputs = [
      nixStoreDev
      nixUtilDev
      # boost is used by nix headers but absent from the .pc files.
      pkgs.boost
      pkgs.nlohmann_json
    ];

    buildPhase = ''
      runHook preBuild
      # buildRustCrate names the staticlib with a metadata hash suffix —
      # find it explicitly instead of trusting a fixed name.
      staticlib=$(find ${evalstoreCrate.lib}/lib -name 'librio_evalstore-*.a' -print -quit)
      if [ -z "$staticlib" ]; then
        echo "librio_evalstore-*.a not found in ${evalstoreCrate.lib}/lib" >&2
        ls -l ${evalstoreCrate.lib}/lib >&2
        exit 1
      fi
      $CXX -std=c++23 -fPIC -shared -o librio-evalstore.so \
        shim.cc \
        $(pkg-config --cflags nix-store nix-util) \
        "$staticlib" \
        -pthread -ldl -lm
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      mkdir -p $out/lib
      cp librio-evalstore.so $out/lib/
      runHook postInstall
    '';
  };

  pluginSo = "${plugin}/lib/librio-evalstore.so";

  # Plugin loads + scheme registers + store opens. If registration broke,
  # `store info` fails with "don't know how to open Nix store".
  smoke =
    pkgs.runCommand "evalstore-plugin-smoke"
      {
        nativeBuildInputs = [ nixCli ];
      }
      ''
        export HOME=$TMPDIR
        export NIX_CONF_DIR=$TMPDIR/conf
        mkdir -p $NIX_CONF_DIR
        nix --extra-experimental-features nix-command \
          --plugin-files ${pluginSo} \
          store info --store "rio://?cas=$TMPDIR/cas" | tee info.txt
        # The rust core created the CAS layout on open.
        test -d $TMPDIR/cas/index
        test -d $TMPDIR/cas/blobs
        mv info.txt $out
      '';

in
{
  inherit plugin smoke;
}
