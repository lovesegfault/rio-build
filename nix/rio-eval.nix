# rio-eval — the ADR-024 P3b eval parent.
#
# Compiles rio-eval.cc + the SAME C++ store shim the rio:// plugin uses
# (shim.cc) into one binary, linked against the FLAKE-PINNED nix 2.34
# components (`inputs.nix` — never the ambient nix) and the
# crate2nix-built rio-evalstore staticlib. Unlike the plugin (.so with
# undefined nix symbols), this is a real binary: libexpr/libstore/
# libflake/libcmd resolve at link time from the pinned derivations.
#
# Also defines the `rio-eval-smoke` check: the eval-harness (the
# coordinator-side channel driver from rio-build-cli) runs the real
# binary in the build sandbox — parent boots, locks the fixture,
# forks workers, evaluates through real libexpr, streams ResultFrames
# over a real socketpair — and asserts drvPath parity against the
# stock pinned nix-cli, worker recycling (N=1 → fresh fork per attr,
# identical results), and crash injection (kill -9 a worker mid-eval →
# the parent re-queues and completes).
{
  pkgs,
  lib,
  inputs,
  system,
  # crate2nix buildRustCrate derivation for rio-evalstore (staticlib in
  # its `lib` output) — the same one the plugin links.
  evalstoreCrate,
  # Scrubbed bin/ dir of rio-build-cli (provides eval-harness).
  buildCliBins,
}:
let
  nixPkgs = inputs.nix.packages.${system};
  nixCli = nixPkgs.nix-cli;
  nixComponents = [
    nixPkgs.nix-util
    nixPkgs.nix-store
    nixPkgs.nix-fetchers
    nixPkgs.nix-expr
    nixPkgs.nix-flake
    nixPkgs.nix-main
    nixPkgs.nix-cmd
  ];
  pcNames = "nix-util nix-store nix-fetchers nix-expr nix-flake nix-main nix-cmd";

  rioEval = pkgs.stdenv.mkDerivation {
    pname = "rio-eval";
    version = "0.1.0";
    src = lib.fileset.toSource {
      root = ../.;
      fileset = lib.fileset.unions [
        ../rio-evalstore/shim
        ../rio-eval
      ];
    };

    nativeBuildInputs = [ pkgs.pkg-config ];
    buildInputs = map lib.getDev nixComponents ++ [
      pkgs.boost
      pkgs.nlohmann_json
      # Native deps of the Rust staticlib that rustc does NOT bundle
      # (dynamically-linked -sys crates: zstd via rio-packstore, sqlite
      # via the workspace-hack feature union).
      pkgs.zstd
      pkgs.sqlite
    ];

    buildPhase = ''
      runHook preBuild
      staticlib=$(find ${evalstoreCrate.lib}/lib -name 'librio_evalstore-*.a' -print -quit)
      if [ -z "$staticlib" ]; then
        echo "librio_evalstore-*.a not found in ${evalstoreCrate.lib}/lib" >&2
        exit 1
      fi
      # NOT `-o rio-eval`: that name is the source directory.
      $CXX -std=c++23 -O2 -o rio-eval-bin \
        rio-eval/rio-eval.cc \
        rio-evalstore/shim/shim.cc \
        -Irio-evalstore/shim \
        $(pkg-config --cflags ${pcNames}) \
        "$staticlib" \
        $(pkg-config --libs ${pcNames}) \
        -lzstd -lsqlite3 \
        -pthread -ldl -lm
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      mkdir -p $out/bin
      cp rio-eval-bin $out/bin/rio-eval
      runHook postInstall
    '';
  };

  fixture = ./fixtures/rio-eval-smoke;

  smoke =
    pkgs.runCommand "rio-eval-smoke"
      {
        nativeBuildInputs = [
          nixCli
          pkgs.jq
          buildCliBins
        ];
      }
      ''
        export HOME=$TMPDIR
        export NIX_CONF_DIR=$TMPDIR/conf
        mkdir -p $NIX_CONF_DIR
        flags="--extra-experimental-features nix-command --option substitute false"

        # Copy the fixture OUT of the store (a source dir already at a
        # store path would bypass addToStore) and backdate it past the
        # racy-fingerprint slack.
        mkdir -p $TMPDIR/work
        cp -r ${fixture}/. $TMPDIR/work/
        chmod -R u+w $TMPDIR/work
        find $TMPDIR/work -exec touch -h -d '1 hour ago' {} +

        echo "== stock drvPaths (pinned nix-cli, local file store)"
        nix $flags --store "local?root=$TMPDIR/stock" \
          eval --file $TMPDIR/work/fixture.nix --json \
          --apply 'f: { hello = f.hello.drvPath; world = f.world.drvPath; }' \
          > stock.json
        jq . stock.json

        echo "== run 1: real eval parent — boot, fork, evaluate, stream frames"
        eval-harness \
          --eval-parent ${rioEval}/bin/rio-eval \
          --cas $TMPDIR/cas \
          --file $TMPDIR/work/fixture.nix \
          --attrs hello,world \
          > run1.json
        jq . run1.json
        test "$(jq '.results | length' run1.json)" = 2
        # drvPath parity: the eval parent's reported roots are byte-
        # identical to stock nix's drvPaths.
        for attr in hello world; do
          stock=$(jq -r ".$attr" stock.json)
          got=$(jq -r ".results[] | select(.attr == \"$attr\") | .root_drv_path" run1.json)
          if [ "$stock" != "$got" ]; then
            echo "drvPath parity FAILED for $attr: stock=$stock rio-eval=$got" >&2
            exit 1
          fi
        done
        # The local source tree rode the frames as a SourceRoot.
        test "$(jq '[.results[].source_roots] | add' run1.json)" -ge 1

        echo "== run 2: recycling — every attr in a fresh worker, results identical"
        eval-harness \
          --eval-parent ${rioEval}/bin/rio-eval \
          --cas $TMPDIR/cas2 \
          --file $TMPDIR/work/fixture.nix \
          --attrs hello,world \
          --workers 1 --recycle-attrs 1 \
          > run2.json
        jq . run2.json
        test "$(jq '.recycles' run2.json)" -ge 1
        for attr in hello world; do
          d1=$(jq -r ".results[] | select(.attr == \"$attr\") | .root_digest_hex" run1.json)
          d2=$(jq -r ".results[] | select(.attr == \"$attr\") | .root_digest_hex" run2.json)
          if [ "$d1" != "$d2" ]; then
            echo "recycle determinism FAILED for $attr: $d1 vs $d2" >&2
            exit 1
          fi
        done

        echo "== run 3: crash injection — kill -9 a worker mid-eval, parent completes"
        eval-harness \
          --eval-parent ${rioEval}/bin/rio-eval \
          --cas $TMPDIR/cas3 \
          --file $TMPDIR/work/fixture.nix \
          --attrs slow \
          --workers 1 \
          --kill-worker-after-ms 700 \
          > run3.json
        jq . run3.json
        test "$(jq '.results | length' run3.json)" = 1
        test "$(jq '.faults | length' run3.json)" -ge 1

        cp run1.json run2.json run3.json stock.json $TMPDIR/work/ 2>/dev/null || true
        mkdir -p $out
        cp run1.json run2.json run3.json stock.json $out/
      '';
in
{
  inherit rioEval smoke;
}
