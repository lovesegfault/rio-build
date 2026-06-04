# Non-rustc check derivations (shared by checks.* and ci aggregate).
#
# Rust checks (clippy/nextest/doc/coverage) are in nix/checks.nix
# (per-crate caching, deps built once). These are the rest:
# workspace-level policy checks that don't invoke rustc.
{
  pkgs,
  inputs,
  config,
  version,
  unfilteredRoot,
  workspaceFileset,
  # Manifests + lockfile only (nix/lib/nextest-args.nix). Paired with
  # `stubTargetFiles` so `cargo metadata` works without `.rs` source —
  # the `deny` check reads only manifests, and tying it to
  # `workspaceFileset` made it rebuild on every `.rs` edit.
  manifestsFileset,
  stubTargetFiles,
  rustStable,
  rustPlatformStable,
  traceyPkg,
  subcharts,
  dockerImages,
  nodeAmi,
  docsLib,
  xtaskBin,
}:
let
  # Regenerate-then-diff drift check. `generate` populates
  # $TMPDIR/gen (file or dir); `committed` is the path to compare
  # against. `diff -r` works for both file and directory inputs.
  mkDriftCheck =
    {
      name,
      nativeBuildInputs ? [ ],
      generate,
      committed,
      what,
      regenHint,
    }:
    pkgs.runCommand "rio-${name}" { nativeBuildInputs = nativeBuildInputs ++ [ pkgs.diffutils ]; } ''
      ${generate}
      diff -r $TMPDIR/gen ${committed} > $TMPDIR/diff || {
        echo "FAIL: ${what}" >&2
        echo "Run: ${regenHint}" >&2
        cat $TMPDIR/diff >&2
        exit 1
      }
      touch $out
    '';
