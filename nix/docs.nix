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
        finite
        autograph
        pinit
        suiji
        # chronos 0.3.0 wants typst ≥0.14.2; shiroa embeds 0.14.0.
        chronos_0_2_1
        shiroa
        shiroa-mdbook
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

  # Typst sources only. docs/gen/ is excluded — compileRoot overlays
  # the hermetic docsData there.
  docsSrc = lib.fileset.toSource {
    root = ../docs;
    fileset = lib.fileset.difference ../docs ../docs/gen;
  };

  # Generated reference data (metric/alert/error/config tables).
  # `xtask regen docs-data` scans rio-*/src/**/*.rs for describe_*! and
  # `pub enum *Error` plus prometheusrule.yaml for alert names. The
  # crate2nix-built xtask binary's compile-time CARGO_MANIFEST_DIR is a
  # store path, so RIO_REPO_ROOT points it at the runCommand src tree.
  #
  # Fileset is the minimal scan surface — rio-*/src/*.rs (read_dir scan
  # needs the rio-* dir level to exist, fileFilter at the crate level
  # gives that) + the prometheusrule template. NOT the full workspaceSrc:
  # editing a Cargo.toml or a test file shouldn't rebuild the PDF.
  docsData =
    pkgs.runCommand "rio-docs-data"
      {
        nativeBuildInputs = [
          xtaskBin
          pkgs.jq
        ];
        src = lib.fileset.toSource {
          root = ../.;
          fileset = lib.fileset.unions [
            (lib.fileset.fileFilter (f: f.hasExt "rs") ../.)
            ../infra/helm/rio-build/templates/prometheusrule.yaml
          ];
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
        test "$(jq '.variants|length' $out/errors.json)" -gt 0
      '';

  # Compile root: docs sources + generated data, fused into one tree
  # so typst's `--root` sees `/lib`, `/spec`, and `/gen` together.
  compileRoot = pkgs.runCommand "rio-docs-root" { } ''
    mkdir -p $out
    cp -r ${docsSrc}/* $out/
    cp -r ${docsData} $out/gen
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
