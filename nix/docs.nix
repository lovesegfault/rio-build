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
  # Fileset is the minimal scan surface: rio-*/src/*.rs, every
  # Cargo.toml (workspace() reads each member's [package].description
  # + [dependencies]/[dev-dependencies]/[target.*] for the full crate
  # graph), and the two helm files alerts()/helm_ns() read.
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
            (lib.fileset.fileFilter (f: f.name == "Cargo.toml") ../.)
            ../infra/helm/rio-build/templates/prometheusrule.yaml
            ../infra/helm/rio-build/values.yaml # helm_ns()
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
        test "$(jq '.members|length' $out/workspace.json)" -gt 0
        test "$(jq 'keys|length' $out/consts.json)" -gt 0
        test "$(jq 'keys|length' $out/helm-ns.json)" -eq 4
      '';

  # `--input` pairs both targets must see. Factored so the PDF and
  # shiroa-HTML invocations can't drift (bug_003: HTML omitted gh-sha,
  # so refs.gh() permalinks pointed at /blob/main/). x-target stays
  # per-invocation (shiroa sets it itself; PDF passes book-pdf).
  typstInputs = [ "gh-sha=${self.rev or "dirty"}" ];
  inputArgs = lib.concatMapStringsSep " " (i: "--input ${lib.escapeShellArg i}") typstInputs;

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
          ${inputArgs} --input x-target=book-pdf \
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
          ${inputArgs} \
          --font-path "$TYPST_FONT_PATHS" -d $out .
      '';

  checks = {
    typst = docs-pdf;
    shiroa = docs;
    # PDF/HTML divergence smoke. Asserts the two cross-target invariants
    # bug_003/033/025 broke: gh-sha permalinks pinned (no own-repo
    # /blob/main/) and rref() anchors live (~115 same-chapter rrefs as
    # of 2026-05 after the _rid()-strip fix; cross-chapter degrade to
    # plain text by design — shiroa static-html compiles per-chapter —
    # so this is a regression tripwire, not an exhaustive count). Runs
    # against the built `docs` output so it's free (just greps result/).
    shiroa-smoke = pkgs.runCommand "rio-docs-html-smoke" { } ''
      set -euo pipefail
      cd ${docs}
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
      touch $out
    '';
  };
}