in
{
  # License + advisory audit. Policy: deny GPL-3.0 (project is
  # MIT/Apache), fail on RustSec advisories with a curated
  # ignore list in .cargo/deny.toml. The advisory DB is a
  # flake input (hermetic — no network). Bump via `nix flake
  # update advisory-db` to pick up new advisories.
  #
  # cargo-deny internally runs `cargo metadata` to resolve
  # the full dep tree for license/advisory analysis. That
  # needs vendored sources (cargoSetupHook writes a source-
  # replacement config so cargo finds crates.io deps in the
  # vendored dir instead of the registry index).
  #
  # src is manifests + lockfile + deny.toml only — cargo-deny
  # never reads `.rs` content (license is `[workspace.package]`,
  # not per-file headers; advisories/bans/sources come from
  # Cargo.lock). `stubTargetFiles` synthesizes the empty target
  # files cargo's autodiscovery needs at build time, computed
  # at eval time from pathExists/readDir — so a `.rs` body edit
  # never rehashes this drv, only adding/removing a target
  # file or touching a manifest does.
  deny = pkgs.stdenv.mkDerivation {
    pname = "rio-deny";
    inherit version;
    src = pkgs.lib.fileset.toSource {
      root = unfilteredRoot;
      fileset = pkgs.lib.fileset.unions [
        ../.cargo/deny.toml
        manifestsFileset
      ];
    };
    cargoDeps = rustPlatformStable.importCargoLock {
      lockFile = ../Cargo.lock;
    };
    nativeBuildInputs = with pkgs; [
      cargo-deny
      rustStable
      rustPlatformStable.cargoSetupHook
      git
    ];
    # cargoSetupHook writes .cargo/config.toml with vendored
    # source replacement. cargo metadata reads it; no registry
    # access needed.
    buildPhase = ''
      ${stubTargetFiles}
      # HOME defaults to /homeless-shelter (RO). deny.toml's
      # db-path = "~/.cargo/advisory-db" resolves against
      # HOME. cargo-deny expects the DB as a GIT REPO (reads
      # HEAD to determine DB version for the report). The
      # flake input is a plain dir (flake=false strips .git),
      # so we init a throwaway repo with the content.
      export HOME=$TMPDIR
      db=$HOME/.cargo/advisory-db/advisory-db-3157b0e258782691
      mkdir -p "$db"
      cp -r ${inputs.advisory-db}/. "$db"/
      chmod -R u+w "$db"
      git -C "$db" init -q
      git -C "$db" add -A
      git -C "$db" \
        -c user.name=nix -c user.email=nix@localhost \
        commit -q -m snapshot
      cargo deny \
        --manifest-path ./Cargo.toml \
        --offline \
        check \
        --config ./.cargo/deny.toml \
        --disable-fetch \
        advisories licenses bans sources \
        2>&1 | tee deny.out
    '';
    installPhase = ''
      cp deny.out $out
    '';
  };

  # workspace-hack drift check. The pre-commit `hakari-check` hook is
  # gated on `git diff --cached` and so no-ops in the hermetic
  # `pre-commit run --all-files` derivation (nothing is staged) — the
  # same documented limitation as `crate2nix-check`. workspace-hack is
  # also stubbed in nix builds (nix/crate2nix.nix), so a stale
  # workspace-hack never breaks CI directly. Without this check the
  # only enforcement is the pre-commit hook, which `--no-verify` and
  # any non-interactive push path bypass.
  #
  # Stale workspace-hack means per-package `cargo build -p X` resolves
  # a narrower feature set than `cargo build --workspace`, causing
  # full recompiles on every workspace↔package switch. Silent local
  # dev-loop degradation, no CI signal.
  #
  # `cargo hakari verify` is a metadata-only check (no rustc) that
  # asserts workspace-hack still unifies one version of every
  # non-omitted third-party crate. Same `cargoSetupHook +
  # importCargoLock` setup as `deny` so `cargo metadata` resolves
  # against vendored sources without network.
  hakari-drift = pkgs.stdenv.mkDerivation {
    pname = "rio-hakari-drift";
    inherit version;
    src = pkgs.lib.fileset.toSource {
      root = unfilteredRoot;
      fileset = pkgs.lib.fileset.unions [
        ../.config/hakari.toml
        workspaceFileset
      ];
    };
    cargoDeps = rustPlatformStable.importCargoLock {
      lockFile = ../Cargo.lock;
    };
    nativeBuildInputs = with pkgs; [
      cargo-hakari
      rustStable
      rustPlatformStable.cargoSetupHook
    ];
    buildPhase = ''
      export HOME=$TMPDIR
      # cargoSetupHook writes .cargo/config.toml with vendored source
      # replacement; cargo-hakari's internal `cargo metadata` reads it.
      # CARGO_NET_OFFLINE belt-and-braces against accidental registry
      # access if a future cargo-hakari version changes its metadata
      # invocation.
      export CARGO_NET_OFFLINE=true
      cargo hakari verify || {
        echo 'error: workspace-hack is stale — run `cargo xtask regen hakari`'
        exit 1
      }
    '';
    installPhase = ''
      touch $out
    '';
  };

  # Spec-coverage validation: fails on broken r[...]
  # references, duplicate requirement IDs, or unparseable
  # include files. Does NOT fail on uncovered/untested — those
  # are informational.
  #
  # tracey scans docs/spec/**/*.typ (spec) + .rs/.nix/.py/.ts for
  # `r[impl/verify ...]` annotations + .config/tracey/config.styx.
  # `.ts` is required: config.styx lists rio-dashboard/src/{lib,api}/*.ts
  # in `impls.include` and `__tests__/*.ts` in `test_include`; without
  # it the gate passes vacuously for the dashboard's spec coverage.
  # tracey's daemon writes .tracey/daemon.sock under the working
  # dir, so we cp to a writable tmpdir first.
  tracey-validate =
    pkgs.runCommand "rio-tracey-validate"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            ../docs
            ../.config/tracey
            ../nix/tests/default.nix
            (pkgs.lib.fileset.fileFilter (
              f: f.hasExt "rs" || f.hasExt "nix" || f.hasExt "py" || f.hasExt "ts"
            ) unfilteredRoot)
          ];
        };
        nativeBuildInputs = [ traceyPkg ];
      }
      ''
        cp -r $src $TMPDIR/work
        chmod -R +w $TMPDIR/work
        cd $TMPDIR/work
        rm -rf .tracey/
        export HOME=$TMPDIR
        set -o pipefail
        # Retry once: `tracey query validate` auto-starts a daemon
        # and waits 5s for the socket. Under sandbox parallel-build
        # load the socket-wait races ("Daemon failed to start within
        # 5s" / "Error getting status: Cancelled"). tracey 1.3.0 has
        # no --no-daemon mode and no TRACEY_DAEMON_TIMEOUT knob, so
        # retry-once is the minimal fix. P0490.
        tracey query validate 2>&1 | tee $out || {
          echo "retry: first tracey attempt failed, retrying once" >&2
          rm -rf .tracey/  # clear partial daemon state
          sleep 2
          tracey query validate 2>&1 | tee $out
        }
      '';

  # Workspace-level invariant lints — pure file walks over staged
  # source, no DB/network. `xtask lint` with no subcommand runs every
  # `Lint` variant, so the dispatch is self-discovering: adding a lint
  # to `xtask/src/lint.rs` adds it here without editing this file. The
  # per-lint rationale lives on the enum's variant doc-comments.
  #
  # The fileset is the union of every lint's read surface — keep it
  # matched. A new lint that reads files outside the union below MUST
  # extend it, or the lint sees a partial tree under nix and fails (or
  # worse, passes vacuously). Run the lint once with `xtask lint` from
  # a clean checkout vs. `nix build .#checks.<system>.xtask-lint` to
  # confirm parity. The narrow fileset is the point: rebuild only when
  # a lint's input changes, not on every workspace edit.
  #
  # The crate2nix-built xtask's compile-time `CARGO_MANIFEST_DIR` is a
  # store path, so `RIO_REPO_ROOT` points it at the staged fileset
  # (same pattern as docsData in nix/docs.nix).
  xtask-lint =
    pkgs.runCommand "rio-xtask-lint"
      {
        nativeBuildInputs = [ xtaskBin ];
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            ../rio-migrations/migrations
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-store/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-scheduler/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-controller/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../xtask/src)
            # floatingness-probe (round-17 merged_bug_062) walks the
            # owner crate + the remaining consumer crates — without
            # them in the fileset the lint passes VACUOUSLY under nix
            # (the trap this header warns about).
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-nix/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-gateway/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-builder/src)
            ../infra/helm/rio-build/templates/scheduler.yaml
            # seccomp-allowlist validates both Localhost profiles —
            # rio-builder.json and rio-fetcher.json. Both must be in
            # the fileset so an edit to either rehashes this check.
            ./nixos-node/seccomp/rio-builder.json
            ./nixos-node/seccomp/rio-fetcher.json
          ];
        };
      }
      ''
        export RIO_REPO_ROOT=$src
        xtask lint
        touch $out
      '';

  # Helm chart lint + template for all value profiles. Catches
  # Go-template syntax errors, missing required values, bad
  # YAML in rendered output. Subcharts symlinked from nixhelm
  # (FOD) — `helm dependency build` needs network.
  #
  # Per-assertion fragments live in nix/tests/helm/*.sh. Each fragment
  # is self-contained: it does its own `helm template` render(s) and
  # asserts against the output. The driver below provides the chart
  # workdir (subcharts symlinked) and runs each fragment under
  # `bash -euo pipefail`. Fail-fast: first failing fragment aborts.
  helm-lint =
    let
      chart = pkgs.lib.cleanSource ../infra/helm/rio-build;
      fragments = pkgs.lib.fileset.toSource {
        root = ./tests/helm;
        fileset = pkgs.lib.fileset.fileFilter (f: f.hasExt "sh") ./tests/helm;
      };
    in
    pkgs.runCommand "rio-helm-lint"
      {
        nativeBuildInputs = [
          pkgs.kubernetes-helm
          pkgs.yq-go
          pkgs.jq
          pkgs.gnugrep
        ];
      }
      ''
        cp -r ${chart} $TMPDIR/chart
        chmod -R +w $TMPDIR/chart
        cd $TMPDIR/chart
        mkdir -p charts
        ln -s ${subcharts.postgresql} charts/postgresql
        # 22-alert-quality.sh asserts every RioNodeclaimPool* alert has a
        # runbook table row. The fragment sandbox only has the chart and
        # nix/tests/helm/*.sh as build inputs — docs/ is unreachable —
        # so stage the runbook it cross-references.
        cp ${../docs/ops/sla-model.typ} $TMPDIR/chart/.runbook-sla-model.typ

        for f in ${fragments}/*.sh; do
          echo "▸ helm-lint: $(basename "$f" .sh)" >&2
          bash -euo pipefail "$f"
        done
        touch $out
      '';

  # proxy_buffering off in dashboardNginxConf is LOAD-BEARING
  # (docker.nix:349): nginx default-buffers upstream → WatchBuild /
  # GetDerivationLogs streams arrive as one blob at close. The config is a
  # writeText baked into the dashboard image, invisible to helm-lint.
  # vm-dashboard-k3s's 0x80-at-tail grep can't distinguish (NotFound is
  # tiny either way) — this is the structural backstop.
  dashboard-nginx-conf-guard = pkgs.runCommand "rio-dashboard-nginx-conf-guard" { } ''
    grep -F 'proxy_buffering off;' ${dockerImages.dashboardNginxConf} >/dev/null || {
      echo "FAIL: dashboardNginxConf lost 'proxy_buffering off;' — gRPC-Web streams will buffer" >&2
      exit 1
    }
    # Syntax check: njs js_import/js_set wiring is easy to get wrong
    # and vm-dashboard-k3s is the only other place nginx parses this.
    # `nginx -t` resolves upstreams and open()s the error_log/access_log
    # targets after parsing — sed the cluster FQDN to a resolvable
    # address and /dev/std{err,out} to TMPDIR (a remote build sandbox
    # may not provide /dev/std*). Everything else is checked verbatim.
    mkdir -p $TMPDIR/logs
    sed -e 's/rio-scheduler\.rio-system\.svc\.cluster\.local/127.0.0.1/' \
        -e "s#/dev/stderr#$TMPDIR/logs/error.log#" \
        -e "s#/dev/stdout#$TMPDIR/logs/access.log#" \
      ${dockerImages.dashboardNginxConf} > $TMPDIR/nginx.conf
    ${dockerImages.dashboardNginx}/bin/nginx -t -p $TMPDIR -c $TMPDIR/nginx.conf
    touch $out
  '';

  # CRD drift: crdgen output (one file per CRD) must equal the
  # committed infra/helm/crds/. Catches the "Rust CRD struct
  # changed but nobody ran cargo xtask regen crds" drift — the committed
  # YAML is what Argo syncs, so a stale file means the deployed
  # schema diverges from what the controller expects.
  #
  # packages.crds is a directory with one `<crd-name>.yaml` per CRD,
  # produced by the same crdgen binary `cargo xtask regen crds` runs —
  # single serialization path, so the bytes match by construction.
  crds-drift = mkDriftCheck {
    name = "crds-drift";
    generate = ''
      cp -r ${config.packages.crds} $TMPDIR/gen
    '';
    committed = ../infra/helm/crds;
    what = "crdgen output drifted from infra/helm/crds/";
    regenHint = "cargo xtask regen crds";
  };

  # infra/eks/generated.auto.tfvars.json must match nix/pins.toml.
  # jq -S on both sides so key-order and whitespace don't matter
  # (committed file is pretty-printed, writeText output is compact).
  # The generate side goes through the nix path (pins.nix shim →
  # builtins.fromTOML → toJSON) while `cargo xtask regen tfvars` writes
  # the committed side from the toml crate — so this check also catches
  # the two parsers ever disagreeing about pins.toml.
  tfvars-fresh = mkDriftCheck {
    name = "tfvars-fresh";
    nativeBuildInputs = [ pkgs.jq ];
    generate = ''
      jq -S . ${config.packages.tfvars} > $TMPDIR/gen
      jq -S . ${../infra/eks/generated.auto.tfvars.json} > $TMPDIR/committed
    '';
    committed = "$TMPDIR/committed";
    what = "nix/pins.toml drifted from infra/eks/generated.auto.tfvars.json";
    regenHint = "cargo xtask regen tfvars";
  };

  # docs/gen/*.json are committed (nextest + dev-shell typst read them
  # from the working tree) AND regenerated hermetically by docsData for
  # the nix docs build. Drift means CI's docs build accepts a metric the
  # dev's local `typst compile` rejects, and nextest under-covers.
  #
  # Both the local and nix-built xtask now build serde_json with the
  # workspace-pinned `preserve_order` feature, so they emit identical
  # object-key and array ordering. The committed copy is the local
  # one; this check cares about content. `jq -S` + `walk(sort)`
  # canonicalises both sides as insurance against any future ordering
  # difference between the local and hermetic builds — same approach
  # as tfvars-fresh, deepened for nested arrays.
  docs-data-fresh = mkDriftCheck {
    name = "docs-data-fresh";
    nativeBuildInputs = [ pkgs.jq ];
    generate = ''
      # Recursive canonical sort: keys sorted (via -S after), arrays
      # sorted by canonical-JSON of each element. `walk` is bottom-up
      # so by the time it sorts an array, child objects already had
      # their keys ordered — `tojson` is stable.
      canon='walk(
        if type=="object" then to_entries|sort_by(.key)|from_entries
        elif type=="array" then sort_by(tojson)
        else . end)'
      mkdir -p $TMPDIR/gen $TMPDIR/committed
      for f in ${docsLib.docsData}/*.json; do
        jq -S "$canon" "$f" > $TMPDIR/gen/"$(basename "$f")"
      done
      for f in ${../docs/gen}/*.json; do
        jq -S "$canon" "$f" > $TMPDIR/committed/"$(basename "$f")"
      done
    '';
    committed = "$TMPDIR/committed";
    what = "docs/gen/*.json drifted from xtask regen docs-data output";
    regenHint = "nix develop -c cargo xtask regen docs-data";
  };

  # Prose references that bypass lib/refs.typ validators. Grep-based —
  # typst itself can't see raw backticks. Each pattern catches a class
  # that has produced ≥1 bughunter finding (Nth-strike close for
  # merged_001/028/032 et al.; tightened per merged_015/bug_005).
  docs-lint =
    pkgs.runCommand "rio-docs-lint"
      {
        nativeBuildInputs = [ pkgs.jq ];
        # docs/**/*.typ minus gitignored build artifacts (mirrors
        # docsSrc — without the difference, .cache/typst-xdg vendored
        # packages (~230 .typ files) are scanned and the lint becomes
        # non-deterministic across dev environments).
        typSrc = pkgs.lib.fileset.toSource {
          root = ../docs;
          fileset = pkgs.lib.fileset.difference (pkgs.lib.fileset.fileFilter (f: f.hasExt "typ") ../docs) (
            pkgs.lib.fileset.unions [
              (pkgs.lib.fileset.maybeMissing ../docs/.cache)
              (pkgs.lib.fileset.maybeMissing ../docs/dist)
            ]
          );
        };
        # Non-.typ files that reference docs by path — rust comments +
        # warn! bodies, nix comments, github workflows, shell scripts,
        # helm chart annotations / tofu comments / NOTES.txt.
        crossSrc = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs" || f.name == "Cargo.toml") ../.)
            # .json under nix/: the seccomp profiles carry prose `"//"`
            # comments that reference spec markers + crate names — same
            # drift surface as .nix/.sh.
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "nix" || f.hasExt "sh" || f.hasExt "json") ../nix)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "ts") ../rio-dashboard)
            ../flake.nix
            ../CLAUDE.md
            ../README.md
            (pkgs.lib.fileset.fileFilter (
              f:
              builtins.any f.hasExt [
                "yaml"
                "yml"
                "tf"
                "tfvars"
                "txt"
                "tpl"
              ]
            ) ../infra)
            ../.github
            ../.cargo
          ];
        };
        # Directories #src()/refs.gh paths reference. Fileset (not bare
        # unfilteredRoot) so docs-lint's hash doesn't depend on
        # Cargo.lock / fuzz corpora / target/. If a future #src()
        # points outside this set the lint reports DEAD — extend the
        # union, don't switch to unfilteredRoot.
        pathSrc = pkgs.lib.fileset.toSource {
          root = unfilteredRoot;
          fileset = pkgs.lib.fileset.unions (
            [
              ../nix
              ../infra
              ../.config
              ../.github
              ../rio-proto/proto
            ]
            ++ builtins.map pkgs.lib.fileset.maybeMissing [
              ../docs/gen
              ../docs/spec
              ../docs/ref
              ../docs/ops
              # #src("fuzz/rio-{nix,store}/") — Cargo.toml only so the
              # directory exists without pulling corpora/Cargo.lock into
              # the hash (see comment above).
              ../fuzz/rio-nix/Cargo.toml
              ../fuzz/rio-store/Cargo.toml
            ]
            ++ map (m: ../. + "/${m}") (
              builtins.filter (m: m != "workspace-hack")
                (builtins.fromTOML (builtins.readFile ../Cargo.toml)).workspace.members
            )
          );
        };
        metricsJson = ../docs/gen/metrics.json;
        cliJson = ../docs/gen/cli.json;
      }
      ''
        set -euo pipefail
        fail=0
        # Internal .md link — must be #cross-link("/abs.typ") or plain text.
        # Match every .md target, then exclude only URLs (`://`). Covers both
        # `#link("x.md")` and `#link("x.md", body)` two-arg forms.
        if grep -rn --include='*.typ' -E '#link\("[^"]*\.md"' $typSrc \
             | grep -v '://'; then
          echo "FAIL: internal #link to .md — use #cross-link or drop link" >&2
          fail=1
        fi
        # Raw backtick metric name — must be #(refs.metric)("…") so the
        # gen/metrics.json membership assert fires. Component-prefix
        # pattern (not suffix-based: 28 distinct suffixes, only 6 are
        # common; prefix catches all). Prefix alternation derived from
        # metrics.json so a new component auto-extends the lint.
        # ref/metrics.typ derives FROM metrics.json now so no exemption.
        comps=$(jq -r '.names[] | capture("^rio_(?<c>[a-z]+)_").c' $metricsJson \
          | sort -u | paste -sd'|')
        if grep -rn --include='*.typ' -E "\`rio_($comps)_[a-z_]+" $typSrc \
             | grep -v 'lib/refs\.typ'; then
          echo "FAIL: raw metric name — use #(refs.metric)(\"…\")" >&2
          fail=1
        fi
        # Stale `<chapter>.md` reference in non-typ sources — the
        # chapter was migrated to typst. Stem alternation derived from
        # book.typ's #chapter() list (the canonical chapter set; same
        # source as the book-pdf-subset check below) so a new chapter
        # auto-extends and lib/book/manifest stems are excluded by
        # construction.
        stems=$(grep -oE '#chapter\("[^"]+\.typ"' $typSrc/book.typ \
          | sed 's/#chapter("//;s/\.typ"//' \
          | xargs -n1 basename | sort -u | paste -sd'|')
        # Folded chapters: existed in docs/src/*.md, merged into other
        # .typ files during the migration (no 1:1 .typ). Frozen
        # migration-time diff of `git ls-tree origin/main -- docs/src/`
        # minus the book.typ chapter set; not derivable in the hermetic
        # build (no git).
        folded="challenges|dependencies|data-flows|decisions|components|integration|introduction|multi-tenancy|SUMMARY"
        stems="$stems|$folded"
        # nb: this file is in $crossSrc — the literal pattern below
        # would match itself, so misc-checks.nix is excluded post-hoc.
        if grep -rn -E "\b($stems)\.md\b|docs/src/" $crossSrc $typSrc \
             | grep -v 'nix/misc-checks\.nix'; then
          echo "FAIL: stale .md reference to a migrated chapter — update to .typ path" >&2
          fail=1
        fi
        # book-pdf.typ includes ⊆ book.typ chapters. Catches stale/typo'd
        # #include paths; the reverse (HTML chapter not in PDF) is
        # intentional per book-pdf.typ's scope comment.
        pdf=$(grep -oE '#include "[^"]+\.typ"' $typSrc/book-pdf.typ | sed 's/#include "//;s/"//')
        html=$(grep -oE '#chapter\("[^"]+\.typ"' $typSrc/book.typ | sed 's/#chapter("//;s/"//')
        stray=$(comm -23 <(echo "$pdf" | sort) <(echo "$html" | sort))
        if [[ -n "$stray" ]]; then
          echo "FAIL: book-pdf.typ includes chapters not in book.typ:" >&2
          echo "$stray" >&2
          fail=1
        fi
        # #src("path") and (refs.gh)("path:L") name files in the repo.
        # Assert each exists. bug_016: verification.typ referenced a
        # deleted scenario file. --exclude-dir=lib: refs.typ/rio.typ's
        # own header comments contain literal `(refs.gh)("path:line")`
        # examples that would false-positive.
        while IFS= read -r path; do
          p=''${path%%:*}
          if [[ ! -e "$pathSrc/$p" ]]; then
            echo "FAIL: #src/refs.gh references nonexistent path: $path" >&2
            fail=1
          fi
        done < <(grep -rohE --exclude-dir=lib '#src\("[^"]+"\)|\(refs\.gh\)\("[^"]+"\)' $typSrc \
          | sed -E 's/.*\("([^"]+)"\).*/\1/' | sort -u)
        # Retired identifiers — names that no longer exist in code/CRDs/
        # CLI but kept appearing in docs (R4: ≥5 instances). One
        # alternation per rename; future renames append here.
        #
        # Split into shared/docs/cross (R7-m025): a single alternation
        # over both scan sets is the structural reason "widen pattern X
        # → false-positive in the other scan set" recurs. deny_shared
        # is identifiers retired everywhere; deny_docs adds doc-only
        # phrases (legitimately appear in code as historical context);
        # deny_cross adds case/separator variants needed for nix/infra
        # that would FP docs' "Squid FOD proxy is deleted" prose.
        deny_shared='\bBuilderPool\b|\bFetcherPools?\b|rio-cli bps\b|`bps`|vm-lifecycle-bps|RIO_TLS__|\bTlsError\b|rio-common/src/tls\.rs|load_client_tls|init_client_tls|spec\.sizing|Sizing::|fuseCacheBudget|logBudget|migration-lock mechanism|trigger-gc|--grace-period-hours|mTLS client[- ]cert|mTLS cert mount|mTLS main port|VMs: mTLS|plaintext-health listener|TLS and plaintext ports|mTLS bypass|mTLS-identified|mTLS identifies|falls? back to mTLS|mTLS peer cert|\bplaintext port\b|CN-allowlist\)|\(gateway cert|dev-mode/dev-mode|TLS is env-only|\bTLS init\b|without relying on service tokens|replacement for the service-HMAC|RIO_JWT_SIGNING_KEY_PATH|rio\.jwt(Verify|Sign)Env|worker\.seccomp|`tls` / `metrics_addr`|\brio-worker\b'
        deny_docs="$deny_shared|\bmTLS\b|fod-proxy|bundled into the scheduler|kubectl exec deploy/rio-scheduler -- rio-cli"
        deny_cross="$deny_shared|[Ff][Oo][Dd][- ]proxy"
        # builder.typ's "Formerly `rio-worker`" info-box is the rename
        # record (deliberate); allowlist it for the rio-worker pattern.
        if grep -rn -E "$deny_docs" $typSrc | grep -vE 'glossary\.typ|builder\.typ:.*Formerly|builder\.typ:.*r\[worker'; then
          echo "FAIL: retired identifier in docs — see deny-list in misc-checks.nix" >&2
          fail=1
        fi
        # crossSrc allowlist: pool.rs:4-11 explains the BuilderPool→Pool
        # consolidation (legitimate history); flake.nix "Before this
        # assert, vm-lifecycle-bps-k3s and vm-fod-proxy-k3s" is
        # legitimate history; misc-checks.nix is the lint itself.
        if grep -rn -E "$deny_cross" $crossSrc \
             | grep -vE 'rio-crds/src/pool\.rs:([4-9]|1[01]):|flake\.nix:.*Before this assert|misc-checks\.nix'; then
          echo "FAIL: retired identifier in non-doc source" >&2
          fail=1
        fi
        # DEFAULT_GC_GRACE_HOURS literal-value tripwire — the const is
        # in gen/consts.json so prose must derive. Broad over $typSrc;
        # NARROW over $crossSrc (only the doc-comment shapes that
        # should cite the const — broad would FP `ungracefully` /
        # build_timeout's unrelated `2h` / test literals).
        if grep -rn -E '\b2h\b.*grace|grace.*\b2h\b' $typSrc; then
          echo "FAIL: literal '2h' grace-period — use #(refs.const)(\"DEFAULT_GC_GRACE_HOURS\")h" >&2
          fail=1
        fi
        if grep -rn -E "store's 2h\b|None = default \(2h\)|use default 2h\b" $crossSrc \
             | grep -v 'misc-checks\.nix'; then
          echo "FAIL: rust comment hardcodes 2h grace — cite DEFAULT_GC_GRACE_HOURS" >&2
          fail=1
        fi
        # Raw `rio-cli X` spans (inline backtick OR fenced-block
        # line-start) bypass refs.cli-sub. Extract and assert ∈
        # gen/cli.json. Nested subcommands (`rio-cli sla status`) only
        # check top-level `sla`.
        while IFS= read -r sub; do
          if ! jq -e --arg s "$sub" '.subcommands | index($s)' $cliJson > /dev/null; then
            echo "FAIL: raw \`rio-cli $sub\` — unknown subcommand (use #(refs.cli-sub) or fix)" >&2
            fail=1
          fi
        done < <(grep -rohE '(^|`)rio-cli [a-z][a-z-]*' $typSrc \
          | sed -E 's/(^|`)rio-cli //' | sort -u)
        # configuration.typ is 100% derived from rust Config::default()
        # via gen/config.json. Rust source citing it as a spec source is
        # inverted-dataflow (R3-m002, R4-003, R5-019). observability.typ
        # is NOT in this pattern — ~25 legitimate `Per observability.typ`
        # refs describe spec contracts that aren't derived.
        if grep -rn --include='*.rs' '\bconfiguration\.typ\b' $crossSrc \
             | grep -v 'render.*into\|flows\? into\|-> .*configuration\.typ'; then
          echo "FAIL: rust comment cites configuration.typ — it's derived FROM rust" >&2
          fail=1
        fi
        # describe_metrics() doc-comment parity. Five lib.rs comments
        # must point at xtask's canonical (and NONE may regress to the
        # old "sourced from" wording — second strike R3-m002 → R4-003).
        n=$(grep -rl 'docs_data.rs::metrics()' $crossSrc/rio-*/src/lib.rs | wc -l)
        if [[ $n -ne 5 ]]; then
          echo "FAIL: $n/5 describe_metrics() comments reference xtask canonical" >&2
          fail=1
        fi
        if grep -rn 'sourced from' $crossSrc/rio-*/src/lib.rs; then
          echo "FAIL: describe_metrics() comment regressed to old wording" >&2
          fail=1
        fi
        # QA4-B: chapter's FIRST `= ` heading must not duplicate its
        # manifest title — the synthetic <h1 class="rio-chapter-title">
        # already renders the title; a leading `= <Title>` either
        # (a) duplicates it (now that the QA3 show-rule is gone), or
        # (b) was previously suppressed leaving an H1→H3 skip /
        # §-starts-at-2. Heuristic matches the QA3 rule's three forms
        # (exact / `rio-<title>` / `<title> *` prefix). Limitation:
        # `[^]]+` title-extraction fails on a `]` in a manifest title;
        # none currently exist.
        while IFS=: read -r ch title; do
          # `|| true`: chapters with no `^= ` (post-migration glossary)
          # make grep exit 1; under set -e that's fatal before the
          # -z check below can skip them.
          first=$(grep -m1 '^= ' "$typSrc/$ch" 2>/dev/null | sed 's/^= //' || true)
          [[ -z "$first" ]] && continue
          tlow=$(echo "$title" | tr 'A-Z' 'a-z')
          flow=$(echo "$first" | tr 'A-Z' 'a-z')
          if [[ "$flow" == "$tlow" || "$flow" == "rio-$tlow" || "$flow" == "$tlow "* ]]; then
            echo "FAIL: $ch first heading '= $first' duplicates manifest title '$title'" >&2
            fail=1
          fi
        done < <(grep -oE '#chapter\("[^"]+\.typ"\)\[[^]]+\]' $typSrc/book.typ \
          | sed -E 's/#chapter\("([^"]+)"\)\[([^]]+)\]/\1:\2/')
        # QA4-#2: stray `=` at (post-indent) line-start — typst parses as
        # heading inside content blocks. book*.typ's `summary:[...]` part
        # markers (`= Guide`, `= Spec`) are intentional.
        if grep -rn --include='*.typ' --exclude='book*.typ' -E '^\s+= ' $typSrc; then
          echo "FAIL: indented '= ' parsed as heading — escape as '\= ' or rewrap" >&2
          fail=1
        fi
        # QA4-#1/#5: CSS-presence in source (base64-decode in docs-html-smoke
        # is fragile; source-grep is robust).
        grep -q '\.rio-frame svg' $typSrc/lib/rio.typ \
          || { echo "FAIL: lib/rio.typ missing '.rio-frame svg' (QA4-#1 invert scope)" >&2; fail=1; }
        grep -q 'scrollbar-width: thin' $typSrc/lib/rio.typ \
          || { echo "FAIL: lib/rio.typ missing 'scrollbar-width: thin' (QA4-#5)" >&2; fail=1; }
        [[ $fail -eq 0 ]]
        touch $out
      '';

  # REVIEW.md §-ref liveness — rust comments cite review-rule sections
  # by name; assert the section exists. Prefix match, hyphen↔space
  # folded (comments abbreviate `## Stability tests perturb…` to
  # `§Stability-tests`). Also catches "REVIEW.md deleted again": grep
  # on missing file → fail.
  review-ref =
    pkgs.runCommand "rio-review-ref"
      {
        nativeBuildInputs = [ pkgs.gawk ];
        rs = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../.)
            ../docs/REVIEW.md
          ];
        };
      }
      ''
        set -euo pipefail
        hdrs=$(grep -E '^## ' $rs/docs/REVIEW.md | sed 's/^## //; s/ /-/g' | tr 'A-Z' 'a-z')
        fail=0
        # Two trigger forms: `REVIEW.md §Word` on one line, or
        # `…REVIEW.md` at EOL followed by `// §Word` (rustfmt wraps long
        # comments — snapshot.rs:763, sla/mod.rs:154). Any non-comment
        # line resets the carry so a stray `§` after a code line doesn't
        # match.
        while IFS=: read -r file line ref; do
          ref_l=$(echo "$ref" | tr 'A-Z' 'a-z')
          if ! echo "$hdrs" | grep -q "^$ref_l"; then
            echo "FAIL: dangling REVIEW.md ref: $file:$line §$ref" >&2
            fail=1
          fi
        done < <(
          find $rs -name '*.rs' -exec gawk '
            match($0, /REVIEW\.md[[:space:]]*§([A-Za-z][A-Za-z-]*)/, m) {
              print FILENAME":"FNR":"m[1]; carry=0; next
            }
            /REVIEW\.md[[:space:]]*$/ { carry=1; next }
            carry && match($0, /^[[:space:]]*\/\/\/?[[:space:]]*§([A-Za-z][A-Za-z-]*)/, m) {
              print FILENAME":"FNR":"m[1]
            }
            { carry=0 }
          ' {} +
        )
        [[ $fail -eq 0 ]]
        touch $out
      '';
}
// {
  # Seed↔ECR-push layer-digest parity. The seed warms
  # containerd's content store; the warm only works if the
  # seed's layer digests match what push.rs puts in ECR
  # (containerd checks blobs by digest). This rebuilds the
  # same skopeo transcode push.rs would do and asserts every
  # resulting blob is present in the seed. Builds the
  # builder+fetcher images — but those are already in the
  # VM-test closure, so no extra cold-cache cost in the gate.
  executor-seed-layer-parity = dockerImages.executorSeedLayerParity;

  # Eval-only: instantiate both AMI nixosSystems so a typo in
  # the seedImages wiring (or any nixos-node module change)
  # fails the gate without building the multi-GB disk image.
  # drvPath forces full module eval; unsafeDiscardStringContext
  # prevents the drvPath's context from making this derivation
  # depend on actually BUILDING the AMI.
  node-ami-eval = pkgs.runCommand "rio-node-ami-eval" { } ''
    cat > $out <<'EOF'
    ${builtins.unsafeDiscardStringContext (nodeAmi "x86_64-linux" { }).drvPath}
    ${builtins.unsafeDiscardStringContext (nodeAmi "aarch64-linux" { }).drvPath}
    ${builtins.unsafeDiscardStringContext (nodeAmi "x86_64-linux" { efi = false; }).drvPath}
    EOF
  '';

  # /dev/{fuse,kvm} in the OCI base runtime spec must be world-rw
  # (0666). The nix sandbox build user inside the pod is unprivileged
  # and unmapped relative to the device node's owner, so the world bits
  # are the only permission class it can ever match — a group-rw node
  # is EACCES on open for every build that needs the device, and for
  # kvm that only surfaces on a .metal node running a kvm-requiring
  # derivation (no cheaper signal). Pins the fileMode of every injected
  # device in both spec variants; the vm-security-nonpriv-k3s
  # passthrough subtest proves the entries reach runc, this proves they
  # leave the spec with a usable mode.
  # rio-exec is the build-system-AGNOSTIC sandbox layer: its Cargo.toml
  # bans rio-crate deps and Nix conventions so the executor stays
  # reusable for non-Nix build systems. The dependency half is enforced
  # by Cargo itself; this check enforces the conventions half by
  # banning the tokens through which Nix-isms have historically leaked
  # in. '/nix/store' is deliberately NOT banned: tests legitimately
  # bind the host store as an opaque mount source.
  rio-exec-boundary =
    let
      rioExecSrc = pkgs.lib.fileset.toSource {
        root = unfilteredRoot;
        fileset = pkgs.lib.fileset.unions [
          ../rio-exec/src
          ../rio-exec/tests
          ../rio-exec/Cargo.toml
        ];
      };
    in
    pkgs.runCommand "rio-rio-exec-boundary" { nativeBuildInputs = [ pkgs.ripgrep ]; } ''
      status=0
      ban() {
        # $1 = fixed-string token, $2 = which leak it bans.
        if rg --fixed-strings --with-filename --line-number "$1" ${rioExecSrc}/rio-exec; then
          echo "FAIL: forbidden token '$1' in rio-exec — $2" >&2
          status=1
        fi
      }
      # The Nix sandbox user identity: lives in rio-builder's
      # nix_sandbox_identity(); the executor takes it via the mandatory
      # SandboxIdentity request field.
      ban 'nixbld' 'the build-user name is a SandboxIdentity request field'
      ban 'Nix build user' 'the GECOS field is a SandboxIdentity request field'
      # The Nix structured-log side channel: rio-builder's NixLogFilter
      # owns the protocol; the executor only carries framing metadata
      # (ExecEvent::Log.terminated).
      ban '@nix' 'the structured-log protocol belongs to rio-builder NixLogFilter'
      # Nix environment-variable conventions (NIX_LOG_FD,
      # NIX_BUILD_CORES, ...): the request env is caller-provided and
      # verbatim; the executor must not know any key.
      ban 'NIX_' 'Nix env-var conventions belong to the request glue'
      if [ "$status" -ne 0 ]; then
        echo "" >&2
        echo "rio-exec is build-system-agnostic (see its Cargo.toml ARCHITECTURAL RULE" >&2
        echo "and spec rule exec.request.identity): Nix names/conventions belong in" >&2
        echo "rio-builder's glue (nix_sandbox_identity, NixLogFilter, env assembly)," >&2
        echo "passed to the executor through the ExecutionRequest." >&2
        exit 1
      fi
      touch $out
    '';

  base-runtime-spec-devices =
    let
      baseRuntimeSpec = import ./base-runtime-spec.nix { inherit pkgs; };
    in
    pkgs.runCommand "rio-base-runtime-spec-devices" { nativeBuildInputs = [ pkgs.jq ]; } ''
      check() {
        spec=$1 want=$2
        jq -e --argjson n "$want" '
          [.linux.devices[] | select(.path == "/dev/fuse" or .path == "/dev/kvm")]
          | length == $n and all(.fileMode == 438)
        ' "$spec" >/dev/null || {
          echo "FAIL: $spec — expected $want of /dev/{fuse,kvm} in linux.devices, all fileMode=438 (0666)." >&2
          echo "      The unprivileged, unmapped sandbox build user only matches the world permission" >&2
          echo "      bits; anything narrower is EACCES on open. See nix/base-runtime-spec.nix." >&2
          jq '.linux.devices' "$spec" >&2
          exit 1
        }
      }
      check ${baseRuntimeSpec.fuseSpec} 1
      check ${baseRuntimeSpec.kvmSpec} 2
      touch $out
    '';

  # nginx allow-list (docker.nix dashboardReadonlyMethods) MUST equal
  # the Cilium Gateway rio-scheduler-readonly HTTPRoute's Exact paths.
  # Both implement r[dash.auth.method-gate+3]; before this check the
  # nginx side was a deny-list that fail-OPENED 10 mutating RPCs.
  # Diffing the two closes the drift class — adding an RPC to either
  # side without the other fails CI.
  dashboard-method-gate-parity =
    let
      chart = pkgs.lib.cleanSource ../infra/helm/rio-build;
      nginxSide = pkgs.writeText "nginx-readonly-methods" (
        pkgs.lib.concatLines dockerImages.dashboardReadonlyMethods
      );
    in
    pkgs.runCommand "rio-dashboard-method-gate-parity"
      {
        nativeBuildInputs = [
          pkgs.kubernetes-helm
          pkgs.yq-go
          pkgs.diffutils
        ];
      }
      ''
        cp -r ${chart} $TMPDIR/chart
        chmod -R +w $TMPDIR/chart
        cd $TMPDIR/chart
        mkdir -p charts
        ln -s ${subcharts.postgresql} charts/postgresql

        helm template rio . \
          --set dashboard.enabled=true \
          --set global.image.tag=test \
          --set postgresql.enabled=false \
          | yq 'select(.kind=="HTTPRoute" and .metadata.name=="rio-scheduler-readonly")
                | .spec.rules[].matches[].path.value' \
          | sort > $TMPDIR/gateway-side

        sort ${nginxSide} > $TMPDIR/nginx-side

        diff $TMPDIR/nginx-side $TMPDIR/gateway-side || {
          echo "FAIL: nginx readonly allow-list (docker.nix dashboardReadonly{Admin,Scheduler})" >&2
          echo "      diverged from rio-scheduler-readonly HTTPRoute (dashboard-gateway.yaml)." >&2
          echo "      Both implement r[dash.auth.method-gate+3] — keep them in sync." >&2
          exit 1
        }
        touch $out
      '';

  # bootstrap-job.yaml documents the script as "Idempotent". Round-15
  # made the signing-key block fail-closed; round-16 (bug_023,
  # critical) found its pub re-derive published the PRIVATE SEED for
  # 32-byte seed-only secrets — and the harness was green over it
  # because the rio-cli mock could only emit the 64-byte expanded
  # population and the aws mock was newline-blind (echo). This
  # harness is byte-faithful: the REAL rio-cli binary, printf-exact
  # secret storage, one transport newline on get-secret-value (what
  # `--output text` emits), and cmp (never string compare) for every
  # state assertion. Scenarios cover the full secret-entry population
  # the consumer (Signer::parse) accepts, not just what keygen emits.
  bootstrap-idempotent =
    pkgs.runCommand "rio-bootstrap-idempotent"
      {
        nativeBuildInputs = [
          pkgs.bash
          pkgs.diffutils
        ];
      }
      ''
        export TMPDIR=$PWD
        mkdir -p secrets bin tmp
        sh=${pkgs.bash}/bin/bash
        # Mock aws: state in $TMPDIR/secrets/<id-with-slashes-as-_>.
        # Byte-faithful: create/put store the payload with printf '%s'
        # (NO added newline — the round-15 mock's `echo` hid the
        # trailing-newline divergence merged_bug_004 found);
        # get-secret-value emits the value plus the ONE transport
        # newline `--output text` appends; file:// and fileb://
        # payloads are dereferenced byte-exact like the real CLI.
        # describe-secret → ResourceNotFoundException (exit 254) when
        # missing — the REAL exception name, because the script
        # discriminates on it. On success it emits what the script's
        # `--query DeletedDate --output text` would print: "None" for
        # a live secret, a timestamp when $TMPDIR/inject-deleted lists
        # the id (the default delete-secret only SCHEDULES deletion —
        # describe keeps succeeding for the 7-30 day recovery window
        # while get/put/create fail InvalidRequestException, which the
        # mock also reproduces so a script that wrongly proceeds past
        # a deleting verdict still dies, just with the wrong error).
        # Failure injection: if $TMPDIR/inject-describe-fail lists
        # this id, describe fails with ThrottlingException — same exit
        # code as not-found, so only stderr discrimination passes.
        # Asserts CONTROL FLOW and BYTES, not AWS semantics.
        cat > bin/aws <<EOF
        #!$sh
        sub="\$1 \$2"; id=""; payload=""
        while [ \$# -gt 0 ]; do
          case "\$1" in
            --secret-id|--name) id="\''${2//\//_}"; shift ;;
            --secret-string|--secret-binary) payload="\$2"; shift ;;
          esac; shift
        done
        f="$TMPDIR/secrets/\$id"
        case "\$payload" in
          file://*) payload=\$(cat "\''${payload#file://}") ;;
          fileb://*) payload=\$(cat "\''${payload#fileb://}") ;;
        esac
        # Mock-internal matching uses ABSOLUTE store paths: the mock
        # executes under run()'s image-confined PATH (no gnugrep), but
        # the mock is harness infrastructure — pinning its tools must
        # not widen the PATH the script under test sees.
        deleted() {
          [ -f "$TMPDIR/inject-deleted" ] && ${pkgs.gnugrep}/bin/grep -qx "\$1" "$TMPDIR/inject-deleted"
        }
        refuse_deleted() {
          if deleted "\$1"; then
            echo "An error occurred (InvalidRequestException): You can't perform this operation on the secret because it was marked for deletion." >&2
            exit 254
          fi
        }
        case "\$sub" in
          "secretsmanager describe-secret")
            if [ -f "$TMPDIR/inject-describe-fail" ] \
              && ${pkgs.gnugrep}/bin/grep -qx "\$id" "$TMPDIR/inject-describe-fail"; then
              echo "An error occurred (ThrottlingException): Rate exceeded" >&2
              exit 254
            fi
            [ -f "\$f" ] || {
              echo "An error occurred (ResourceNotFoundException): Secrets Manager can't find the specified secret." >&2
              exit 254
            }
            if deleted "\$id"; then
              echo "2026-06-03T00:00:00+00:00"
            else
              echo None
            fi ;;
          "secretsmanager create-secret")
            refuse_deleted "\$id"
            # Concurrency near-miss injection: the listed id behaves
            # as if a racing Job created it BETWEEN our describe probe
            # and this create — the exact window the create-only CAS
            # guards. Fails without writing, like the real API.
            if [ -f "$TMPDIR/inject-create-exists" ] \
              && ${pkgs.gnugrep}/bin/grep -qx "\$id" "$TMPDIR/inject-create-exists"; then
              echo "An error occurred (ResourceExistsException): The operation failed because the secret rio/signing-key already exists." >&2
              exit 254
            fi
            [ -f "\$f" ] && { echo ResourceExistsException >&2; exit 254; }
            printf '%s' "\$payload" > "\$f" ;;
          "secretsmanager put-secret-value")
            refuse_deleted "\$id"
            printf '%s' "\$payload" > "\$f" ;;
          "secretsmanager get-secret-value")
            refuse_deleted "\$id"
            cat "\$f"; echo ;;
          *) exit 0 ;;
        esac
        EOF
        for m in openssl ssh-keygen mktemp; do
          printf '#!%s\n' "$sh" > bin/$m
        done
        echo 'd=$TMPDIR/mktemp.$$.$RANDOM; mkdir -p "$d"; echo "$d"' >> bin/mktemp
        echo 'echo mock' >> bin/openssl
        cat >> bin/ssh-keygen <<EOF
        while [ \$# -gt 0 ]; do
          [ "\$1" = -f ] && { : > "\$2"; : > "\$2.pub"; }; shift
        done
        EOF
        chmod +x bin/*
        # REAL rio-cli — the byte contract under test is the binary's,
        # not a mock's reading of it.
        export PATH=$PWD/bin:${dockerImages.rioCli}/bin:${pkgs.coreutils}/bin:${pkgs.gnugrep}/bin:${pkgs.diffutils}/bin
        export AWS_REGION=x CHUNK_BUCKET=x

        # The SCRIPT runs under the production image's tool envelope
        # (nix/docker.nix bootstrap makeBinPath: awscli2/openssl/
        # openssh → mocked in $PWD/bin, rio-cli, coreutils, diffutils
        # — and NOTHING else; notably no gnugrep). Round-17
        # composition bug: this harness PATH was tool-richer than the
        # image, so a grep inside secret_state passed every scenario
        # here and died 'grep: command not found' in the real pod
        # (vm-lifecycle-prod-parity-k3s). The harness's own ASSERTIONS
        # keep the wide PATH above; only run() is confined. Keep this
        # list in lockstep with the image's makeBinPath.
        run() {
          PATH=$PWD/bin:${dockerImages.rioCli}/bin:${pkgs.coreutils}/bin:${pkgs.diffutils}/bin \
            $sh ${dockerImages.bootstrapScript}
        }
        derive() { rio-cli keygen derive-pub; }  # stdin → stdout

        # Scenario A: fresh → both halves exist, canonical, and the
        # stored pair is self-consistent (pub == derive(sec), byte-cmp
        # against the real binary's derivation; canonical entries are
        # newline-free so cmp also pins the no-trailing-newline
        # contract).
        run
        [ -f secrets/rio_signing-key ] && [ -f secrets/rio_signing-key-pub ] \
          || { echo "FAIL-A: fresh run did not create both signing-key halves" >&2; exit 1; }
        derive < secrets/rio_signing-key > tmp/a.derived
        cmp tmp/a.derived secrets/rio_signing-key-pub \
          || { echo "FAIL-A: stored pub is not the canonical derivation of the stored secret" >&2; exit 1; }

        # Scenario B (round-15 bug): private exists, pub missing →
        # converge by RE-DERIVING — never regenerate. Preservation:
        # private half byte-identical; pub byte-equal to the real
        # derivation of that exact secret.
        cp secrets/rio_signing-key tmp/b.sec-before
        rm secrets/rio_signing-key-pub
        run
        cmp secrets/rio_signing-key tmp/b.sec-before \
          || { echo "FAIL-B: pub-only retry changed the private half (rotation!)" >&2; exit 1; }
        derive < tmp/b.sec-before > tmp/b.expected
        cmp secrets/rio_signing-key-pub tmp/b.expected \
          || { echo "FAIL-B: re-derived pub diverges from the codec derivation" >&2; exit 1; }

        # Scenario C: stale pub exists, private missing → both must
        # regenerate; pub overwritten with the NEW pair's derivation
        # (private-half create-only lands FIRST; a stale pub can never
        # outlive a fresh private half).
        rm secrets/rio_signing-key
        printf '%s' OLD > secrets/rio_signing-key-pub
        run
        [ -f secrets/rio_signing-key ] \
          || { echo "FAIL-C: private not recreated" >&2; exit 1; }
        if grep -q OLD secrets/rio_signing-key-pub; then
          echo "FAIL-C: pub not overwritten (stale pair)" >&2; exit 1
        fi
        derive < secrets/rio_signing-key > tmp/c.derived
        cmp tmp/c.derived secrets/rio_signing-key-pub \
          || { echo "FAIL-C: regenerated pair not self-consistent" >&2; exit 1; }

        # Scenario D (fail-closed): TRANSIENT describe failure on the
        # live PRIVATE half (throttle — same exit code as not-found,
        # different exception) → abort, both halves byte-untouched.
        cp secrets/rio_signing-key tmp/d.sec
        cp secrets/rio_signing-key-pub tmp/d.pub
        echo rio_signing-key > $TMPDIR/inject-describe-fail
        if run 2>$TMPDIR/d-stderr; then
          echo "FAIL-D: Job exited 0 despite a transient describe failure" >&2; exit 1
        fi
        rm $TMPDIR/inject-describe-fail
        grep -q "refusing to guess" $TMPDIR/d-stderr \
          || { echo "FAIL-D: abort did not come from the fail-closed probe" >&2; cat $TMPDIR/d-stderr >&2; exit 1; }
        cmp secrets/rio_signing-key tmp/d.sec && cmp secrets/rio_signing-key-pub tmp/d.pub \
          || { echo "FAIL-D: transient describe failure mutated signing-key state" >&2; exit 1; }

        # Scenario D2: same, but the throttle hits the PUB half's
        # describe — the pair probe must abort before ANY branch runs
        # (the round-15 harness only injected on the private half).
        echo rio_signing-key-pub > $TMPDIR/inject-describe-fail
        if run 2>$TMPDIR/d2-stderr; then
          echo "FAIL-D2: Job exited 0 despite a transient pub-describe failure" >&2; exit 1
        fi
        rm $TMPDIR/inject-describe-fail
        grep -q "refusing to guess" $TMPDIR/d2-stderr \
          || { echo "FAIL-D2: abort did not come from the fail-closed probe" >&2; exit 1; }
        cmp secrets/rio_signing-key tmp/d.sec && cmp secrets/rio_signing-key-pub tmp/d.pub \
          || { echo "FAIL-D2: pub-describe failure mutated signing-key state" >&2; exit 1; }

        # Scenario E (bug_023, the critical arm): a 32-byte SEED-ONLY
        # secret (BYO import — a population Signer::parse accepts and
        # the old shell tail-surgery published verbatim). The re-derived
        # pub must be the DERIVED public key: byte-equal to the codec
        # derivation and never containing the seed's base64 anywhere.
        rm secrets/rio_signing-key secrets/rio_signing-key-pub
        seed_b64=$(head -c 32 /dev/urandom | base64 -w0)
        printf '%s' "rio-byo:$seed_b64" > secrets/rio_signing-key
        run > tmp/e.log
        derive < secrets/rio_signing-key > tmp/e.expected
        cmp secrets/rio_signing-key-pub tmp/e.expected \
          || { echo "FAIL-E: seed-only re-derive is not the codec derivation" >&2; exit 1; }
        if grep -qF "$seed_b64" secrets/rio_signing-key-pub; then
          echo "FAIL-E: PRIVATE SEED published in rio/signing-key-pub (bug_023)" >&2; exit 1
        fi
        if grep -qF "$seed_b64" tmp/e.log; then
          echo "FAIL-E: PRIVATE SEED leaked into the Job log (bug_023)" >&2; exit 1
        fi

        # Scenario F (the 64-byte stale-tail arm): an expanded entry
        # whose tail is NOT derive(seed) is internally inconsistent —
        # the re-derive must ABORT with nothing published (the old
        # surgery published the stale tail as the "public key").
        rm secrets/rio_signing-key secrets/rio_signing-key-pub
        { head -c 32 /dev/urandom; head -c 32 /dev/zero | tr '\0' Z; } > tmp/f.bin
        printf '%s' "rio-stale:$(base64 -w0 < tmp/f.bin)" > secrets/rio_signing-key
        if run 2>$TMPDIR/f-stderr; then
          echo "FAIL-F: Job exited 0 on an internally inconsistent secret" >&2; exit 1
        fi
        grep -qi "inconsistent" $TMPDIR/f-stderr \
          || { echo "FAIL-F: abort did not name the inconsistency" >&2; cat $TMPDIR/f-stderr >&2; exit 1; }
        [ ! -f secrets/rio_signing-key-pub ] \
          || { echo "FAIL-F: something was published from a corrupt secret" >&2; exit 1; }

        # Scenario G: corrupt secret (48 bytes — neither population
        # member) → abort, nothing published.
        printf '%s' "rio-bad:$(head -c 48 /dev/urandom | base64 -w0)" > secrets/rio_signing-key
        if run 2>$TMPDIR/g-stderr; then
          echo "FAIL-G: Job exited 0 on a malformed secret" >&2; exit 1
        fi
        [ ! -f secrets/rio_signing-key-pub ] \
          || { echo "FAIL-G: something was published from a malformed secret" >&2; exit 1; }

        # Scenario I: both present but MISMATCHED → self-heal converges
        # the pub onto derive(sec) without touching the private half;
        # then the steady state logs the pair-consistency line (the
        # runbook's per-upgrade probe) and changes nothing.
        rm secrets/rio_signing-key
        run > /dev/null   # mint a fresh consistent pair
        cp secrets/rio_signing-key tmp/i.sec
        printf '%s' "rio-junk:AAAA" > secrets/rio_signing-key-pub
        run > tmp/i.log
        grep -q "healing" tmp/i.log \
          || { echo "FAIL-I: mismatch not detected as a heal" >&2; exit 1; }
        cmp secrets/rio_signing-key tmp/i.sec \
          || { echo "FAIL-I: heal touched the private half" >&2; exit 1; }
        derive < tmp/i.sec > tmp/i.expected
        cmp secrets/rio_signing-key-pub tmp/i.expected \
          || { echo "FAIL-I: heal did not converge pub to derive(sec)" >&2; exit 1; }
        run > tmp/i2.log
        grep -q "signing-key pair consistent" tmp/i2.log \
          || { echo "FAIL-I: steady state did not log the pair-consistency probe" >&2; exit 1; }
        grep -q "healing" tmp/i2.log \
          && { echo "FAIL-I: consistent pair triggered a heal" >&2; exit 1; }
        cmp secrets/rio_signing-key-pub tmp/i.expected && cmp secrets/rio_signing-key tmp/i.sec \
          || { echo "FAIL-I: steady-state run mutated the pair" >&2; exit 1; }

        # Scenario H (concurrency near-miss, R3): two Jobs race the
        # GENERATE branch. The loser's private-half create-secret hits
        # ResourceExistsException in the window between its describe
        # probe ("missing") and its create — the create-only CAS. The
        # loser must abort having written NOTHING (with the old
        # pub-first order it had already clobbered the winner's pub
        # with a keypair about to be discarded, merged_bug_015); its
        # retry then converges through the re-derive branch against
        # the winner's pair.
        rm -f secrets/rio_signing-key secrets/rio_signing-key-pub
        echo rio_signing-key > $TMPDIR/inject-create-exists
        if run 2>$TMPDIR/h-stderr; then
          echo "FAIL-H: losing Job exited 0 despite losing the create CAS" >&2; exit 1
        fi
        rm $TMPDIR/inject-create-exists
        [ ! -f secrets/rio_signing-key-pub ] \
          || { echo "FAIL-H: losing Job wrote the pub half (clobbers the winner)" >&2; exit 1; }
        [ ! -f secrets/rio_signing-key ] \
          || { echo "FAIL-H: losing Job wrote the private half despite the CAS" >&2; exit 1; }
        # Winner's pair lands (a clean run plays the winner)...
        run > /dev/null
        cp secrets/rio_signing-key tmp/h.sec
        cp secrets/rio_signing-key-pub tmp/h.pub
        # ...and the loser's RETRY converges without touching it.
        run > tmp/h.log
        grep -q "signing-key pair consistent" tmp/h.log \
          || { echo "FAIL-H: loser retry did not converge on the winner's pair" >&2; exit 1; }
        cmp secrets/rio_signing-key tmp/h.sec && cmp secrets/rio_signing-key-pub tmp/h.pub \
          || { echo "FAIL-H: loser retry mutated the winner's pair" >&2; exit 1; }

        # Scenarios J-M (round-17 bug_097): the provider's FOURTH
        # state. The default delete-secret only SCHEDULES deletion —
        # describe succeeds (DeletedDate set) for the whole 7-30 day
        # recovery window while every read/write fails
        # InvalidRequestException. The probe must abort NAMING the two
        # operator exits (restore-secret / force-delete) — never
        # classify present (the round-17 wedge: every retry dies at
        # get-secret-value for up to 30 days) nor missing (create
        # would wedge identically).
        deletion_abort() {
          # $1 = scenario tag, $2 = stderr file
          grep -q "scheduled for deletion" "$2" \
            || { echo "FAIL-$1: abort did not come from the deletion arm" >&2; cat "$2" >&2; exit 1; }
          grep -q "restore-secret" "$2" \
            || { echo "FAIL-$1: remediation does not name restore-secret" >&2; exit 1; }
          grep -q -- "--force-delete-without-recovery" "$2" \
            || { echo "FAIL-$1: remediation does not name --force-delete-without-recovery" >&2; exit 1; }
        }

        # Scenario J: PRIVATE half scheduled for deletion → abort with
        # remediation; both halves byte-untouched (no heal, no put —
        # delete-only rotation converges through the operator exit,
        # not through a 30-day retry wedge).
        cp secrets/rio_signing-key tmp/j.sec
        cp secrets/rio_signing-key-pub tmp/j.pub
        echo rio_signing-key > $TMPDIR/inject-deleted
        if run 2>$TMPDIR/j-stderr; then
          echo "FAIL-J: Job exited 0 with the private half scheduled for deletion" >&2; exit 1
        fi
        deletion_abort J $TMPDIR/j-stderr
        rm $TMPDIR/inject-deleted
        cmp secrets/rio_signing-key tmp/j.sec && cmp secrets/rio_signing-key-pub tmp/j.pub \
          || { echo "FAIL-J: deletion-window abort mutated signing-key state" >&2; exit 1; }

        # Scenario K: PUB half scheduled for deletion (private live) →
        # the pair probe aborts BEFORE any branch runs; a heal put
        # would die InvalidRequestException with a raw AWS error
        # instead of the remediation (deletion_abort discriminates).
        echo rio_signing-key-pub > $TMPDIR/inject-deleted
        if run 2>$TMPDIR/k-stderr; then
          echo "FAIL-K: Job exited 0 with the pub half scheduled for deletion" >&2; exit 1
        fi
        deletion_abort K $TMPDIR/k-stderr
        rm $TMPDIR/inject-deleted
        cmp secrets/rio_signing-key tmp/j.sec && cmp secrets/rio_signing-key-pub tmp/j.pub \
          || { echo "FAIL-K: pub deletion-window abort mutated signing-key state" >&2; exit 1; }

        # Scenario L: a CREATE-ONLY secret (rio/hmac, the first guard)
        # scheduled for deletion → abort with remediation before ANY
        # later block runs; nothing anywhere is mutated.
        cp secrets/rio_hmac tmp/l.hmac
        echo rio_hmac > $TMPDIR/inject-deleted
        if run 2>$TMPDIR/l-stderr; then
          echo "FAIL-L: Job exited 0 with rio/hmac scheduled for deletion" >&2; exit 1
        fi
        deletion_abort L $TMPDIR/l-stderr
        rm $TMPDIR/inject-deleted
        cmp secrets/rio_hmac tmp/l.hmac \
          || { echo "FAIL-L: deletion-window abort mutated rio/hmac" >&2; exit 1; }
        cmp secrets/rio_signing-key tmp/j.sec && cmp secrets/rio_signing-key-pub tmp/j.pub \
          || { echo "FAIL-L: hmac deletion abort reached the signing-key block" >&2; exit 1; }

        # Scenario M: TRANSIENT throttle on a create-only guard. The
        # pre-r17 raw `if aws describe-secret` guards classified ANY
        # failure as missing — a throttled probe on a LIVE secret then
        # attempted create (ResourceExistsException, wrong error) and
        # a throttled probe on a missing one minted under a blip. All
        # five guards now route through the fail-closed probe.
        echo rio_hmac > $TMPDIR/inject-describe-fail
        if run 2>$TMPDIR/m-stderr; then
          echo "FAIL-M: Job exited 0 despite a transient hmac-describe failure" >&2; exit 1
        fi
        rm $TMPDIR/inject-describe-fail
        grep -q "refusing to guess" $TMPDIR/m-stderr \
          || { echo "FAIL-M: hmac guard abort did not come from the fail-closed probe" >&2; cat $TMPDIR/m-stderr >&2; exit 1; }
        cmp secrets/rio_hmac tmp/l.hmac \
          || { echo "FAIL-M: transient hmac-describe failure mutated rio/hmac" >&2; exit 1; }

        # Scenario N (round-17 bug_006): a trailing-newline-corrupted
        # stored pub — the round-16 merged_bug_004 class the heal
        # exists for — written DIRECTLY into the provider state
        # (bypassing the byte-faithful put path; the corruption
        # entered via the legacy shell re-derive, not via this
        # script). The probe must DETECT it (the old normalization
        # stripped all trailing newlines via command substitution, so
        # 'entry\n' compared equal and logged 'pair consistent'
        # forever) and heal the stored pub back to canonical
        # newline-free bytes.
        derive < secrets/rio_signing-key > tmp/n.canonical
        { cat tmp/n.canonical; echo; } > secrets/rio_signing-key-pub
        run > tmp/n.log
        grep -q "healing" tmp/n.log \
          || { echo "FAIL-N: newline-corrupted pub logged 'pair consistent' (bug_006)" >&2; cat tmp/n.log >&2; exit 1; }
        cmp secrets/rio_signing-key-pub tmp/n.canonical \
          || { echo "FAIL-N: heal did not converge the pub to canonical newline-free bytes" >&2; exit 1; }
        run > tmp/n2.log
        grep -q "signing-key pair consistent" tmp/n2.log \
          || { echo "FAIL-N: healed pub did not settle to the consistent steady state" >&2; exit 1; }
        touch $out
      '';

  # NAR class-cap conformance (round-17 bug_030 / RC17-01): the
  # round-16 cap consolidation missed the scheduler dispatch fetch
  # because nothing made "every derivation-text fetch site uses the
  # shared bound" mechanically checkable. The NarSizeCap type seal
  # makes a raw-u64 fetch param a compile error; this check covers the
  # residue the type system cannot reach: (1) new private NAR-cap
  # consts shadowing the class caps, (2) the sealed signatures
  # regressing to raw u64, (3) the constructor call-site registry
  # (set-equality: a NEW fetch site must register here with its class
  # reasoning, a REMOVED one must deregister).
  #
  # This is the round-17 reference deny-table implementation
  # (plan §E.4): pattern + count-pinned per-file carve-outs with
  # reason comments + remediation-naming failure messages. Later
  # conformance checks copy this shape.
  drv-cap-conformance =
    pkgs.runCommand "rio-drv-cap-conformance"
      {
        nativeBuildInputs = [ pkgs.ripgrep ];
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-common/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-proto/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-scheduler/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-gateway/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-builder/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-store/src)
          ];
        };
      }
      ''
        cd $src
        fail=0

        # ── 1. Deny-table: NAR/DRV size-cap consts. The ONLY minting
        # site is rio-common/src/limits.rs; every other match needs a
        # count-pinned carve-out naming a DISTINCT mechanism (not a
        # transfer cap). Format: path:expected-count:reason.
        denylist_carveouts() {
          cat <<'TABLE'
        rio-common/src/limits.rs:4:the owner — MAX_NAR_SIZE, MAX_NARINFO_BYTES, MAX_DRV_CONTENT_BYTES, MAX_DRV_NAR_BYTES
        rio-gateway/src/translate.rs:2:inline-DAG size bounds (64 KiB inline + fallback alias of the shared const), not transfer caps
        rio-proto/src/client/store.rs:1:NAR_CHUNK_SIZE is the stream chunking unit, not a cap
        rio-store/src/substitute.rs:2:decompression bomb bound (prod aliases MAX_NAR_SIZE; cfg(test) dual) — substitution-ingest admission caps are W1-S4 scope
        rio-store/src/metadata/drv_modulo.rs:1:chunk-reassembly arena bound, a distinct R4 budget
        TABLE
        }
        pattern='const [A-Z_]*(DRV|NAR)[A-Z_]*(SIZE|BYTES|CAP)[A-Z_]*[[:space:]]*:[[:space:]]*(u64|usize)'
        actual=$(rg -n "$pattern" --type rust . | sed 's|^\./||' | sort)
        # Per-file expected counts from the table:
        while IFS=: read -r file expected reason; do
          file=$(echo "$file" | tr -d ' ')
          [ -n "$file" ] || continue
          got=$(echo "$actual" | grep -c "^$file:" || true)
          if [ "$got" != "$expected" ]; then
            echo "FAIL: $file has $got NAR/DRV-cap const matches, deny-table pins $expected ($reason)." >&2
            echo "  A NEW size-cap const must live in rio-common/src/limits.rs (or, if it is" >&2
            echo "  genuinely a distinct mechanism, add a count-pinned carve-out with its reason" >&2
            echo "  to denylist_carveouts in nix/misc-checks.nix drv-cap-conformance)." >&2
            fail=1
          fi
        done < <(denylist_carveouts | sed 's/^[[:space:]]*//')
        # Files NOT in the table must have zero matches:
        table_files=$(denylist_carveouts | sed 's/^[[:space:]]*//' | cut -d: -f1 | tr -d ' ')
        echo "$actual" | cut -d: -f1 | sort -u | while read -r f; do
          [ -n "$f" ] || continue
          echo "$table_files" | grep -qx "$f" || {
            echo "FAIL: $f mints a NAR/DRV size-cap const outside rio-common/src/limits.rs:" >&2
            echo "$actual" | grep "^$f:" >&2
            echo "  Use rio_common::limits::NarSizeCap (round-17 bug_030: a private cap is the" >&2
            echo "  unmigrated-sibling signature) or register a carve-out with its reason." >&2
            exit 1
          }
        done || fail=1

        # ── 2. The sealed fetch signatures must never regress to raw
        # u64. Carve-out: the two INTERNAL collectors keep max_size: u64
        # (the byte mechanic beneath the seal), pinned at exactly 2.
        raw_param=$(rg -c 'max_nar_size: u64' rio-proto/src/client/store.rs || true)
        if [ "''${raw_param:-0}" != "0" ]; then
          echo "FAIL: get_path_nar/get_path_nar_to_file take max_nar_size: u64 again —" >&2
          echo "  the param must be rio_common::limits::NarSizeCap (the class-cap seal)." >&2
          fail=1
        fi
        internal=$(rg -c 'max_size: u64' rio-proto/src/client/store.rs || true)
        if [ "''${internal:-0}" != "2" ]; then
          echo "FAIL: expected exactly 2 internal 'max_size: u64' collector params in" >&2
          echo "  rio-proto/src/client/store.rs, found ''${internal:-0}. If a collector was" >&2
          echo "  added/removed, update this pin; new EXTERNAL fetch APIs take NarSizeCap." >&2
          fail=1
        fi

        # ── 3. Constructor call-site registry (set-equality): every
        # NarSizeCap mint outside rio-common is a fetch site that chose
        # a class. Adding a fetch site = register it here with its
        # class reasoning; removing one = deregister.
        registry() {
          cat <<'TABLE'
        rio-scheduler/src/actor/dispatch.rs:derivation:1:claims/CA-resolve .drv fetch (bug_030 site)
        rio-builder/src/executor/inputs.rs:derivation:1:worker glue-table .drv text fetch
        rio-gateway/src/drv_cache.rs:derivation:2:BFS .drv resolution (cold + warm paths)
        rio-builder/src/fuse/fetch/mod.rs:general:1:FUSE input materialization (arbitrary paths)
        rio-gateway/src/handler/opcodes_read.rs:general:1:client NAR download (arbitrary paths; stored .drvs already admission-bounded)
        TABLE
        }
        while IFS=: read -r file class expected reason; do
          file=$(echo "$file" | tr -d ' ')
          [ -n "$file" ] || continue
          got=$(rg -c "NarSizeCap::$class\(\)" "$file" || true)
          if [ "''${got:-0}" != "$expected" ]; then
            echo "FAIL: $file has ''${got:-0} NarSizeCap::$class() call(s), registry pins $expected ($reason)." >&2
            echo "  New fetch site? Add it to the registry in nix/misc-checks.nix" >&2
            echo "  drv-cap-conformance with its class reasoning. Removed one? Deregister it." >&2
            fail=1
          fi
        done < <(registry | sed 's/^[[:space:]]*//')
        # Mints outside the registry (and outside the owning crate):
        rg -ln 'NarSizeCap::(derivation|general|for_path_class)\(' --type rust .           | sed 's|^\./||' | grep -v '^rio-common/src/limits.rs$' | sort -u | while read -r f; do
          registry | sed 's/^[[:space:]]*//' | cut -d: -f1 | tr -d ' ' | grep -qx "$f" || {
            # Comments/docs may NAME the constructors; only CALLS count.
            calls=$(rg -n 'NarSizeCap::(derivation|general|for_path_class)\(\)' "$f" | rg -v '^\s*\d+:\s*//' | wc -l)
            [ "$calls" = "0" ] || {
              echo "FAIL: $f mints a NarSizeCap outside the call-site registry:" >&2
              rg -n 'NarSizeCap::(derivation|general|for_path_class)\(\)' "$f" >&2
              echo "  Register it in nix/misc-checks.nix drv-cap-conformance with its class reasoning." >&2
              exit 1
            }
          }
        done || fail=1

        [ "$fail" = 0 ] || exit 1
        touch $out
      '';

  # The bootstrap script's AWS calls and the IRSA policy in
  # infra/eks/secrets.tf must agree on (action, resource) PAIRS, not
  # just action names. Round-16's confinements introduced a resource
  # axis (GetSecretValue → rio/signing-key*; PutSecretValue →
  # rio/signing-key-pub, round-17 merged_bug_013) that an action-name
  # gate is structurally blind to (round-17 merged_bug_004): a future
  # script edit reading rio/hmac would have passed the old gate and
  # AccessDenied'd at runtime — the precise drift this gate's header
  # promises cannot happen. For every action the script uses, the
  # EXPECTED Resource is DERIVED from the script's own per-verb target
  # set (longest common prefix + '*', which also matches Secrets
  # Manager's random ARN suffix); the granting statement's resource
  # must equal it exactly — too wide is an over-grant, too narrow is
  # a runtime AccessDenied. The gate self-calibrates against three
  # negative fixtures so its own parsing cannot silently rot.
  bootstrap-iam-parity =
    pkgs.runCommand "rio-bootstrap-iam-parity"
      {
        script = ../nix/bootstrap-job.sh;
        policy = ../infra/eks/secrets.tf;
      }
      ''
        parity() {
          s=$1; p=$2
          block=$(sed -n '/resource "aws_iam_policy" "rio_bootstrap"/,/^}/p' "$p")
          [ -n "$block" ] || { echo "FAIL: rio_bootstrap policy block not found"; return 1; }

          # Script (verb, target) pairs: literal --secret-id/--name
          # args of executed calls, plus secret_state callsites for
          # describe-secret (the probe takes the id as an argument;
          # it is the script's only describe site, pinned by
          # bootstrap-probe-conformance). Remediation prose is immune
          # by construction: its verbs ride as printf arguments.
          pairs=$(grep -oE 'aws secretsmanager [a-z-]+ +(--secret-id|--name) +rio/[a-z-]+' "$s" \
            | awk '{print $3" "$5}')
          probes=$(grep -oE '\$\(secret_state +rio/[a-z-]+\)' "$s" \
            | grep -oE 'rio/[a-z-]+' | sed 's/^/describe-secret /')
          pairs=$(printf '%s\n%s\n' "$pairs" "$probes" | sort -u | grep .)
          [ -n "$pairs" ] || { echo "FAIL: no aws secretsmanager calls found in script?"; return 1; }
          verbs=$(printf '%s\n' "$pairs" | awk '{print $1}' | sort -u)

          # Statement table: lines tagged by statement index.
          stmts=$(printf '%s\n' "$block" | awk '/Effect = "Allow"/{n++} n>0{print n"\t"$0}')
          nmax=$(printf '%s\n' "$stmts" | awk -F'\t' 'END{print $1+0}')
          fail=0
          for v in $verbs; do
            action=$(echo "$v" | sed -E 's/(^|-)([a-z])/\U\2/g')
            targets=$(printf '%s\n' "$pairs" | awk -v v="$v" '$1==v{print $2}' | sort -u)
            expected=$(printf '%s\n' $targets | awk '
              NR==1{p=$0; next}
              {while (length(p)>0 && substr($0,1,length(p))!=p) p=substr(p,1,length(p)-1)}
              END{print p"*"}')
            grants=""
            i=1
            while [ "$i" -le "$nmax" ]; do
              printf '%s\n' "$stmts" | awk -F'\t' -v i="$i" '$1==i{print $2}' \
                | grep -q "secretsmanager:$action" && grants="$grants $i"
              i=$((i+1))
            done
            ng=$(echo $grants | wc -w)
            if [ "$ng" = 0 ]; then
              echo "FAIL: script uses 'aws secretsmanager $v' on {$(echo $targets | tr ' ' ',')}"
              echo "      but no rio_bootstrap statement grants secretsmanager:$action"
              fail=1; continue
            fi
            if [ "$ng" != 1 ]; then
              echo "FAIL: secretsmanager:$action is granted by $ng statements ($grants);"
              echo "      one statement per action keeps the resource axis auditable"
              fail=1; continue
            fi
            res=$(printf '%s\n' "$stmts" | awk -F'\t' -v i="$(echo $grants)" '$1==i{print $2}' \
              | grep -oE ':secret:[^"]*' | sed 's/^:secret://' | sort -u)
            if [ "$(printf '%s\n' "$res" | wc -l)" != 1 ] || [ -z "$res" ]; then
              echo "FAIL: statement$grants (secretsmanager:$action) needs exactly one secret:* resource"
              fail=1; continue
            fi
            if [ "$res" != "$expected" ]; then
              echo "FAIL: secretsmanager:$action is granted on 'secret:$res' but the script's"
              echo "      target set {$(echo $targets | tr ' ' ',')} derives 'secret:$expected'."
              echo "      Too wide = over-grant (merged_bug_013); too narrow = runtime"
              echo "      AccessDenied the gate exists to prevent (merged_bug_004)."
              fail=1
            fi
          done
          # Reverse direction: every granted action is exercised by
          # the script (no silent over-grant).
          granted=$(printf '%s\n' "$block" | grep -oE 'secretsmanager:[A-Za-z]+' | cut -d: -f2 | sort -u)
          for a in $granted; do
            verb=$(echo "$a" | sed -E 's/([A-Z])/-\L\1/g; s/^-//')
            echo "$verbs" | grep -qx "$verb" || {
              echo "FAIL: IAM grants secretsmanager:$a but the script never executes 'aws secretsmanager $verb'"
              fail=1
            }
          done
          return $fail
        }

        parity $script $policy || { echo "FAIL: live script/policy pair failed parity" >&2; exit 1; }

        # Self-calibration: three negative fixtures that the round-17
        # findings prove this gate MUST catch. Each mutation must turn
        # the gate red, or the parsing has rotted.
        cat $script > f1.sh
        printf 'aws secretsmanager get-secret-value --secret-id rio/hmac --query SecretString --output text > /dev/null\n' >> f1.sh
        if parity f1.sh $policy >/dev/null 2>&1; then
          echo "FAIL: fixture-1 (script reads rio/hmac) passed — the resource axis is not enforced (merged_bug_004)" >&2
          exit 1
        fi
        sed 's|rio/signing-key-pub\*|rio/*|' $policy > f2.tf
        if parity $script f2.tf >/dev/null 2>&1; then
          echo "FAIL: fixture-2 (PutSecretValue widened to rio/*) passed — over-grants are invisible (merged_bug_013)" >&2
          exit 1
        fi
        grep -v 'secretsmanager:GetSecretValue' $policy > f3.tf
        if parity $script f3.tf >/dev/null 2>&1; then
          echo "FAIL: fixture-3 (GetSecretValue grant removed) passed — missing grants are invisible" >&2
          exit 1
        fi
        touch $out
      '';

  # r17 merged_bug_003 (RC17-14): the hashedMirrors admission pattern
  # is single-sourced in rio-crds — the distinctive accept-set class
  # bytes may appear ONLY in the defining file (exactly once: the
  # macro body) and the generated CRD YAML. A copy anywhere else
  # re-creates the three-spellings drift this check exists to kill;
  # consumers import rio_crds::pool::{HASHED_MIRROR_URL_PATTERN,
  # hashed_mirror_entry_admissible} instead. Fixed-string grep — the
  # pattern bytes contain no quote or backslash by design, so -F
  # needs no escaping at any quoting layer.
  mirror-pattern-single-source =
    pkgs.runCommand "rio-mirror-pattern-single-source"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            workspaceFileset
            ../docs/spec
            ../nix
            ../infra
            ../.config
          ];
        };
      }
      ''
        cd $src
        # The needle is assembled from two halves so THIS file never
        # contains the contiguous pattern bytes (the check would
        # otherwise flag itself).
        needle='[-!-+'
        needle="$needle"'.-~]'
        hits=$(grep -rlF -- "$needle" . | LC_ALL=C sort)
        allowed=$(printf '%s\n' ./infra/helm/crds/pools.rio.build.yaml ./rio-crds/src/pool.rs)
        if [ "$hits" != "$allowed" ]; then
          echo "FAIL: hashedMirrors accept-set pattern bytes found outside the single source:" >&2
          echo "$hits" >&2
          echo "remediation: import rio_crds::pool::{HASHED_MIRROR_URL_PATTERN, hashed_mirror_entry_admissible}" >&2
          echo "(allowlist lives in nix/misc-checks.nix mirror-pattern-single-source)" >&2
          exit 1
        fi
        n=$(grep -cF -- "$needle" rio-crds/src/pool.rs)
        if [ "$n" != "1" ]; then
          echo "FAIL: expected exactly 1 pattern literal in rio-crds/src/pool.rs (the macro body), found $n" >&2
          echo "build derived strings from hashed_mirror_url_pattern!/HASHED_MIRROR_URL_PATTERN, never a second literal" >&2
          exit 1
        fi
        touch $out
      '';

  # Round-17 bug_097 (RC17-02 mechanism): secret_state() is the ONLY
  # legal Secrets Manager existence probe in the bootstrap Job. Raw
  # `if aws secretsmanager describe-secret ...` guards collapse the
  # provider's four states to two — a throttle classifies as missing
  # (round-15's transient class) and a secret in its 7-30 day deletion
  # recovery window classifies as present (round-17's wedge: every
  # retry dies at the first read/write until the window elapses).
  # Deny-table: pattern `secretsmanager describe-secret`, count-pinned
  # carve-out of exactly ONE site (inside secret_state). The check
  # red-tests its own deny logic against a scratch mutation so the
  # pattern cannot silently rot.
  bootstrap-probe-conformance =
    pkgs.runCommand "rio-bootstrap-probe-conformance"
      {
        script = ../nix/bootstrap-job.sh;
      }
      ''
        count() { grep -c 'secretsmanager describe-secret' "$1" || true; }

        grep -q 'secret_state()' $script || {
          echo "FAIL: secret_state() definition missing from bootstrap-job.sh —" >&2
          echo "      it is the sole legal existence probe (r[infra.bootstrap.secret-state-probe])." >&2
          exit 1
        }
        n=$(count $script)
        [ "$n" = 1 ] || {
          echo "FAIL: expected exactly 1 'secretsmanager describe-secret' call site in" >&2
          echo "      bootstrap-job.sh (the one inside secret_state); found $n." >&2
          echo "      Every existence decision must route through secret_state() — the only" >&2
          echo "      probe discriminating present/missing/scheduled-for-deletion/transient." >&2
          echo "      Use the assignment-then-test form: state=\$(secret_state rio/<name>)." >&2
          exit 1
        }

        # Self-calibration: the deny pattern must actually fire on the
        # raw-guard form, or this gate is green over the very drift it
        # exists to stop. (cat, not cp: the store path is read-only.)
        cat $script > scratch.sh
        printf '\nif aws secretsmanager describe-secret --secret-id rio/extra >/dev/null 2>&1; then :; fi\n' >> scratch.sh
        n2=$(count scratch.sh)
        [ "$n2" = 2 ] || {
          echo "FAIL: self-calibration — injected raw describe-secret guard was not counted" >&2
          echo "      (got $n2, want 2); the deny pattern has rotted." >&2
          exit 1
        }
        touch $out
      '';

  # Round-17 RC17-05 c3 (merged_bug_063): NAR residency is sealed
  # behind the admission witness (`ingest::AdmittedNar` — class cap +
  # .drv text-CA binding + single preimage extraction). The persistence
  # primitives take the witness TYPE, so an unguarded route is a
  # compile error; this check is the CI half of the seal — it denies
  # the RAW persistence/extraction forms outside the sealed layer, so
  # a future "convenience" overload or a copy-pasted extraction cannot
  # silently re-open a fourth route. Deny-table shape: pattern +
  # count-pinned per-file carve-outs, each with the reason it is
  # allowed; counts are exact so a NEW site fails even in an allowed
  # file (the remediation is in the failure message).
  store-ingest-conformance =
    pkgs.runCommand "rio-store-ingest-conformance"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../rio-store;
          fileset = ../rio-store/src;
        };
      }
      ''
        cd $src
        fail=0
        # check PATTERN FILE EXPECTED REASON
        check() {
          local pat="$1" file="$2" want="$3"
          local got
          got=$(grep -c -F "$pat" "$file" 2>/dev/null || true)
          if [ "''${got:-0}" -ne "$want" ]; then
            echo "FAIL: $file has $got occurrences of '$pat' (pinned: $want)." >&2
            echo "  New NAR persistence/extraction sites must go through ingest::AdmittedNar" >&2
            echo "  (the admission witness) — see store.put.drv-text-ca and" >&2
            echo "  nix/misc-checks.nix store-ingest-conformance for the carve-out registry." >&2
            fail=1
          fi
        }
        # Total-count check across the tree for a pattern: the sum of
        # the carve-outs below. A site in a NEW file fails here.
        total() {
          local pat="$1" want="$2"
          local got
          got=$(grep -r -c -F "$pat" src --include="*.rs" | awk -F: '{n+=$2} END {print n+0}')
          if [ "$got" -ne "$want" ]; then
            echo "FAIL: rio-store/src has $got total occurrences of '$pat' (pinned: $want)." >&2
            echo "  New NAR persistence/extraction sites must take ingest::AdmittedNar." >&2
            fail=1
          fi
        }
        # -- extract_single_file: ingest routes extract ONCE inside the
        #    witness. Sole carve-out: the proof-walk READ of an
        #    already-admitted resident manifest (not an ingest route).
        check "extract_single_file(" src/metadata/drv_modulo.rs 1
        total "extract_single_file(" 1
        # -- complete_manifest_inline: definition (metadata/inline.rs),
        #    the sealed call in ingest::persist_nar, plus test fixtures
        #    (metadata/mod.rs x4, grpc/sign.rs x6 — all under
        #    #[cfg(test)]; they construct rows directly to test
        #    metadata-layer invariants, not ingest routes).
        check "complete_manifest_inline(" src/metadata/inline.rs 1
        check "complete_manifest_inline(" src/ingest.rs 1
        check "complete_manifest_inline(" src/metadata/mod.rs 4
        check "complete_manifest_inline(" src/grpc/sign.rs 6
        total "complete_manifest_inline(" 12
        # -- chunked staging/persist: definitions + internal call in
        #    cas.rs (witness-typed), the sealed put_chunked call in
        #    ingest.rs, and the witness-typed batch staging wrapper.
        check "stage_chunked(" src/cas.rs 2
        check "stage_chunked(" src/grpc/put_path/common.rs 1
        total "stage_chunked(" 3
        check "put_chunked(" src/cas.rs 2
        check "put_chunked(" src/ingest.rs 1
        total "put_chunked(" 3
        # -- batch atomic completion: definition chain in metadata
        #    (mod.rs dispatcher, inline.rs + chunked.rs arms) + the one
        #    batch commit site (bytes witness-derived via NarPersist).
        check "complete_manifest_in_conn(" src/grpc/put_path_batch.rs 1
        check "complete_manifest_in_conn(" src/metadata/chunked.rs 1
        check "complete_manifest_in_conn(" src/metadata/inline.rs 1
        check "complete_manifest_in_conn(" src/metadata/mod.rs 1
        total "complete_manifest_in_conn(" 4
        [ "$fail" = 0 ] || exit 1
        touch $out
      '';

  # Migration-completeness conformance for the signing_keyfmt codec
  # (round-17 RC17-11, bet consequence: adoption-incomplete ⇒ every
  # owner chokepoint gains a CI artifact that makes the NEXT missed
  # sibling a build failure, not a review hope).
  #
  # The codec owns the `name:base64` key-entry byte contract. Every
  # raw base64 use in the registered key-handling files is
  # count-pinned with a reason; a NEW site (hand-rolled parser or
  # encoder) drifts a count and fails this check, naming the
  # remediation. Signature bytes (sign/verify loops) are a different
  # byte contract and stay raw — they are part of each file's pinned
  # count, classified in the reason column.
  single-source-conformance =
    pkgs.runCommand "rio-single-source-conformance"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            ../rio-common/src/signing_keyfmt.rs
            ../rio-store/src/signing.rs
            ../rio-store/src/grpc/admin.rs
            ../rio-store/src/grpc/sign.rs
            ../rio-store/src/metadata/tenant_keys.rs
            ../rio-store/src/metadata/cluster_key_history.rs
            ../rio-store/src/substitute.rs
            ../rio-cli/src/keygen.rs
            ../nix/bootstrap-job.sh
            # outputHashAlgo strip-prefix table (carve-outs + must-stay-clean):
            ../rio-nix/src/hash.rs
            ../rio-nix/src/hash_oracle.rs
            ../rio-nix/src/derivation/output.rs
            ../rio-nix/src/derivation/hash.rs
            ../rio-gateway/src/handler/opcodes_write.rs
            ../rio-gateway/src/translate.rs
            ../rio-builder/src/executor/native_result/mod.rs
            ../rio-builder/src/executor/glue/mod.rs
            ../rio-store/src/grpc/put_path/common.rs
          ];
        };
      }
      ''
        cd $src
        fail=0

        # ── Key-material base64 deny-table ──────────────────────────
        # file : expected `general_purpose::STANDARD` count : reason
        # Production key-entry parse/encode lives ONLY in the codec;
        # every other pinned site is signature-byte handling or a
        # test fixture (deliberately hand-built adversarial entries).
        check_count() {
          file=$1; expected=$2; reason=$3
          actual=$(grep -c 'general_purpose::STANDARD' "$file" || true)
          if [ "$actual" != "$expected" ]; then
            echo "FAIL: $file has $actual raw base64 sites, pinned $expected ($reason)." >&2
            echo "  A new raw base64 use of KEY-ENTRY material must route through" >&2
            echo "  rio_common::signing_keyfmt (SecretEntry/PublicEntry parse/encode/from_parts)." >&2
            echo "  Signature-byte or test-fixture sites: re-pin the count here WITH the reason." >&2
            fail=1
          fi
        }
        check_count rio-common/src/signing_keyfmt.rs 5 \
          "the OWNER: 2 production decode (secret/public parse), 2 canonical encode, 1 test helper"
        check_count rio-store/src/signing.rs 14 \
          "2 production SIGNATURE encode/decode (sign(), any_sig_trusted) + 12 test fixtures"
        check_count rio-store/src/grpc/admin.rs 2 \
          "2 test fixtures (valid/short pubkey payloads); the write gate parses via PublicEntry"
        check_count rio-store/src/grpc/sign.rs 1 \
          "1 test fixture (hand-built trusted entry)"
        check_count rio-store/src/metadata/tenant_keys.rs 1 \
          "1 test SIGNATURE decode; production entries via Signer::trusted_key_entry -> codec"
        check_count rio-store/src/metadata/cluster_key_history.rs 0 \
          "passes DB strings through; parse happens at the read gate via the codec"
        check_count rio-store/src/substitute.rs 5 \
          "4 test trusted-entry fixtures (W1-S4's narinfo suite added one) + 1 test SIGNATURE decode; production entries via the codec"
        check_count rio-cli/src/keygen.rs 3 \
          "3 test fixtures verifying codec output; production writes via SecretEntry/PublicEntry encode"

        # ── Hand-rolled derive deny ─────────────────────────────────
        # The bug_023 shape (byte-window slicing of decoded key
        # material) must never reappear in the bootstrap script: key
        # bytes are only touched by `rio-cli keygen`. Comments may
        # mention it as history; strip them first.
        if sed 's/[[:space:]]*#.*//' nix/bootstrap-job.sh | grep -qE 'base64|tail -c'; then
          echo "FAIL: nix/bootstrap-job.sh performs raw key-byte operations (base64/tail)." >&2
          echo "  All key-byte work goes through 'rio-cli keygen' (the signing_keyfmt codec)." >&2
          fail=1
        fi

        # ── Owner-file self-test ────────────────────────────────────
        # Prove the grep pattern still matches reality: the owner must
        # contain the canonical constructors this check's remediation
        # names. If a refactor renames them, this check must be
        # updated alongside (not silently weakened).
        for sym in 'fn parse' 'fn from_parts' 'fn from_seed' 'fn derive_pub' 'fn encode' 'validate_key_name'; do
          grep -q "$sym" rio-common/src/signing_keyfmt.rs || {
            echo "FAIL: signing_keyfmt.rs lost '$sym' — update single-source-conformance with the rename." >&2
            fail=1
          }
        done

        # ── outputHashAlgo strip-prefix deny-table ──────────────────
        # OutputHashAlgo::parse (rio-nix/src/hash.rs) is THE
        # constructor for `outputHashAlgo` declarations
        # (r[nix.hash.algos+2]). Open-coding `strip_prefix("r:")`
        # re-creates the pre-round-17 divergence (merged_bug_074: the
        # descriptor stamping and the modulo fingerprint silently
        # diverged from every other gate). Carve-outs, one site each:
        #   rio-nix/src/hash.rs              — the owner itself
        #   rio-nix/src/hash_oracle.rs       — line-by-line CppNix port
        #   rio-nix/src/derivation/output.rs — floating_algo keeps the
        #     RAW string for population classification (not algo parse)
        #   rio-gateway/.../opcodes_write.rs — ContentAddress WIRE
        #     format ("fixed:r:" colon descriptor — a different grammar
        #     that embeds the prefix)
        #   rio-store/.../put_path/common.rs — same wire grammar
        check_strip() {
          file=$1; expected=$2
          actual=$(grep -c 'strip_prefix("r:")' "$file" 2>/dev/null || true)
          if [ "$actual" != "$expected" ]; then
            echo "FAIL: $file has $actual strip_prefix(\"r:\") sites, pinned $expected." >&2
            echo "  outputHashAlgo declarations parse ONLY through" >&2
            echo "  rio_nix::hash::OutputHashAlgo::parse (r[nix.hash.algos+2])." >&2
            echo "  Wire-format/oracle/classification sites: re-pin here WITH a reason comment." >&2
            fail=1
          fi
        }
        check_strip rio-nix/src/hash.rs 1
        check_strip rio-nix/src/hash_oracle.rs 1
        check_strip rio-nix/src/derivation/output.rs 1
        check_strip rio-gateway/src/handler/opcodes_write.rs 1
        check_strip rio-store/src/grpc/put_path/common.rs 1
        # And the previously-divergent files must stay clean.
        for f in rio-nix/src/derivation/hash.rs \
                 rio-builder/src/executor/native_result/mod.rs \
                 rio-builder/src/executor/glue/mod.rs \
                 rio-gateway/src/translate.rs; do
          if grep -q 'strip_prefix("r:")' "$f" 2>/dev/null; then
            echo "FAIL: $f re-introduced an open-coded strip_prefix(\"r:\") — route through OutputHashAlgo::parse." >&2
            fail=1
          fi
        done

        [ "$fail" = 0 ] || exit 1
        touch $out
      '';

  # Round-17 bug_100 (RC17-06): worker-reported built outputs reach the
  # trusted plane ONLY through actor::completion::AdmittedOutputs. The
  # deny-table pins every `.built_outputs` field access (the raw,
  # pre-admission data) to the proto→domain conversion and the single
  # mem::take at the admission boundary; a new consumer must go through
  # the chokepoint or register here with a reason.
  completion-admission-conformance =
    pkgs.runCommand "rio-completion-admission-conformance"
      {
        nativeBuildInputs = [ pkgs.ripgrep ];
        src = pkgs.lib.fileset.toSource {
          root = ../rio-scheduler/src;
          fileset = pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-scheduler/src;
        };
      }
      ''
        cd $src
        fail=0
        # Format: path:expected-count:reason.
        carveouts() {
          cat <<'TABLE'
        domain.rs:1:the proto→domain conversion (constructs the raw vector; pre-trust by definition)
        actor/completion.rs:1:the mem::take at the AdmittedOutputs::admit boundary — the SOLE consumption
        TABLE
        }
        # Field ACCESS only (`.built_outputs`), comments excluded; the
        # actor's test tree exercises raw shapes deliberately and is
        # excluded from the production surface.
        actual=$(rg -n '\.built_outputs' --type rust . \
          | rg -v '^\S+:\s*\d+:\s*//' \
          | grep -v '^\./actor/tests/' | sed 's|^\./||' | sort)
        while IFS=: read -r file expected reason; do
          file=$(echo "$file" | tr -d ' ')
          [ -n "$file" ] || continue
          got=$(echo "$actual" | grep -c "^$file:" || true)
          if [ "$got" != "$expected" ]; then
            echo "FAIL: $file has $got raw .built_outputs accesses, deny-table pins $expected ($reason)." >&2
            echo "  Worker-reported outputs are trusted-plane data ONLY after" >&2
            echo "  actor::completion::AdmittedOutputs::admit (sched.completion.output-membership+1)." >&2
            echo "  Consume the AdmittedOutputs value, or register a count-pinned carve-out here." >&2
            fail=1
          fi
        done < <(carveouts | sed 's/^[[:space:]]*//')
        table_files=$(carveouts | sed 's/^[[:space:]]*//' | cut -d: -f1 | tr -d ' ')
        echo "$actual" | cut -d: -f1 | sort -u | while read -r f; do
          [ -n "$f" ] || continue
          echo "$table_files" | grep -qx "$f" || {
            echo "FAIL: $f reads .built_outputs outside the admission boundary:" >&2
            echo "$actual" | grep "^$f:" >&2
            echo "  Route through actor::completion::AdmittedOutputs (round-17 bug_100)." >&2
            exit 1
          }
        done || fail=1
        [ "$fail" = 0 ] || exit 1
        touch $out
      '';
  # Round-17 merged_bug_017 (RC17-10 mechanism): verdict folds at
  # composite seams must key on TYPED error properties / effective
  # request state, never on literal-string forms that silently route a
  # population around its arm. Three open-coded regression forms are
  # denied (comments stripped before matching); each carve-out is
  # count-pinned with its reason, and pins are expected to ratchet DOWN
  # within this stream (raw_os_error -> 0 at c3, e.to_string -> 0 at
  # c6) — a pin that can only grow is a deny-table that has stopped
  # denying.
  verdict-fold-policy =
    pkgs.runCommand "rio-verdict-fold-policy"
      {
        nativeBuildInputs = [ pkgs.ripgrep ];
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-builder/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-store/src)
          ];
        };
      }
      ''
        cd $src
        fail=0

        # Strip line comments so prose mentioning a form does not count.
        match_count() {
          # $1 = file, $2 = fixed-string needle. Occurrences, not lines
          # (an admission gate tests two schemes on one line). Only
          # LINE-LEADING comments are excluded — a sed comment-strip
          # would truncate URL literals at the '//' inside 'https://'.
          # stdenv sets pipefail: a zero-match grep would fail the whole
          # pipeline under set -e, killing the script before any FAIL
          # line — neutralize inside the braces, count outside.
          { grep -v '^[[:space:]]*//' "$1" | grep -oF -- "$2" || true; } | wc -l
        }

        check_pin() {
          # $1 = needle, $2 = file, $3 = expected, $4 = reason
          got=$(match_count "$2" "$1")
          if [ "$got" != "$3" ]; then
            echo "FAIL: $2 has $got occurrences of '$1' (pinned: $3 — $4)." >&2
            echo "  Scheme tests go through has_scheme() (ASCII-case-insensitive); errno" >&2
            echo "  transience uses the classify_restore_error allowlist; singleflight" >&2
            echo "  verdicts cross the Shared boundary as the typed Clone carrier." >&2
            echo "  (Pins live in nix/misc-checks.nix verdict-fold-policy.)" >&2
            fail=1
          fi
        }

        # ── 1. Case-sensitive scheme literals (RFC 3986: schemes are
        # case-insensitive; the literal form lets S3:// / HTTPS:// skip
        # their verdict arm — round-17 merged_bug_017).
        for f in $(find . -name '*.rs' | sed 's|^\./||'); do
          case "$f" in
            rio-store/src/grpc/admin.rs)
              check_pin 'starts_with("http' "$f" 2 \
                "fail-closed admission accept-gate: case-variant URLs are refused outright, never misclassified into a retry arm" ;;
            rio-store/src/substitute.rs)
              check_pin 'starts_with("http' "$f" 1 "test fixture URL probe" ;;
            *)
              check_pin 'starts_with("http' "$f" 0 "no carve-out" ;;
          esac
          check_pin 'starts_with("s3' "$f" 0 "no carve-out: has_scheme() owns scheme tests"
        done

        # ── 2. Errno-PRESENCE-as-transience (payload-composable errnos
        # like ENAMETOOLONG inherit Transient through it — round-17
        # merged_bug_022; narrowed to the errno allowlist by W2-S3 c3,
        # when this pin drops to 0).
        for f in $(find . -name '*.rs' | sed 's|^\./||'); do
          case "$f" in
            *)
              check_pin 'raw_os_error().is_some()' "$f" 0 \
                "no carve-out anywhere: errno transience goes through the classify_restore_error ALLOWLIST (W2-S3 c3 dropped the last presence-form pin)" ;;
          esac
        done

        # ── 3. Stringly verdict erasure across the singleflight Shared
        # boundary (flattens the anyhow chain before ChunkError
        # construction, making the BackendAuthError fail-fast
        # unreachable — round-17 merged_bug_061; replaced by the typed
        # Clone carrier at W2-S3 c6, when this pin drops to 0).
        check_pin 'Err(e.to_string())' rio-store/src/cas.rs 0 \
          "no carve-out: the singleflight Shared carries the typed Clone FetchFail (W2-S3 c6 dropped the last stringly carrier)"

        touch $out
      '';

  # Round-17 merged_bug_058 / RC17-09 (kill-writer conformance — the
  # F2 TRIGGER DEFINITION for the kill-corroboration family): every
  # cgroup.kill WRITER PRIMITIVE (`join("cgroup.kill")` expression) and
  # every caller of the two method wrappers must appear here with a
  # disposition. A NEW writer landing without a same-commit disposition
  # is the "in-governance recurrence" that activates the family's
  # pre-registered F2 scope (rio-exec owns ALL enforcement-kill writers
  # via KillClaim; cgroup.kill unwritable outside it). Dispositions:
  #   CLAIMED  — routed through rio-exec's kill-claim machinery; the
  #              wait status corroborates the claim (KillTarget).
  #   SCOPED   — principal-scoped enforcement kill via
  #              kill_principal_scope; UNCLAIMED (verdict authority is
  #              the caller's own flag: log_limit_exceeded /
  #              oom_detected); TRANSITIONAL until KillReason::LogLimit
  #              claim routing lands (named follow-up tail).
  #   TEARDOWN — cancel/drain/abort whole-tree kill; fires only where
  #              no forwarded-status verdict exists or it is already
  #              settled, so no evidence can be destroyed.
  #   TEST     — assertion reads in test code; not a writer.
  kill-writer-conformance =
    pkgs.runCommand "rio-kill-writer-conformance"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [ workspaceFileset ];
        };
        nativeBuildInputs = [ pkgs.ripgrep ];
      }
      ''
        cd $src
        fail=0
        remediation() {
          echo "  Every cgroup.kill writer needs a same-commit disposition in" >&2
          echo "  nix/misc-checks.nix kill-writer-conformance. An enforcement" >&2
          echo "  kill (one that produces or overrides a build verdict) MUST be" >&2
          echo "  principal-scoped (crate::cgroup::kill_principal_scope) or" >&2
          echo "  CLAIMED via rio-exec; a bypass is the in-governance trigger" >&2
          echo "  for this family's F2 escalation (KillClaim ownership)." >&2
        }
        # Table 1: writer primitives — join("cgroup.kill") expressions.
        prim() {
          f=$1; want=$2; why=$3
          got=$(rg -c 'join\("cgroup\.kill"\)' "$f" 2>/dev/null || echo 0)
          if [ "$got" != "$want" ]; then
            echo "FAIL: $f has $got join(cgroup.kill) primitives, allowlist pins $want ($why)." >&2
            remediation; fail=1
          fi
        }
        prim rio-exec/src/execute.rs 5 \
          "CLAIMED principal (KillTarget::Principal, post-placement) + CLAIMED tree (kill_pid_and_cgroup, pre-placement) + 3 TEST reads (cancel assertion; placement near-miss battery asserts no kill file touched x2)"
        prim rio-builder/src/cgroup.rs 4 \
          "kill_principal_scope SCOPED x2 (principal write + pre-placement ENOENT fallback) + BuildCgroup::kill TEARDOWN + kill_cgroup TEARDOWN"
        prim rio-builder/src/executor/mod.rs 1 \
          "abort scopeguard TEARDOWN (error-path teardown; no settled verdict exists)"
        prim rio-builder/src/runtime/mod.rs 1 "TEST read (cancel-registry assertion)"
        files=$(rg -l 'join\("cgroup\.kill"\)' --type rust . | wc -l)
        if [ "$files" != "4" ]; then
          echo "FAIL: join(cgroup.kill) primitives appear in $files rust files; the allowlist registers 4:" >&2
          rg -l 'join\("cgroup\.kill"\)' --type rust . >&2
          remediation; fail=1
        fi
        # Table 2: method-wrapper callers OUTSIDE the defining module —
        # the likeliest shape for a new unaudited kill.
        wrap() {
          f=$1; want=$2; why=$3
          got=$(rg -n 'kill_principal_scope\(|kill_cgroup\(|build_cgroup\.kill\(\)' "$f" 2>/dev/null | rg -v ':\s*//' | rg -v 'fn (kill_principal_scope|kill_cgroup)' | wc -l)
          if [ "$got" != "$want" ]; then
            echo "FAIL: $f has $got kill-wrapper call sites, allowlist pins $want ($why)." >&2
            remediation; fail=1
          fi
        }
        wrap rio-builder/src/executor/monitors.rs 3 \
          "OOM-loop breaker SCOPED (the round-17 sweep-found writer the finding missed) + drain_build_cgroup TEARDOWN (post-settlement drain) + its failure-warn STRING (prose, not a writer)"
        wrap rio-builder/src/executor/mod.rs 1 \
          "principal_cap_kill body SCOPED (log-cap arms; both cap arms route through it)"
        wrap rio-builder/src/runtime/slot.rs 1 \
          "scheduler cancel TEARDOWN (no verdict; CancelSignal semantics)"

        [ "$fail" = 0 ] || exit 1
        touch $out
      '';

  # Round-17 bug_043 (RC17-08 mechanism): the M_072 failure-evidence
  # pair (builds.error_summary, builds.failed_derivation) is sealed
  # per COLUMN, not per helper — round-16's R2 sweep keyed to the
  # helper name (persist_build_error_summary) and missed the plain
  # bind in update_build_status_tx's terminal arm 75 lines below it.
  # Three deny patterns over non-test scheduler sources:
  #   P1 — raw SQL bind `<col> = $N`: ZERO sites. Both legal SQL
  #        assignments COALESCE, and live in db/builds.rs (pinned 3:
  #        two halves of the pair statement + the terminal-arm
  #        backstop).
  #   P2 — in-memory `.{col} = Some(..)`: ZERO sites. The chokepoint
  #        form is get_or_insert_with — first-failure wins on every
  #        ordering (at-source, dispatch echo, timeout, recovery
  #        reconstruction).
  #   P3 — any other direct field assignment: carve-out for the two
  #        recovery HYDRATION sites only (restoring the row's own
  #        value is not a new observation).
  paired-writer-seal =
    pkgs.runCommand "rio-paired-writer-seal"
      {
        nativeBuildInputs = [ pkgs.ripgrep ];
        src = pkgs.lib.fileset.toSource {
          root = ../rio-scheduler/src;
          fileset = pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-scheduler/src;
        };
      }
      ''
        cd $src
        fail=0
        nontest() { grep -v '/tests/' || true; }

        # P1: raw SQL plain bind of a pair column.
        hits=$(rg -n '(error_summary|failed_derivation) = \$' --type rust . | nontest || true)
        if [ -n "$hits" ]; then
          echo "FAIL: plain SQL bind of a failure-evidence pair column (must COALESCE through db/builds.rs):" >&2
          echo "$hits" >&2
          echo "  remediation: route through persist_build_error_summary_tx, or COALESCE like the terminal arm." >&2
          fail=1
        fi

        # P1b: COALESCE assignments allowed only in db/builds.rs, pinned at 3.
        co=$(rg -c '(error_summary|failed_derivation) = COALESCE' db/builds.rs || true); co=''${co:-0}
        if [ "$co" != "3" ]; then
          echo "FAIL: db/builds.rs has $co COALESCE pair-column assignments, pinned 3 (pair statement x2 + terminal arm)." >&2
          echo "  A new writer tier must update this pin WITH its first-write-wins reasoning." >&2
          fail=1
        fi
        outside=$(rg -ln '(error_summary|failed_derivation) = COALESCE' --type rust . | sed 's|^\./||' | nontest | grep -v '^db/builds.rs$' || true)
        if [ -n "$outside" ]; then
          echo "FAIL: COALESCE pair-column SQL outside db/builds.rs (the pair's sole SQL owner):" >&2
          echo "$outside" >&2
          fail=1
        fi

        # P2: in-memory plain Some-assignment.
        hits=$(rg -n '\.(error_summary|failed_derivation) = Some\(' --type rust . | nontest || true)
        if [ -n "$hits" ]; then
          echo "FAIL: plain Some-assignment to a failure-evidence field (chokepoint form is get_or_insert_with):" >&2
          echo "$hits" >&2
          fail=1
        fi

        # P3: any remaining direct assignment — hydration carve-out only.
        hits=$(rg -n '\.(error_summary|failed_derivation) = ' --type rust . | sed 's|^\./||' | nontest | grep -v 'get_or_insert' || true)
        n=$(printf '%s' "$hits" | grep -c . || true); n=''${n:-0}
        nrec=$(printf '%s' "$hits" | grep -c '^actor/recovery.rs:' || true); nrec=''${nrec:-0}
        if [ "$n" != "2" ] || [ "$nrec" != "2" ]; then
          echo "FAIL: direct pair-field assignments outside the 2 pinned recovery hydration sites:" >&2
          echo "$hits" >&2
          echo "  hydration restores the row's own value; any NEW observation goes through" >&2
          echo "  record_failure_evidence / get_or_insert_with (first-failure wins)." >&2
          fail=1
        fi

        [ "$fail" = 0 ] || exit 1
        touch $out
      '';
  # Round-17 merged_bug_001 (RC17-15 c8 — third recurrence of the
  # mangled-wrap class on this branch, so it gets the class gate):
  # a wrapped Rust string literal missing its backslash continuations
  # ships embedded multi-space runs to clients/operators (gRPC status
  # messages, basis labels, assertion text). Pattern: a run of 5+
  # spaces with non-space on BOTH sides inside ONE double-quoted
  # literal on a single source line (between-literal alignment and
  # multi-line raw SQL do not match). Carve-outs are count-pinned
  # per file: deliberate column alignment (CLI/banner output, where
  # the run is the format) and source-shape test fixtures.
  string-space-runs =
    pkgs.runCommand "rio-string-space-runs"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = workspaceFileset;
        };
        nativeBuildInputs = [ pkgs.ripgrep ];
      }
      ''
        cd $src
        pat='"[^"]*[^" ][ ]{5,}[^" ][^"]*"'
        fail=0

        # Per-file pins: alignment families stay, anything else fails.
        declare -A allow=(
          [rio-builder/src/banner.rs]=4        # exec-id banner column alignment
          [rio-cli/src/sla.rs]=10              # CLI key/value column alignment
          [rio-cli/src/invalidate_path.rs]=5   # CLI key/value column alignment
          [rio-cli/src/pool.rs]=2              # CLI key/value column alignment
          [rio-cli/src/workers.rs]=1           # CLI key/value column alignment
          [rio-scheduler/src/sla/metrics.rs]=1 # source-shape fixture (gauge! grep)
          [rio-store/src/gc/sweep.rs]=1        # SQL-shape assertion fixture
        )

        while IFS=: read -r f n; do
          f=$(printf '%s' "$f" | sed 's|^\./||')
          want=''${allow[$f]:-0}
          if [ "$n" != "$want" ]; then
            echo "FAIL: $f has $n in-literal space runs (pinned: $want)." >&2
            rg -n "$pat" "$f" | head -5 >&2
            echo "  A wrapped string literal needs backslash continuations: \"...text \\" >&2
            echo "       more text\" — otherwise the indentation ships to the client." >&2
            echo "  Deliberate alignment goes in the pin table" >&2
            echo "  (nix/misc-checks.nix string-space-runs) with a reason." >&2
            fail=1
          fi
        done < <(rg -c "$pat" --type rust . || true)

        [ "$fail" = 0 ] || exit 1
        touch $out
      '';
  # Round-17 merged_bug_024 / RC17-13 (closure-witness producers — the
  # F2 TRIGGER DEFINITION for the closure-witness family): every DAG
  # truncation primitive call site and every closure-hole stamp site
  # must appear here with a disposition. The witness contract is
  # cumulative since the hole was last whole, and round-17 found two of
  # four producers applying it at their original trigger only — a NEW
  # truncation site landing without a same-commit registry entry (and a
  # stamp disposition) is the "in-governance recurrence" that activates
  # the family's pre-registered F2 scope: hole-stamping moves INTO the
  # DAG truncation chokepoint (remove_node/edge-drop own the stamp) and
  # per-call-site stamp policy ceases to exist. Patterns are rg -U
  # multiline-tolerant: rustfmt splits method chains, and a wrapped
  # `.closure_hole\n.stamp(` site is invisible to single-line grep.
  # Dispositions:
  #   REAP        — remove_build_interest_and_reap: stamps via the
  #                 un-produced trigger ∪ witness_watched (produced
  #                 removals from already-holed parents), full-set
  #                 capture for triggered parents.
  #   TTL-SWEEP   — poison-TTL housekeeping: removes Poisoned (un-
  #                 produced by definition) children, stamps every
  #                 surviving parent at the call site.
  #   CLEARPOISON — admin ClearPoison: same capture-stamp-persist
  #                 sequence at its call site.
  #   RECOVERY    — load-time edge-drop: trigger = un-produced drop OR
  #                 restored watched flag; content = ALL dropped
  #                 terminal children.
  #   PRUNE       — merge-time top-down prune: kept nodes born holed
  #                 with the dropped closure as the witness (the
  #                 in-memory half of Batch 1b's paired write).
  #   EPOCH       — scrub_dependency_edges callers (displacement /
  #                 authority takeover): definition REPLACEMENT, not
  #                 closure truncation — the witness lifecycle is owned
  #                 by the epoch machinery (carry_across / clear) at
  #                 the same sites.
  #   DEFINITION  — the primitive's own fn body.
  #   TEST        — test fixtures staging holes directly.
  closure-witness-producers =
    pkgs.runCommand "rio-closure-witness-producers"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = workspaceFileset;
        };
        nativeBuildInputs = [ pkgs.ripgrep ];
      }
      ''
        cd $src/rio-scheduler/src
        fail=0

        check_registry() {
          # $1 = label, $2 = rg -U pattern, $3 = name of assoc array
          local label=$1 pat=$2
          local -n pins=$3
          declare -A seen=()
          while IFS=: read -r f n; do
            f=$(printf '%s' "$f" | sed 's|^\./||')
            seen[$f]=$n
            local want=''${pins[$f]:-0}
            if [ "$n" != "$want" ]; then
              echo "FAIL($label): $f has $n sites, registry pins $want." >&2
              rg -Un "$pat" "$f" | head -6 >&2
              fail=1
            fi
          done < <(rg -Uc "$pat" . || true)
          for f in "''${!pins[@]}"; do
            if [ -z "''${seen[$f]:-}" ] && [ "''${pins[$f]}" != "0" ]; then
              echo "FAIL($label): registry pins ''${pins[$f]} sites in $f but rg found none (stale registry)." >&2
              fail=1
            fi
          done
        }

        # Truncation primitive: node removal.
        declare -A rm_pins=(
          [dag/mod.rs]=1            # REAP
          [actor/housekeeping.rs]=1 # TTL-SWEEP
          [actor/completion.rs]=1   # CLEARPOISON
          [dag/tests.rs]=6          # TEST
        )
        check_registry remove_node '\.\s*\n?\s*remove_node\(' rm_pins

        # Truncation primitive: dependency-edge scrub.
        declare -A scrub_pins=(
          [dag/mod.rs]=3 # 1 DEFINITION + 2 EPOCH (displacement, takeover)
        )
        check_registry scrub_dependency_edges 'scrub_dependency_edges\s*\(' scrub_pins

        # Witness stamp sites (the producers' write half).
        declare -A stamp_pins=(
          [dag/mod.rs]=1            # REAP
          [actor/housekeeping.rs]=1 # TTL-SWEEP
          [actor/completion.rs]=1   # CLEARPOISON
          [actor/recovery.rs]=1     # RECOVERY
          [actor/merge.rs]=1        # PRUNE (born-holed; found by THIS check's first run)
          [dag/tests.rs]=9          # TEST
        )
        check_registry closure_hole.stamp 'closure_hole\s*\n?\s*\.stamp\(' stamp_pins

        if [ "$fail" != 0 ]; then
          echo "remediation: a DAG truncation site must stamp the closure witness (or carry" >&2
          echo "  an EPOCH/TEST disposition) and register here in the SAME commit:" >&2
          echo "  nix/misc-checks.nix closure-witness-producers. An unregistered truncation" >&2
          echo "  is the closure-witness family's F2 trigger (round-17 plan section A.2)." >&2
          exit 1
        fi
        touch $out
      '';
}
