# Typst design-book build pipeline.
#
# Produces a hermetic typst environment (rioTypst) with the @preview/*
# packages the book uses, plus two output forms:
#   - docs-pdf  : single-file PDF via `typst compile` (checks.docs-pdf)
#   - docs      : static HTML site via `typst compile --format bundle`
#                 + pagefind search index (checks.docs-html)
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

  # Transitive closure over propagatedBuildInputs. wrapper.nix only
  # walks one hop; this walks to fixpoint.
  closeDeps =
    ps: lib.unique (lib.concatMap (p: [ p ] ++ closeDeps (p.propagatedBuildInputs or [ ])) ps);

  # The package set the book imports, plus their transitive closure.
  # tracey lives outside the `typstPackages` scope (built from a flake
  # input source), so it's spliced in after the `with p;` list.
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
      ])
      ++ [
        traceyTypstPkg
      ]
    );

  fonts = [
    pkgs.newcomputermodern
  ];

  # Wrapped typst binary: TYPST_PACKAGE_CACHE_PATH + TYPST_FONT_PATHS
  # baked into $out/bin/typst via makeWrapper. `packages` is a
  # function (typstPackages scope → list); `fonts` is a list of paths
  # joined with ':' (typst scans them recursively).
  rioTypst = pkgs.typst.wrapper {
    packages = typstDeps;
    inherit fonts;
  };

  # The same package/font paths the rioTypst wrapper bakes in, exposed
  # as a runCommand env so `--font-path "$TYPST_FONT_PATHS"` and any
  # tool that reads the cache path see hermetic values.
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
        # Contributor bug-pattern catalog cited from rust comments by
        # literal path; not book content. Excluded so editing it doesn't
        # rebuild docs-pdf/docs-html.
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
  # HTML invocations can't drift (bug_003: HTML omitted gh-sha, so
  # refs.gh() permalinks pointed at /blob/main/). x-target stays
  # per-invocation (HTML passes `html`, PDF passes `book-pdf`).
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
            rioTypst
            pkgs.pagefind
            pkgs.makeWrapper
          ];
        }
      )
      ''
        # Native typst bundle export: one `typst compile` writes the
        # full multi-page site (book.typ's html-target arm walks the
        # manifest and emits one .html per chapter). rioTypst is the
        # wrapped binary so TYPST_PACKAGE_CACHE_PATH/TYPST_FONT_PATHS
        # are baked in; --font-path is belt-and-suspenders.
        cp -r ${compileRoot} ./root && chmod -R +w ./root
        cd ./root
        mkdir -p $out
        typst compile --features bundle,html --format bundle \
          --root . ${mkInputArgs ghSha} --input x-target=html \
          --font-path "$TYPST_FONT_PATHS" book.typ $out/
        # Webfonts: ship the NCM faces style.css references — body text
        # (NewCM10 regular/bold/italic/bold-italic), mono (NewCMMono10
        # regular/bold), and math (NewCMMath-Regular for the Plane-1
        # glyphs typst emits, U+1D400–, which need an OpenType MATH
        # table). Matches the @font-face set in docs/assets/style.css.
        mkdir -p $out/assets/fonts
        cp ${pkgs.newcomputermodern}/share/fonts/opentype/public/{NewCMMath-Regular,NewCM10-{Regular,Bold,Italic,BoldItalic},NewCMMono10-{Regular,Bold}}.otf \
          $out/assets/fonts/
        # Static search index over the emitted HTML.
        pagefind --site $out --output-subdir pagefind
        # `nix run .#docs` → serve the built tree. Only --index and the
        # docs path are baked in; everything else (port, interface,
        # auth, tls) passes through, e.g. `nix run .#docs -- -p 9000`.
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
    # Structural smoke over the native-bundle output. Asserts the
    # multi-page shape (every meta.typ chapter has a route), the page
    # shell rendered (nav/active-marker/edit-link), fletcher diagrams
    # survived html-target as inline SVG, and the pagefind index
    # landed. Closes over docsCheck (placeholder-SHA) so it caches
    # across commits that don't touch typst sources.
    docs-html-smoke =
      let
        html = docsCheck;
      in
      pkgs.runCommand "rio-docs-html-smoke" { } ''
        # (a) every chapter route has a file. Route list is scraped
        # from meta.typ (the `chapters` table is the bundle's source of
        # truth) — `typst query` is deprecated in 0.15, so grep is the
        # primary path. `intro.typ` → index per meta.typ's route-for.
        routes=$(grep -oE '"[a-z][a-z0-9/-]*\.typ"' \
            ${compileRoot}/lib/html/meta.typ \
          | tr -d '"' | sed 's/\.typ$//; s/^intro$/index/' | sort -u)
        n=$(printf '%s\n' "$routes" | wc -l)
        echo "docs-html-smoke: $n routes from meta.typ"
        # Floor guards the regex: a meta.typ reformat that breaks the
        # scrape would otherwise pass on zero routes.
        test "$n" -ge 30
        for r in $routes; do
          test -f ${html}/$r.html || { echo "missing: $r.html"; exit 1; }
        done
        # (b) nav present + active-page marker on the landing page
        grep -q '<nav' ${html}/index.html
        grep -q 'aria-current="page"' ${html}/index.html
        # (c) edit-this-page link (page.typ footer; repo-edit-base from
        # meta.typ). architecture.typ is a top-level chapter so the
        # href is unambiguous.
        grep -q 'github.com/lovesegfault/rio-build/edit/main/docs/architecture.typ' \
          ${html}/architecture.html
        # (d) fletcher diagram survived html-target as inline SVG
        grep -q '<svg' ${html}/architecture.html
        # (e) pagefind index emitted
        test -s ${html}/pagefind/pagefind.js
        # (f) bug_003: refs.gh() permalinks pin a commit, not /blob/main/
        ! grep -rq 'lovesegfault/rio-build/blob/main/' ${html}
        # (g) bug_033/025: #r() emits an `id="r-…"` anchor per marker so
        # rref() resolves. gateway.typ has ~100 markers; floor 80.
        test "$(grep -oE 'id="r-[a-z]' \
          ${html}/spec/components/gateway.html | wc -l)" -ge 80
        # (h) QA #1: zero-width html.frame() SVGs
        ! grep -rqE '<svg[^>]*width="0pt"' ${html}
        # (i) QA2-D: page backref leak into glossary HTML
        ! grep -q 'pp\.' ${html}/glossary.html
        # (j) design assertion: nothing from the retired pipeline leaked
        # (charclass dodges the deny_shared identifier lint on this file)
        ! grep -rqiE 'shi[r]oa' ${html}
        touch $out
      '';
  };
}
