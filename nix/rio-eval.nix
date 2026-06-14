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
# identical results), crash injection (kill -9 a worker mid-eval →
# the parent re-queues and completes), and attrset-installable
# expansion (a checks-style attr fans out into per-child roots).
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
  flakeFixture = ./fixtures/rio-eval-smoke-flake;

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
          --apply 'f: let sys = builtins.currentSystem; in {
            hello = f.hello.drvPath;
            world = f.world.drvPath;
            "checks.''${sys}.alpha" = f.checks.''${sys}.alpha.drvPath;
            "checks.''${sys}.beta" = f.checks.''${sys}.beta.drvPath;
            "checks.''${sys}.grouped.gamma" = f.checks.''${sys}.grouped.gamma.drvPath;
          }' \
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

        echo "== run 4: attrset expansion — a checks-style attr fans out per child"
        sys=$(nix $flags eval --impure --raw --expr builtins.currentSystem)
        eval-harness \
          --eval-parent ${rioEval}/bin/rio-eval \
          --cas $TMPDIR/cas4 \
          --file $TMPDIR/work/fixture.nix \
          --attrs checks,emptyset \
          > run4.json
        jq . run4.json
        # Three derivation children become roots, named by full attr path,
        # with drvPath parity against stock nix (system descent + the
        # recurseForDerivations descent included).
        test "$(jq '.results | length' run4.json)" = 3
        for child in alpha beta grouped.gamma; do
          attr="checks.$sys.$child"
          stock=$(jq -r --arg a "$attr" '.[$a]' stock.json)
          got=$(jq -r --arg a "$attr" '.results[] | select(.attr == $a) | .root_drv_path' run4.json)
          if [ -z "$got" ] || [ "$stock" != "$got" ]; then
            echo "expansion drvPath parity FAILED for $attr: stock=$stock rio-eval=$got" >&2
            exit 1
          fi
        done
        # The non-recursable subset and the all-digit name are skipped
        # with a warning, never an error.
        test "$(jq '.skipped | length' run4.json)" = 2
        jq -e --arg a "checks.$sys.plain" '.skipped | index($a) != null' run4.json
        jq -e --arg a "checks.$sys.404" '.skipped | index($a) != null' run4.json
        # An attrset with zero derivation children is a hard eval error.
        test "$(jq '.eval_errors | length' run4.json)" = 1
        jq -e '.eval_errors[0][0] == "emptyset"' run4.json
        jq -e '.eval_errors[0][1] | test("zero derivations")' run4.json

        echo "== run 5: flake mode — parseFlakeRef → lockFlake → callFlake → eval"
        # lockFlake checks Xp::Flakes against the loaded nix.conf;
        # the smoke conf dir was empty so far.
        echo 'experimental-features = nix-command flakes' > $NIX_CONF_DIR/nix.conf
        # Copy the hermetic flake fixture out of the store (a path
        # already at a store path would short-circuit `self` ingest)
        # and backdate past the racy-fingerprint slack.
        mkdir -p $TMPDIR/work-flake
        cp -r ${flakeFixture}/. $TMPDIR/work-flake/
        chmod -R u+w $TMPDIR/work-flake
        find $TMPDIR/work-flake -exec touch -h -d '1 hour ago' {} +
        # Stock parity: pinned nix-cli evaluates the same path flake.
        nix $flags --store "local?root=$TMPDIR/stock-flake" \
          --extra-experimental-features flakes \
          eval --no-write-lock-file --raw \
          "path:$TMPDIR/work-flake#packages.x86_64-linux.hello.drvPath" \
          > stock-flake.txt
        eval-harness \
          --eval-parent ${rioEval}/bin/rio-eval \
          --cas $TMPDIR/cas5 \
          --flake "path:$TMPDIR/work-flake" \
          --attrs packages.x86_64-linux.hello,packages.x86_64-linux.world \
          > run5.json
        jq . run5.json
        test "$(jq '.results | length' run5.json)" = 2
        stock=$(cat stock-flake.txt)
        got=$(jq -r '.results[] | select(.attr == "packages.x86_64-linux.hello") | .root_drv_path' run5.json)
        if [ "$stock" != "$got" ]; then
          echo "flake drvPath parity FAILED: stock=$stock rio-eval=$got" >&2
          exit 1
        fi
        # leaf+hello+world graph assembled (proves callFlake → forceAttrs
        # → eval reached the derivations).
        test "$(jq '.total_nodes' run5.json)" -ge 3
        # The end-to-end build (self uploaded → worker reads it) is the
        # vm-build-client flake leg's job; this smoke leg only checks the
        # eval-parent contract.

        cp run1.json run2.json run3.json run4.json run5.json stock.json $TMPDIR/work/ 2>/dev/null || true
        mkdir -p $out
        cp run1.json run2.json run3.json run4.json run5.json stock.json $out/
      '';
in
{
  inherit rioEval smoke;
}
