# Typst design-book build pipeline.
#
# Produces a hermetic typst environment (rioTypst) with the @preview/*
# packages the book uses, plus two output forms:
#   - docs-pdf  : single-file PDF via `typst compile` (checks.typst)
#   - docs      : static HTML site via shiroa (checks.shiroa)
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
  # tracey lives outside the `typstPackages` scope (it's not in
  # nixpkgs), so it's spliced in after the `with p;` list.
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
        lilaq
        fletcher
        suiji
        # chronos 0.3.0 wants typst ≥0.14.2; shiroa embeds 0.14.0.
        chronos_0_2_1
        shiroa
        shiroa-starlight
      ])
      ++ [ traceyTypstPkg ]
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

  # Only the typst-side sources. The mdbook tree at docs/src/ stays
  # out so editing a .md doesn't rebuild the PDF.
  docsSrc = lib.fileset.toSource {
    root = ../docs;
    fileset = lib.fileset.unions (
      map lib.fileset.maybeMissing [
        ../docs/lib
        ../docs/spec
        ../docs/book.typ
        ../docs/book-pdf.typ
      ]
    );
  };

  # Generated reference data (metric/alert/error/config tables).
  # Stubbed here — Phase B replaces this with the real extractors.
  docsData = pkgs.runCommand "rio-docs-data-stub" { } ''
    mkdir -p $out/gen
    for f in metrics alerts; do echo '{"names":[]}' > $out/gen/$f.json; done
    echo '{"variants":[]}' > $out/gen/errors.json
    echo '{"components":{}}' > $out/gen/config.json
  '';

  # Compile root: docs sources + generated data, fused into one tree
  # so typst's `--root` sees `/lib`, `/spec`, and `/gen` together.
  compileRoot = pkgs.runCommand "rio-docs-root" { } ''
    mkdir -p $out
    cp -r ${docsSrc}/* $out/
    cp -r ${docsData}/gen $out/gen
  '';
in
rec {
  inherit rioTypst typstEnv docsData;

  docs-pdf =
    pkgs.runCommand "rio-docs-pdf"
      (
        typstEnv
        // {
          nativeBuildInputs = [ rioTypst ];
        }
      )
      ''
        typst compile --root ${compileRoot} -f pdf \
          --input x-target=book-pdf \
          --input gh-sha=${self.rev or "dirty"} \
          ${compileRoot}/book-pdf.typ $out
      '';

  docs =
    pkgs.runCommand "rio-docs-html"
      (
        typstEnv
        // {
          nativeBuildInputs = [
            shiroaPkg
            rioTypst
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
          --font-path "$TYPST_FONT_PATHS" -d $out .
      '';

  checks = {
    typst = docs-pdf;
    shiroa = docs;
  };
}
