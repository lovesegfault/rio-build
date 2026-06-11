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

  fixture = ./fixtures/evalstore-parity;

  # Acceptance check (defines M0 done): the same local fixture evaluated
  # with the pinned nix-cli both stock (local file store) and through the
  # plugin must produce byte-identical drvPaths. Any rio-nix/nix path
  # divergence aborts the plugin eval (hard cross-check error), so a
  # passing run also proves zero cross-check failures.
  parity =
    pkgs.runCommand "evalstore-parity"
      {
        nativeBuildInputs = [
          nixCli
          pkgs.jq
        ];
      }
      ''
        export HOME=$TMPDIR
        export NIX_CONF_DIR=$TMPDIR/conf
        mkdir -p $NIX_CONF_DIR
        flags="--extra-experimental-features nix-command --option substitute false"

        # Copy the fixture OUT of the store: a source dir already at a
        # store path would skip copyPathToStore and bypass addToStore.
        mkdir -p $TMPDIR/work
        cp -r ${fixture}/. $TMPDIR/work/
        chmod -R u+w $TMPDIR/work

        echo "== run A: stock nix, local file store"
        nix $flags --store "local?root=$TMPDIR/stock" \
          eval --file $TMPDIR/work/fixture.nix paths --json > stock.json
        jq . stock.json

        echo "== run B: plugin eval store"
        # --plugin-files is a global flag and must precede the subcommand;
        # --eval-store belongs to the eval command itself.
        nix $flags --plugin-files ${pluginSo} \
          --store "local?root=$TMPDIR/main" \
          eval --eval-store "rio://?cas=$TMPDIR/cas" \
          --file $TMPDIR/work/fixture.nix paths --json > rio.json
        jq . rio.json

        echo "== drvPath parity"
        diff stock.json rio.json

        echo "== CAS contains the derivations (index + canonical drv JSON blob)"
        for attr in plain structured; do
          drv=$(jq -r ".$attr" rio.json)
          base=''${drv#/nix/store/}
          test -f "$TMPDIR/cas/index/$base.json" || { echo "missing index entry $base"; exit 1; }
          blob=$(jq -r .drv_json_blob "$TMPDIR/cas/index/$base.json")
          test "$blob" != "null" || { echo "no drv json blob for $base"; exit 1; }
          test -f "$TMPDIR/cas/blobs/''${blob:0:2}/$blob" || { echo "missing drv json blob $blob"; exit 1; }
          # The blob is nix's derivation JSON for this drv.
          jq -e .outputs "$TMPDIR/cas/blobs/''${blob:0:2}/$blob" > /dev/null
        done

        echo "== CAS contains the copied source dir + toFile text path"
        for attr in source toFile; do
          p=$(jq -r ".$attr" rio.json)
          base=''${p#/nix/store/}
          test -f "$TMPDIR/cas/index/$base.json" || { echo "missing index entry $base"; exit 1; }
          test -f "$TMPDIR/cas/paths/$base.json" || { echo "missing DAG for $base"; exit 1; }
        done

        echo "== warm re-eval: CAS dedup means zero new blob writes"
        RIO_EVALSTORE_STATS=1 nix $flags --plugin-files ${pluginSo} \
          --store "local?root=$TMPDIR/main" \
          eval --eval-store "rio://?cas=$TMPDIR/cas" \
          --file $TMPDIR/work/fixture.nix paths --json > rio2.json 2> stats.txt
        diff rio.json rio2.json
        cat stats.txt
        grep -q "rio-evalstore op stats" stats.txt || { echo "stats dump missing"; exit 1; }
        if grep -q "blob_write" stats.txt; then
          echo "warm re-eval wrote new blobs — CAS dedup regressed"
          exit 1
        fi

        cp stats.txt $out
      '';
in
{
  inherit plugin smoke parity;
}
