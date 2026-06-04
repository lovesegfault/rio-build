# Typst design-book build pipeline.
#
# Produces a hermetic typst environment (rioTypst) with the @preview/*
# packages the book uses, plus two output forms:
#   - docs-pdf  : single-file PDF via `typst compile` (checks.docs-pdf)
#   - docs      : static HTML site via shiroa (checks.docs-html)
#
# nixpkgs' `typst.withPackages` / `typst.wrapper` only collect
# `propagatedBuildInputs` ONE level deep (see wrapper.nix's foldl').
# fletcher → cetz → oxifmt etc. fail in-sandbox unless we close the
# transitive set ourselves — hence `closeDeps`.
#
# `@preview/tracey` is not on the typst universe (it's the rio-side
# requirement-marker shim from the tracey fork pinned as a flake input),
# so we buildTypstPackage it from the input source.
{
  pkgs,
  lib,
  tracey-src,
  shiroaPkg,
  self,
  xtaskBin,
}:
let
  # tracey's typst-side `req` shim. Version must match the package's
  # typst.toml (0.1.0) so `@preview/tracey:0.1.0` resolves.
  traceyTypstPkg = pkgs.buildTypstPackage {
    pname = "tracey";
    version = "0.1.0";
    src = "${tracey-src}/typst-package/tracey";
  };

  # @preview/shiroa-mdbook from the same fork as shiroaPkg (rio-pin
  # branch), so the items.sum(default: []) sidebar fix (PR #239) is in
  # the typst package too — nixpkgs' typstPackages.shiroa-mdbook is the
  # unpatched 0.3.1 universe release. Reuse the nixpkgs package's
  # propagatedBuildInputs so closeDeps still resolves transitives
  # (shiroa-mdbook imports shiroa core).
  shiroaMdbookTypstPkg = pkgs.buildTypstPackage {
    pname = "shiroa-mdbook";
    version = "0.3.1";
    src = "${shiroaPkg.src}/themes/mdbook";
    inherit (pkgs.typstPackages.shiroa-mdbook) propagatedBuildInputs;
  };

  # Transitive closure over propagatedBuildInputs. wrapper.nix only
  # walks one hop; this walks to fixpoint.
  closeDeps =
    ps: lib.unique (lib.concatMap (p: [ p ] ++ closeDeps (p.propagatedBuildInputs or [ ])) ps);

  # The package set the book imports, plus their transitive closure.
  # tracey + shiroa-mdbook live outside the `typstPackages` scope
  # (built from fork sources), so they're spliced in after the
  # `with p;` list.
  typstDeps =
    p:
    closeDeps (
      (with p; [
        glossarium
        codly
        codly-languages
        lovelace
        unify
        gentle-clues
        showybox
        lilaq
        fletcher
        finite
        autograph
        pinit
        suiji
        chronos
        shiroa
      ])
      ++ [
        traceyTypstPkg
        shiroaMdbookTypstPkg
      ]
    );

  fonts = [
    pkgs.libertinus
    pkgs.dejavu_fonts
  ];

  # Wrapped typst binary: TYPST_PACKAGE_CACHE_PATH + TYPST_FONT_PATHS
  # baked into $out/bin/typst via makeWrapper. `packages` is a
  # function (typstPackages scope → list); `fonts` is a list of paths
  # joined with ':' (typst scans them recursively).
  rioTypst = pkgs.typst.wrapper {
    packages = typstDeps;
    inherit fonts;
  };

  # shiroa embeds typst as a library (reflexo-typst) — it does NOT
  # exec the wrapped binary. Expose the same env it would have seen so
  # its in-process resolver finds packages + fonts hermetically.
  typstEnv = {
    TYPST_PACKAGE_CACHE_PATH = "${rioTypst}/lib/typst/packages";
    TYPST_FONT_PATHS = lib.concatStringsSep ":" (map toString fonts);
  };

  # Typst sources only. docs/gen/ is excluded — compileRoot overlays
  # the hermetic docsData there.
  docsSrc = lib.fileset.toSource {
    root = ../docs;
    fileset = lib.fileset.difference ../docs (
      lib.fileset.unions [
        ../docs/gen
        # gitignored local-build artifacts that may exist on disk
        (lib.fileset.maybeMissing ../docs/dist)
        (lib.fileset.maybeMissing ../docs/.cache)
        # Contributor bug-pattern catalog cited from rust comments by
        # literal path; not book content. Excluded so editing it doesn't
        # rebuild docs-pdf/shiroa.
        (lib.fileset.maybeMissing ../docs/REVIEW.md)
      ]
    );
  };

  # Generated reference data (metric/alert/error/config tables, plus
  # workspace/consts/helm-ns for the lib/refs.typ validators).
  # `xtask regen docs-data` scans rio-*/src/**/*.rs for describe_*! and
  # `(?:pub\s+)?enum *Error` plus prometheusrule.yaml for alert names. The
  # crate2nix-built xtask binary's compile-time CARGO_MANIFEST_DIR is a
  # store path, so RIO_REPO_ROOT points it at the runCommand src tree.
  #
  # Fileset is the scan surface: every workspace-member src/**/*.rs
  # (derived from `[workspace] members` so xtask and any future member
  # is picked up — modules() walks each workspace_member()/src/, and a
  # `rio-*/src/` glob would silently drop xtask from modules.json via
  # the `if !src.is_dir() { continue }` skip). `<member>/tests/` and
  # `fuzz/` workspaces are excluded: `xtask regen docs-data` never
  # reads them, so including them only forces docs rebuilds on
  # test-file edits. Plus every Cargo.toml (workspace() reads each
  # member's [package].description + [dependencies]/[dev-dependencies]/
  # [target.*] for the full crate graph), and the two helm files
  # alerts()/helm_ns() read.
  docsData =
    pkgs.runCommand "rio-docs-data"
      {
        nativeBuildInputs = [
          xtaskBin
          pkgs.jq
        ];
        src = lib.fileset.toSource {
          root = ../.;
          fileset = lib.fileset.unions (
            [
              (lib.fileset.fileFilter (f: f.name == "Cargo.toml") ../.)
              # config(): per-crate schema snapshots committed by the
              # `config_schema_frozen` snapshot tests. Glob (not 5
              # literal paths) so adding a binary crate is a one-place
              # change (test file + regen).
              (lib.fileset.fileFilter (f: f.name == "config-schema.json") ../.)
              ../infra/helm/rio-build/templates/prometheusrule.yaml
              ../infra/helm/rio-build/values.yaml # helm_ns()
              ../rio-proto/proto # protos()
              ../rio-migrations/migrations # migrations()
            ]
            ++
              map (m: lib.fileset.fileFilter (f: f.hasExt "rs") (../. + "/${m}/src"))
                (builtins.fromTOML (builtins.readFile ../Cargo.toml)).workspace.members
          );
        };
      }
      ''
        # xtask writes to $RIO_REPO_ROOT/docs/gen — needs a writable
        # tree. Copy not symlink: read_dir + create_dir_all both touch.
        cp -r --no-preserve=mode $src work
        export RIO_REPO_ROOT=$PWD/work
        xtask regen docs-data
        mv work/docs/gen $out
        test "$(jq '.names|length' $out/metrics.json)" -gt 0
        test "$(jq '.names|length' $out/alerts.json)" -gt 0
        test "$(jq '.rules|length' $out/alerts.json)" -gt 0
        test "$(jq '.stems|length' $out/migrations.json)" -gt 50
        test "$(jq '.variants|length' $out/errors.json)" -gt 0
        test "$(jq '.members|length' $out/workspace.json)" -gt 0
        test "$(jq 'keys|length' $out/consts.json)" -gt 0
        test "$(jq 'keys|length' $out/helm-ns.json)" -eq 4
        test "$(jq 'keys|length' $out/modules.json)" -gt 10
        test "$(jq '.subcommands|length' $out/cli.json)" -gt 10
        test "$(jq 'keys|length' $out/protos.json)" -ge 8
      '';

  # `--input` pairs both targets must see. Factored so the PDF and
  # shiroa-HTML invocations can't drift (bug_003: HTML omitted gh-sha,
  # so refs.gh() permalinks pointed at /blob/main/). x-target stays
  # per-invocation (shiroa sets it itself; PDF passes book-pdf).
  #
  # gh-sha is a *parameter*: `checks.*` derivations bake a stable
  # placeholder so they cache across commits that don't touch typst/rs
  # sources (`self.rev` changes every commit, which made the four docs
  # checks spin up runners on every PR). `packages.{docs,docs-pdf}`
  # bake the real SHA — they're the deployed artifacts whose refs.gh()
  # permalinks users actually click. The placeholder is SHA-shaped and
  # != "main" so the docs-html-smoke `/blob/main/` regression assert
  # still fires on the real bug class (bug_003: HTML omitted gh-sha →
  # refs.gh() defaulted to "main").
  realSha = self.rev or "dirty";
  placeholderSha = "0000000000000000000000000000000000000000";
  mkInputArgs =
    ghSha:
    lib.concatMapStringsSep " " (i: "--input ${lib.escapeShellArg i}") [
      "gh-sha=${ghSha}"
    ];

  # Compile root: docs sources + generated data, fused into one tree
  # so typst's `--root` sees `/lib`, `/spec`, and `/gen` together.
  compileRoot = pkgs.runCommand "rio-docs-root" { } ''
    mkdir -p $out
    cp -r ${docsSrc}/* $out/
    cp -r ${docsData} $out/gen
  '';

  mkDocsPdf =
    ghSha:
    pkgs.runCommand "rio-docs-pdf"
      (
        typstEnv
        // {
          nativeBuildInputs = [ rioTypst ];
        }
      )
      ''
        typst compile --root ${compileRoot} -f pdf \
          ${mkInputArgs ghSha} --input x-target=book-pdf \
          ${compileRoot}/book-pdf.typ $out
      '';

  mkDocs =
    ghSha:
    pkgs.runCommand "rio-docs-html"
      (
        typstEnv
        // {
          outputs = [
            "out"
            "bin"
          ];
          # nixpkgs' multiple-outputs.sh defaults outputsToInstall to
          # ["bin"] when a "bin" output exists, so `nix build .#docs`
          # would symlink result→bin only (no HTML tree). Override so
          # `result` is the HTML tree, `result-bin` the wrapper.
          meta.outputsToInstall = [
            "out"
            "bin"
          ];
          nativeBuildInputs = [
            shiroaPkg
            rioTypst
            pkgs.makeWrapper
          ];
        }
      )
      ''
        # shiroa embeds reflexo-typst, which resolves @preview/* via
        # XDG_DATA_HOME (ignores TYPST_PACKAGE_CACHE_PATH) and on
        # startup writes its bundled copy of @preview/shiroa there
        # — so the dir must be a writable copy, not a store symlink.
        # `cp -rL`: rioTypst's lib/ is a buildEnv symlink farm.
        export HOME=$TMPDIR
        export XDG_DATA_HOME=$HOME/.local/share
        mkdir -p $XDG_DATA_HOME/typst
        cp -rL "$TYPST_PACKAGE_CACHE_PATH" $XDG_DATA_HOME/typst/packages
        chmod -R u+w $XDG_DATA_HOME/typst
        cp -r ${compileRoot} ./root && chmod -R +w ./root
        cd ./root
        shiroa build --root . --mode static-html \
          ${mkInputArgs ghSha} \
          --font-path "$TYPST_FONT_PATHS" -d $out .
        # Hoist + dedup <symbol> glyph defs (typst content-hashes glyph
        # IDs so identical glyphs share an id but every html.frame()
        # SVG carries its own <defs> copy — sla-sizing.html had 13K
        # defs with 1070 distinct ids). Also strips dyn-paged renderer
        # script tags (shiroa.js heartbeat, svg_utils.js, wasm-init).
        ${pkgs.python3}/bin/python3 ${./docs-svg-dedup.py} $out
        # Dyn-paged renderer assets — script tags are stripped by
        # svg-dedup.py above (no .typst-doc elements in static-html);
        # the files are now unreferenced. ~1.1MB.
        rm -f $out/internal/{shiroa.js,svg_utils.js,typst_ts_renderer_bg.wasm}
        # 404.html — derive from intro.html (chrome, sidebar, theme
        # switcher, all CSS/JS) by replacing <title> + <main>. shiroa
        # has no hidden-chapter mechanism (a `#chapter("404.typ")` would
        # appear in the sidebar), and a hand-written stub can't pick up
        # the inlined data:uri CSS — deriving from a built page gets
        # full theme-awareness for free. print.html is intentionally
        # absent — `nix build .#docs-pdf` is the print equivalent
        # (shiroa-mdbook hardcodes print-enable=false anyway).
        awk '
          /<title>/ { sub(/<title>[^<]*<\/title>/,
                          "<title>Not Found – rio-build design book</title>") }
          /class="menu-title"/ { sub(/class="menu-title">[^<]*</,
                                     "class=\"menu-title\">Not Found<") }
          /<main>/ { in_main=1
                     print "            <main>"
                     print "              <h1 class=\"rio-chapter-title\">Not Found</h1>"
                     print "              <p>The page you are looking for does not exist.</p>"
                     print "              <p><a href=\"/\">← rio-build design book</a></p>"
                     next }
          /<\/main>/ && in_main { in_main=0 }
          !in_main { print }
        ' $out/intro.html > $out/404.html
        # `nix run .#docs` → serve the post-processed tree. Only --index
        # and the docs path are baked in; everything else (port,
        # interface, auth, tls) passes through, e.g., `nix run .#docs --
        # -p 9000 -i 0.0.0.0`. miniserve's default port is 8080. This
        # serves the nix-built output (4.4MB sla-sizing, deduped
        # symbols, dyn-render JS stripped, 404.html) — the dev-loop
        # `shiroa serve` sees none of those post-process steps.
        mkdir -p $bin/bin
        makeWrapper ${pkgs.miniserve}/bin/miniserve $bin/bin/rio-docs \
          --add-flags "--index index.html" \
          --add-flags "--header Cache-Control:no-cache" \
          --add-flags "$out"
      '';

  # Placeholder-SHA builds for the checks gate. Distinct attrs (not
  # `inherit docs;`) so the smoke checks below close over the cache-
  # stable derivations, not the per-commit ones.
  docsCheck = mkDocs placeholderSha;
  docsPdfCheck = mkDocsPdf placeholderSha;
in
rec {
  inherit rioTypst typstEnv docsData;

  # Real-SHA builds for `packages.{docs,docs-pdf}` — the deployed
  # artifacts whose refs.gh() permalinks readers actually click.
  docs-pdf = mkDocsPdf realSha;
  docs = mkDocs realSha;

  # Attr names follow the `<artifact>-<kind>` convention every other
  # check group uses (`clippy-`, `nextest-`, `vm-`, …) so a red
  # `check / docs-pdf` GHA entry or `nix-fast-build` failure line says
  # what was checked, not which tool ran. They line up with the
  # deployed `packages.{docs,docs-pdf}` artifact names.
  checks = {
    docs-pdf = docsPdfCheck;
    docs-html = docsCheck;
    # PDF/HTML divergence smoke. Asserts the two cross-target invariants
    # bug_003/033/025 broke: gh-sha permalinks pinned (no own-repo
    # /blob/main/) and rref() anchors live (~115 same-chapter rrefs as
    # of 2026-05 after the _rid()-strip fix; cross-chapter degrade to
    # plain text by design — shiroa static-html compiles per-chapter —
    # so this is a regression tripwire, not an exhaustive count). Runs
    # against the built `docs` output so it's free (just greps result/).
    docs-html-smoke = pkgs.runCommand "rio-docs-html-smoke" { } ''
      set -euo pipefail
      cd ${docsCheck}
      # Scope to own-repo permalinks: external upstream links like
      # github.com/aws/karpenter-provider-aws/blob/main/... are
      # legitimate and would false-positive an unscoped /blob/main/ grep.
      if grep -rq 'lovesegfault/rio-build/blob/main/' .; then
        echo "FAIL: refs.gh permalinks not pinned (bug_003 regressed)" >&2
        grep -rn 'lovesegfault/rio-build/blob/main/' . | head -5 >&2
        exit 1
      fi
      n=$(grep -rohE '<a [^>]*href="#r-' . | wc -l)
      if [[ $n -lt 80 ]]; then
        echo "FAIL: only $n rref anchors in HTML (expected ≥80; bug_033/025 regressed)" >&2
        exit 1
      fi
      # QA #1: html.frame() zero-width regression. Match the root <svg>
      # element only (nested rect/path width=0 are legitimate).
      if grep -rqE '<svg[^>]*width="0pt"' .; then
        echo "FAIL: zero-width SVG in html output (QA #1 regressed)" >&2
        grep -rlnE '<svg[^>]*width="0pt"' . | head -5 >&2
        exit 1
      fi
      # QA #3: repository/edit links populated (typst emits bare `href`
      # for missing values, not href="").
      if grep -rqE '<a [^>]*\bhref(\s|>)' .; then
        echo "FAIL: bare-href link (QA #3 — repository unset?)" >&2
        exit 1
      fi
      grep -q 'href="https://github.com/lovesegfault/rio-build"' intro.html \
        || { echo "FAIL: repository link missing (QA #3)" >&2; exit 1; }
      # QA #5: tables wrapped in scroll div.
      n=$(grep -roh 'class="rio-table"' . | wc -l)
      if [[ $n -lt 30 ]]; then
        echo "FAIL: only $n rio-table wrappers (expected ≥30; QA #5 regressed)" >&2
        exit 1
      fi
      # QA #6: distinct page titles.
      n=$(grep -rhoP '<title>[^<]+' . | sort -u | wc -l)
      if [[ $n -lt 25 ]]; then
        echo "FAIL: only $n distinct <title> values (expected ≥25; QA #6 regressed)" >&2
        exit 1
      fi
      # Equation single-render + symbol-dedup got sla-sizing.html from
      # 25M → ~4.4M. Tripwire so a future eq-heavy chapter (or a
      # docs-svg-dedup.py regression) doesn't silently re-bloat.
      sz=$(stat -c%s spec/components/sla-sizing.html)
      if [[ $sz -gt 7340032 ]]; then
        echo "FAIL: sla-sizing.html is $sz bytes (>7MB; equation/dedup regression?)" >&2
        exit 1
      fi
      # QA2-h1: <h1> emitted from manifest title.
      grep -q 'class="rio-chapter-title"' intro.html \
        || { echo "FAIL: rio-chapter-title h1 missing (QA2-h1)" >&2; exit 1; }
      # QA2-R3: nav-wide-wrapper present.
      grep -q 'class="nav-wide-wrapper"' spec/components/gateway.html \
        || { echo "FAIL: nav-wide-wrapper missing (QA2-R3)" >&2; exit 1; }
      # QA2-D: no "pp." page-backrefs in HTML glossary.
      if grep -q 'pp\.' glossary.html; then
        echo "FAIL: glossary.html has 'pp.' page-backrefs (QA2-D)" >&2; exit 1
      fi
      # QA4-#9/QA5-B: no duplicate <defs id="glyph">/<defs id="clip-path">,
      # no /internal/shiroa.js (svg-dedup strips all three).
      if grep -rqE 'id="glyph"|id="clip-path"|/internal/shiroa\.js' .; then
        echo 'FAIL: <defs id="glyph|clip-path"> or shiroa.js ref present (svg-dedup not run?)' >&2; exit 1
      fi
      # QA4-B output-level guards: title-dup show-rule removed +
      # range-limited promote applied. deployment.typ §3 reappears (was
      # eaten by `starts-with("deployment ")`); gateway H2 gap closed.
      grep -q '>Deployment Order</h2>' spec/system/deployment.html \
        || { echo "FAIL: deployment.html missing '>Deployment Order</h2>' (QA4-#3)" >&2; exit 1; }
      grep -q '>Responsibilities</h2>' spec/components/gateway.html \
        || { echo "FAIL: gateway.html missing '>Responsibilities</h2>' (QA4-#6 H2 gap)" >&2; exit 1; }
      touch $out
    '';
    # Serve-parity smoke. Runs raw shiroa build (no svg-dedup post-
    # process), then asserts the CSS rules that make serve-mode correct
    # are in the page (decoded from the data: URI) AND the search index
    # covers all chapters. Catches R1/R2-class regressions where a fix
    # is nix-postprocess-only.
    docs-serve-parity =
      pkgs.runCommand "rio-docs-serve-parity"
        (
          typstEnv
          // {
            nativeBuildInputs = [
              shiroaPkg
              pkgs.jq
              pkgs.coreutils
            ];
          }
        )
        ''
          set -euo pipefail
          export HOME=$TMPDIR XDG_DATA_HOME=$TMPDIR/.local/share
          mkdir -p $XDG_DATA_HOME/typst
          cp -rL "$TYPST_PACKAGE_CACHE_PATH" $XDG_DATA_HOME/typst/packages
          chmod -R u+w $XDG_DATA_HOME/typst
          cp -r ${compileRoot} root && chmod -R +w root && cd root
          shiroa build --root . --mode static-html ${mkInputArgs placeholderSha} -d $TMPDIR/out .
          # rio-css ships as data:text/css;base64,… — decode to grep.
          # Last entry: rio-css comes after the bundled chrome/general/
          # variables sheets in head.typ.
          css=$(grep -oP 'data:text/css;base64,\K[A-Za-z0-9+/=]+' \
            $TMPDIR/out/intro.html | tail -1 | base64 -d)
          echo "$css" | grep -q '\[fill="#000000"\]' || {
            echo "FAIL: serve-parity — [fill=#000000] CSS rule missing" >&2; exit 1; }
          echo "$css" | grep -q '\[stroke="#000000"\]' || {
            echo "FAIL: serve-parity — [stroke=#000000] CSS rule missing" >&2; exit 1; }
          n=$(jq '.doc_urls | length' $TMPDIR/out/searchindex.json)
          test "$n" -ge 30 || {
            echo "FAIL: serve-parity — searchindex has $n docs (expected ≥30)" >&2; exit 1; }
          touch $out
        '';
  };
}
