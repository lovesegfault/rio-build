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
        # gitignored local-build artifacts that may exist on disk
        (lib.fileset.maybeMissing ../docs/dist)
        (lib.fileset.maybeMissing ../docs/.cache)
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
    # Placeholder until Task 10 lands the full assertion suite against
    # the native-bundle output shape. Keeps the attr name alive so
    # flake.nix's `// docsLib.checks` doesn't need touching mid-series.
    docs-html-smoke = pkgs.runCommand "rio-docs-html-smoke" { } ''
      test -f ${docsCheck}/index.html
      touch $out
    '';
  };
}
