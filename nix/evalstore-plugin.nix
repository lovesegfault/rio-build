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
  # Header-only: the shim type-gates on FilteringSourceAccessor (the
  # local-git workdir accessor base) and the typeinfo symbol resolves
  # at dlopen against the host binary's libnixfetchers, same as the
  # nix-store/util symbols below.
  nixFetchersDev = lib.getDev nixPkgs.nix-fetchers;

  plugin = pkgs.stdenv.mkDerivation {
    pname = "rio-evalstore-plugin";
    version = "0.1.0";
    src = ../rio-evalstore/shim;

    nativeBuildInputs = [ pkgs.pkg-config ];
    buildInputs = [
      nixStoreDev
      nixUtilDev
      nixFetchersDev
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
        $(pkg-config --cflags nix-store nix-util nix-fetchers) \
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
        # The rust core created the CAS v2 layout (pack store) on open.
        test -d $TMPDIR/cas/packs
        test -f $TMPDIR/cas/gc.lock
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
          pkgs.git
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

        echo "== derivations are memory-only: no drv bytes in the client CAS"
        # The CAS v2 layout is append-pack segments; an ATerm leak would
        # appear verbatim ('Derive(' prefix) inside a pack record.
        if grep -rqa "Derive(" $TMPDIR/cas; then
          echo "drv bytes leaked into the client CAS — drvs must be memory-only"
          exit 1
        fi
        test -d $TMPDIR/cas/packs
        test -f $TMPDIR/cas/index.bin

        echo "== not-a-mirror: local source-tree bytes never enter the CAS"
        # Real nix ingested ./src-dir through addToStore(SourcePath); the
        # two-plane route stores chunk metadata + directory blobs only.
        # The canary line in data.txt would appear verbatim in a pack
        # record if any FETCHED copy of the tree were made.
        grep -q "RIO-NOT-A-MIRROR-CANARY" $TMPDIR/work/src-dir/data.txt
        if grep -rqa "RIO-NOT-A-MIRROR-CANARY" $TMPDIR/cas; then
          echo "local tree content leaked into the client CAS — not-a-mirror violated"
          exit 1
        fi

        echo "== readback: real nix serves source + toFile from the pack store"
        src=$(jq -r .source rio.json)
        nix $flags --plugin-files ${pluginSo} \
          store cat --store "rio://?cas=$TMPDIR/cas" "$src/data.txt" > data.txt
        diff data.txt $TMPDIR/work/src-dir/data.txt
        tofile=$(jq -r .toFile rio.json)
        nix $flags --plugin-files ${pluginSo} \
          store cat --store "rio://?cas=$TMPDIR/cas" "$tofile" > builder.txt
        grep -q "echo hello" builder.txt

        echo "== warm re-eval: CAS dedup means zero new pack records"
        RIO_EVALSTORE_STATS=1 nix $flags --plugin-files ${pluginSo} \
          --store "local?root=$TMPDIR/main" \
          eval --eval-store "rio://?cas=$TMPDIR/cas" \
          --file $TMPDIR/work/fixture.nix paths --json > rio2.json 2> stats.txt
        diff rio.json rio2.json
        cat stats.txt
        grep -q "rio-evalstore op stats" stats.txt || { echo "stats dump missing"; exit 1; }
        if grep -Eq "dirblob_write|fetched_write|meta_write|chunkmeta_write" stats.txt; then
          echo "warm re-eval wrote new pack records — CAS dedup regressed"
          exit 1
        fi
        # Route proof: the local dir took the two-plane ingest, not the
        # NAR-dump fallback (which would show as an add_from_dump of the
        # tree and FETCHED content writes on the cold run).
        grep -q "add_source_tree" stats.txt \
          || { echo "local source dir did not take the two-plane ingest route"; exit 1; }

        echo "== filtered accessor: a local-git flake honours the tracked-files view"
        # nix's git workdir accessor is an AllowListSourceAccessor
        # (FilteringSourceAccessor) over posix; getPhysicalPath() on it
        # returns the worktree root even though gitignored entries are
        # hidden. The two-plane fast path used to walk the raw worktree —
        # ingesting every gitignored file and producing a different NAR
        # hash than stock nix.
        export GIT_CONFIG_GLOBAL=$TMPDIR/gitconfig
        git config --global user.email nobody@example.com
        git config --global user.name nobody
        git config --global init.defaultBranch main
        mkdir -p $TMPDIR/gitrepo/target
        echo target/ > $TMPDIR/gitrepo/.gitignore
        echo RIO-GIT-TRACKED-CONTENT > $TMPDIR/gitrepo/tracked.txt
        cat > $TMPDIR/gitrepo/flake.nix <<'EOF'
        { outputs = { self }: { source = "''${self}"; }; }
        EOF
        # Gitignored sentinels: never tracked. A raw-fs walk (the pre-fix
        # route) records the entry name in a dirblob — the grep below.
        head -c 65536 /dev/zero | tr '\0' R > $TMPDIR/gitrepo/target/sentinel.bin
        echo RIO-GITIGNORE-SENTINEL > $TMPDIR/gitrepo/target/marker
        git -C $TMPDIR/gitrepo init -q
        git -C $TMPDIR/gitrepo add flake.nix tracked.txt .gitignore
        git -C $TMPDIR/gitrepo commit -q -m init
        # Dirty the worktree so the warning trail is unmistakably the
        # workdir-accessor route (a clean repo also takes it: no ref/rev
        # → getAccessorFromWorkdir).
        echo dirty >> $TMPDIR/gitrepo/tracked.txt
        gflags="$flags --extra-experimental-features flakes --option warn-dirty false"

        nix $gflags --store "local?root=$TMPDIR/stock-git" \
          eval "$TMPDIR/gitrepo#source" --raw > stock-git.txt
        nix $gflags --plugin-files ${pluginSo} \
          --store "local?root=$TMPDIR/main-git" \
          eval --eval-store "rio://?cas=$TMPDIR/cas-git" \
          "$TMPDIR/gitrepo#source" --raw > rio-git.txt

        # Same NAR hash → same store path: the rio store saw exactly the
        # tracked-files view stock nix did. A raw-fs walk would include
        # target/ and the paths would diverge.
        diff stock-git.txt rio-git.txt
        # The dirblob entry name proves the route: a raw walk records the
        # ignored entries (not their content — not-a-mirror) in the
        # per-directory blob.
        if grep -rqa "sentinel.bin" $TMPDIR/cas-git; then
          echo "gitignored entry leaked into the CAS — accessor not honoured"
          exit 1
        fi
        if grep -rqa "RIO-GITIGNORE-SENTINEL" $TMPDIR/cas-git; then
          echo "gitignored content leaked into the CAS — accessor not honoured"
          exit 1
        fi
        # The tracked file IS what nix added — it must read back.
        nix $gflags --plugin-files ${pluginSo} \
          store cat --store "rio://?cas=$TMPDIR/cas-git" \
          "$(cat rio-git.txt)/tracked.txt" | grep -q RIO-GIT-TRACKED-CONTENT
        echo "== substitution: rio:// accepts a CA path from a binary cache"
        # builtins.storePath → ensurePath → PathSubstitutionGoal on the
        # destination store, the same path flake-input fetchToStore takes.
        # The base Store::pathInfoIsUntrusted is unconditionally true; if
        # RioStore lacks the override, the goal warns "not signed by any
        # of the keys in 'trusted-public-keys'" and FAILS even for this
        # CA path (which needs no signature). $src is the CA source path
        # already valid in the run-A stock store; serve it from a file://
        # cache and a fresh rio:// CAS must substitute it. (`substitute
        # true` overridden explicitly: in the build sandbox nix detects
        # no-internet and would otherwise force it false.)
        nix $flags copy --no-check-sigs \
          --from "local?root=$TMPDIR/stock" --to "file://$TMPDIR/cache" "$src" 2>&1
        test -f $TMPDIR/cache/nix-cache-info
        subFlags="--extra-experimental-features nix-command --plugin-files ${pluginSo}"
        nix $subFlags --store "rio://?cas=$TMPDIR/cas-sub" \
          --option substitute true --substituters "file://$TMPDIR/cache" \
          eval --impure --raw --expr "builtins.storePath \"$src\"" \
          > sub-out.txt 2> sub-err.txt \
          || { cat sub-err.txt; exit 1; }
        cat sub-err.txt
        if grep -q "not signed by any of the keys" sub-err.txt; then
          echo "rio:// rejected a CA substitute — pathInfoIsUntrusted not overridden"
          exit 1
        fi
        test "$(cat sub-out.txt)" = "$src"
        # The path actually landed (readback through the rio store).
        nix $subFlags store cat --store "rio://?cas=$TMPDIR/cas-sub" \
          "$src/data.txt" > sub-data.txt
        diff sub-data.txt $TMPDIR/work/src-dir/data.txt

        cp stats.txt $out
      '';
in
{
  inherit plugin smoke parity;
}
