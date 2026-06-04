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
  # Derived fuzz workspace list (nix/lib/filesets.nix) — one
  # crate2nix-drift-fuzz-<ws> check per workspace, so adding a fuzz
  # workspace extends the gate without editing this file.
  fuzzWorkspaces,
  # crate2nix CLI (flake input build) — crate2nix-drift regenerates
  # Cargo.json hermetically with it.
  crate2nixCli,
  rustStable,
  rustPlatformStable,
  traceyPkg,
  subcharts,
  dockerImages,
  nodeAmi,
  docsLib,
  xtaskBin,
  # Devshell passthru (flake.nix): the REAL kache policy wrapper and
  # entry-time epoch-GC script the dev shells wire up, plus the
  # store-epoch salt. kache-wrapper-test / kache-epoch-gc-test below
  # run these caller-faithfully — no copy-paste of the shell text, so
  # they cannot drift from what `nix develop` actually executes.
  kacheWrapped,
  rioKacheEpochGc,
  kacheEnvSalt,
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

  # Hermetic regen-and-diff for one fuzz workspace's Cargo.json — the
  # crate2nix-drift recipe below, parameterized over fuzz/<ws>. Why
  # this must exist: nix/fuzz.nix consumes ONLY the checked-in fuzz
  # Cargo.json (its src filesets stage Cargo.toml + fuzz_targets — the
  # fuzz Cargo.locks are read by NO derivation), so a lock-only version
  # bump or feature flip otherwise builds the OLD graph bit-identically
  # green; the only loud failure mode is a newly-ADDED dep missing from
  # the stale json. The pre-commit crate2nix-check covers the same
  # drift, but only when Cargo.{toml,lock} is staged, hooks are
  # installed, and --no-verify isn't used — this is the hermetic
  # backstop.
  #
  # Mechanics: cargoDeps vendors the fuzz workspace's OWN lock, and
  # `cargoRoot` points cargoSetupHook's lock-consistency validation at
  # fuzz/<ws>/Cargo.lock instead of the root lock. crate2nix emits
  # output-relative paths, so generation must run in-place inside
  # fuzz/<ws> for the `../..` member paths to match the committed file
  # (same trick the pre-commit hook documents). `cargo metadata`
  # tolerates missing explicit-path [[bin]] files (verified for the
  # pinned cargo: exit 0 with fuzz_targets/ deleted), so the eval-time
  # fuzz_targets stubs are belt-and-braces for any future crate2nix
  # that probes target paths — existence-keyed like stubTargetFiles,
  # which itself covers the path-dep workspace members' autodiscovery.
  mkFuzzCrate2nixDrift =
    ws:
    let
      stubFuzzTargets =
        let
          dir = unfilteredRoot + "/fuzz/${ws}/fuzz_targets";
        in
        pkgs.lib.optionalString (builtins.pathExists dir) (
          pkgs.lib.concatMapStrings (f: ''
            touch fuzz/${ws}/fuzz_targets/${f}
          '') (builtins.filter (f: pkgs.lib.hasSuffix ".rs" f) (builtins.attrNames (builtins.readDir dir)))
        );
    in
    pkgs.stdenv.mkDerivation {
      pname = "rio-crate2nix-drift-fuzz-${ws}";
      inherit version;
      src = pkgs.lib.fileset.toSource {
        root = unfilteredRoot;
        fileset = pkgs.lib.fileset.unions [
          manifestsFileset
          (unfilteredRoot + "/fuzz/${ws}/Cargo.lock")
          (unfilteredRoot + "/fuzz/${ws}/Cargo.json")
        ];
      };
      cargoRoot = "fuzz/${ws}";
      cargoDeps = rustPlatformStable.importCargoLock {
        lockFile = unfilteredRoot + "/fuzz/${ws}/Cargo.lock";
      };
      nativeBuildInputs = [
        crate2nixCli
        rustStable
        rustPlatformStable.cargoSetupHook
      ];
      buildPhase = ''
        export HOME=$TMPDIR
        export CARGO_NET_OFFLINE=true
        ${stubTargetFiles}
        mkdir -p fuzz/${ws}/fuzz_targets
        ${stubFuzzTargets}
        ( cd fuzz/${ws} && crate2nix generate --format json -o Cargo.json.check )
        # Newline-terminate ONLY if crate2nix didn't — mirrors
        # xtask/src/regen/cargo_json.rs's conditional append, so a
        # future crate2nix that emits its own trailing newline can't
        # double-append here and spuriously drift-fail every gate.
        [ -z "$(tail -c1 fuzz/${ws}/Cargo.json.check)" ] || echo >> fuzz/${ws}/Cargo.json.check
        diff -u fuzz/${ws}/Cargo.json fuzz/${ws}/Cargo.json.check || {
          echo 'error: fuzz/${ws}/Cargo.json is stale — run `cargo xtask regen cargo-json`'
          exit 1
        }
      '';
      installPhase = ''
        touch $out
      '';
    };
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

  # Hermetic backstop for the pre-commit crate2nix-check: regenerate the
  # ROOT Cargo.json from manifests + lockfile and diff against the
  # committed copy. Same cargoSetupHook + importCargoLock recipe as
  # `deny`/`hakari-drift` — crate2nix shells out to `cargo metadata`,
  # which resolves against the vendored sources offline, and
  # stubTargetFiles synthesizes the (empty) target files autodiscovery
  # needs, so the output depends on manifests + lock only (verified for
  # crate2nix 0.15.0: relative paths, lock-derived sha256s, no absolute
  # roots). The two fuzz-workspace Cargo.jsons get the same
  # regen-and-diff treatment below (crate2nix-drift-fuzz-*) — no fuzz
  # derivation reads the fuzz Cargo.locks, so a stale json builds the
  # old graph silently; only a newly-added dep fails loudly.
  crate2nix-drift = pkgs.stdenv.mkDerivation {
    pname = "rio-crate2nix-drift";
    inherit version;
    src = pkgs.lib.fileset.toSource {
      root = unfilteredRoot;
      fileset = pkgs.lib.fileset.unions [
        manifestsFileset
        ../Cargo.json
      ];
    };
    cargoDeps = rustPlatformStable.importCargoLock {
      lockFile = ../Cargo.lock;
    };
    nativeBuildInputs = [
      crate2nixCli
      rustStable
      rustPlatformStable.cargoSetupHook
    ];
    buildPhase = ''
      export HOME=$TMPDIR
      export CARGO_NET_OFFLINE=true
      ${stubTargetFiles}
      crate2nix generate --format json -o Cargo.json.check
      # Newline-terminate ONLY if crate2nix didn't — mirrors
      # xtask/src/regen/cargo_json.rs's conditional append, so a future
      # crate2nix that emits its own trailing newline can't
      # double-append here and spuriously drift-fail every gate.
      [ -z "$(tail -c1 Cargo.json.check)" ] || echo >> Cargo.json.check
      diff -u Cargo.json Cargo.json.check || {
        echo 'error: Cargo.json is stale — run `cargo xtask regen cargo-json`'
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
        # daemon_timeout's unrelated `2h` / test literals).
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
# Fuzz-workspace Cargo.json drift — one check per derived workspace
# (attr names: crate2nix-drift-fuzz-<ws>). See mkFuzzCrate2nixDrift
# above for why these exist (the fuzz derivations would otherwise
# build a stale graph silently).
// builtins.listToAttrs (
  map (
    ws: pkgs.lib.nameValuePair "crate2nix-drift-fuzz-${ws}" (mkFuzzCrate2nixDrift ws)
  ) fuzzWorkspaces
)
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

  # bootstrap-job.yaml documents the script as "Idempotent". The
  # signing-key block guarded ONE secret but created TWO; a Job retry
  # after dying between them (or a delete-private-only rotation) left
  # a permanently mismatched/missing pub. Mock aws + nix-store +
  # openssl + ssh-keygen and assert convergence from partial state.
  bootstrap-idempotent =
    pkgs.runCommand "rio-bootstrap-idempotent"
      {
        nativeBuildInputs = [ pkgs.bash ];
      }
      ''
        export TMPDIR=$PWD
        mkdir -p secrets bin tmp
        sh=${pkgs.bash}/bin/bash
        # Mock aws: state in $TMPDIR/secrets/<id-with-slashes-as-_>.
        # describe-secret → exit 0 iff file exists; create-secret →
        # ResourceExistsException (exit 254) if exists, else write;
        # put-secret-value → unconditional overwrite. Minimal fidelity:
        # asserts CONTROL FLOW, not AWS semantics.
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
        case "\$sub" in
          "secretsmanager describe-secret") [ -f "\$f" ] ;;
          "secretsmanager create-secret")
            [ -f "\$f" ] && { echo ResourceExistsException >&2; exit 254; }
            echo "\$payload" > "\$f" ;;
          "secretsmanager put-secret-value") echo "\$payload" > "\$f" ;;
          *) exit 0 ;;
        esac
        EOF
        # Trivial mocks: nix-store writes deterministic content keyed
        # by a counter so scenario C can detect regeneration.
        cat > bin/nix-store <<EOF
        #!$sh
        n=\$(cat $TMPDIR/gen-count 2>/dev/null || echo 0)
        n=\$((n+1)); echo \$n > $TMPDIR/gen-count
        eval "sec=\\\''${\$((\$#-1))}"; eval "pub=\\\''${\$#}"
        echo "sec-\$n" > "\$sec"; echo "pub-\$n" > "\$pub"
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
        export PATH=$PWD/bin:${pkgs.coreutils}/bin:${pkgs.gnugrep}/bin
        export AWS_REGION=x CHUNK_BUCKET=x

        run() { $sh ${dockerImages.bootstrapScript}; }

        # Scenario A: fresh → both halves exist.
        run
        [ -f secrets/rio_signing-key ] && [ -f secrets/rio_signing-key-pub ] \
          || { echo "FAIL-A: fresh run did not create both signing-key halves" >&2; exit 1; }

        # Scenario B (the bug): private exists, pub missing → must
        # converge. Old guard checked private only → skipped → pub
        # stayed missing forever.
        rm secrets/rio_signing-key-pub
        run
        [ -f secrets/rio_signing-key-pub ] \
          || { echo "FAIL-B: pub missing after retry (guard checked private only?)" >&2; exit 1; }

        # Scenario C: pub exists with OLD content, private missing →
        # both must regenerate (pub overwritten via put-secret-value).
        # Old code: create-secret on existing pub → exit 254 → set -e
        # → script aborts; next retry sees private now exists → skips
        # → stale pub forever.
        rm secrets/rio_signing-key
        echo OLD > secrets/rio_signing-key-pub
        run
        [ -f secrets/rio_signing-key ] \
          || { echo "FAIL-C: private not recreated" >&2; exit 1; }
        if grep -qx OLD secrets/rio_signing-key-pub; then
          echo "FAIL-C: pub not overwritten (stale pair)" >&2; exit 1
        fi
        touch $out
      '';

  # The kacheWrapped RUSTC_WRAPPER's bypass-path debris sweep + argv
  # routing, exercised against the BUILT wrapper — the same store path
  # the devshell exports as RUSTC_WRAPPER. Fixtures simulate kache
  # restores (0444 files named like deps/ outputs). The sweep must
  # anchor on the invocation's OWN -C extra-filename — the unit-unique
  # output identity — so a bypassed compile can never unlink a SIBLING
  # unit's restores (bin+lib double units, feature-config twins,
  # duplicate dep versions all share a crate name). The warning-probe
  # and purge-CLI paths fall through to `exec kache`, which pulls the
  # kache flake input build into this check's closure — accepted
  # (caller-faithful beats stubbing the exec). Every other scenario —
  # including the ENABLED-mode ones — exits via a bypass exec before
  # kache ever runs, so the check needs no daemon/store beyond that.
  kache-wrapper-test =
    pkgs.runCommand "rio-kache-wrapper-test" { nativeBuildInputs = [ pkgs.gnugrep ]; }
      ''
        set -euo pipefail
        wrapper=${kacheWrapped}/bin/kache
        salt=${kacheEnvSalt}
        export HOME=$TMPDIR/home
        mkdir -p "$HOME" "$TMPDIR/bin"
        export STUB_LOG=$TMPDIR/stub.log
        : > "$STUB_LOG"
        # PATH stubs: bypass arms exec argv[1], so a recorded stub run
        # is the observable "bypass fired" signal. Each invocation
        # appends exactly ONE line: argv plus the stub's view of the
        # KACHE_* env at the exec boundary. The line count is the
        # mutation discriminator for enabled-mode bypass arms — a
        # deleted arm sends the invocation through real kache, whose
        # toolchain key probes (-vV etc.) run the stub extra times
        # with probe-shaped argv; the env dump pins scrub/mapping/
        # no-HOME behavior.
        for s in rustc myrustc wkspc-wrap; do
          cat > "$TMPDIR/bin/$s" <<EOF
        #!${pkgs.runtimeShell}
        {
          printf 'stub-$s ran argv:'
          for a in "\$@"; do printf ' %s' "\$a"; done
          printf ' | KACHE_CACHE_DIR=%s KACHE_MAX_SIZE=%s KACHE_DISABLED=%s KACHE_CONFIG=%s\n' \
            "\''${KACHE_CACHE_DIR-<unset>}" "\''${KACHE_MAX_SIZE-<unset>}" \
            "\''${KACHE_DISABLED-<unset>}" "\''${KACHE_CONFIG-<unset>}"
        } >> "\$STUB_LOG"
        exit 0
        EOF
          chmod +x "$TMPDIR/bin/$s"
        done
        export PATH=$TMPDIR/bin:$PATH

        # Multi-unit fixture: one crate name under two extras (lib unit
        # -aaaa…, bin-shaped unit -bbbb…), a foreign crate, and a
        # WRITABLE file matching the -aaaa… glob.
        deps=$TMPDIR/deps
        mkdir -p "$deps"
        for f in librio_demo-aaaaaaaaaaaaaaaa.rlib \
                 librio_demo-aaaaaaaaaaaaaaaa.rmeta \
                 rio_demo-aaaaaaaaaaaaaaaa.d \
                 librio_demo-bbbbbbbbbbbbbbbb.rlib \
                 rio_demo-bbbbbbbbbbbbbbbb \
                 librio_other-cccccccccccccccc.rlib; do
          echo x > "$deps/$f"
          chmod 0444 "$deps/$f"
        done
        echo w > "$deps/rio_demo-aaaaaaaaaaaaaaaa.writable.o"

        # Two-arg form (what current cargo emits): exactly unit
        # -aaaa…'s read-only triple goes — including the lib-prefix-
        # less .d — and nothing else.
        KACHE_DISABLED=1 "$wrapper" rustc --crate-name rio_demo \
          --out-dir "$deps" -C metadata=1111111111111111 \
          -C extra-filename=-aaaaaaaaaaaaaaaa --version 2>"$TMPDIR/err.twoarg"
        grep -q 'stub-rustc ran' "$STUB_LOG" \
          || { echo "FAIL: disabled bypass did not exec the real compiler" >&2; exit 1; }
        for gone in librio_demo-aaaaaaaaaaaaaaaa.rlib \
                    librio_demo-aaaaaaaaaaaaaaaa.rmeta \
                    rio_demo-aaaaaaaaaaaaaaaa.d; do
          [ ! -e "$deps/$gone" ] \
            || { echo "FAIL: $gone survived its own unit's sweep" >&2; exit 1; }
        done
        for kept in librio_demo-bbbbbbbbbbbbbbbb.rlib \
                    rio_demo-bbbbbbbbbbbbbbbb \
                    librio_other-cccccccccccccccc.rlib \
                    rio_demo-aaaaaaaaaaaaaaaa.writable.o; do
          [ -e "$deps/$kept" ] \
            || { echo "FAIL: sweep collateral — $kept deleted" >&2; exit 1; }
        done

        # Joined -Cextra-filename= spelling sweeps the sibling unit,
        # including the extensionless bin artifact.
        KACHE_DISABLED=1 "$wrapper" rustc --crate-name rio_demo \
          --out-dir "$deps" -Cextra-filename=-bbbbbbbbbbbbbbbb --version 2>/dev/null
        for gone in librio_demo-bbbbbbbbbbbbbbbb.rlib rio_demo-bbbbbbbbbbbbbbbb; do
          [ ! -e "$deps/$gone" ] \
            || { echo "FAIL: joined-form sweep missed $gone" >&2; exit 1; }
        done
        [ -e "$deps/librio_other-cccccccccccccccc.rlib" ] \
          || { echo "FAIL: joined-form sweep deleted a foreign crate's file" >&2; exit 1; }

        # Probe shape: no --out-dir → exits 0, deletes nothing.
        KACHE_DISABLED=1 "$wrapper" rustc -vV >/dev/null 2>&1
        [ -e "$deps/librio_other-cccccccccccccccc.rlib" ] \
          || { echo "FAIL: -vV probe swept files" >&2; exit 1; }

        # Extra-less fallback (non-cargo argv shape): conservative
        # -links +1 arm only. The nlink-1 0444 file ALSO matches the
        # deleted crate-name pattern — its survival pins that arm's
        # removal; the nlink-2 0444 hardlink (second link outside the
        # dir, like a store blob) must go.
        fb=$TMPDIR/fb
        mkdir -p "$fb"
        echo solo > "$fb/rio_demo-deadbeefdeadbeef.o"
        chmod 0444 "$fb/rio_demo-deadbeefdeadbeef.o"
        echo blob > "$TMPDIR/store-blob"
        ln "$TMPDIR/store-blob" "$fb/librio_demo-deadbeefdeadbeef.rlib"
        chmod 0444 "$fb/librio_demo-deadbeefdeadbeef.rlib"
        KACHE_DISABLED=1 "$wrapper" rustc --crate-name rio_demo \
          --out-dir "$fb" --version 2>/dev/null
        [ -e "$fb/rio_demo-deadbeefdeadbeef.o" ] \
          || { echo "FAIL: extra-less fallback deleted an nlink-1 file (crate-name arm resurrected?)" >&2; exit 1; }
        [ ! -e "$fb/librio_demo-deadbeefdeadbeef.rlib" ] \
          || { echo "FAIL: extra-less fallback kept the nlink-2 shared-inode file" >&2; exit 1; }

        # Glob-injection guard: a non-hex extra-filename is rejected by
        # the shape validation → conservative arm → nlink-1 canary
        # survives (an unvalidated '*' would glob-match everything).
        gi=$TMPDIR/gi
        mkdir -p "$gi"
        echo c > "$gi/canary"
        chmod 0444 "$gi/canary"
        KACHE_DISABLED=1 "$wrapper" rustc --crate-name x --out-dir "$gi" \
          -C 'extra-filename=*' --version 2>/dev/null
        [ -e "$gi/canary" ] \
          || { echo "FAIL: glob-shaped extra-filename swept unrelated files" >&2; exit 1; }

        # Bare exported RUSTC: cargo passes the value verbatim as
        # argv[1]; the equality escape hatch must take the bypass with
        # NO warning. (16-hex extra so this keeps exercising the
        # extra-anchored sweep arm under the exactly-16 tightening.)
        acc=$TMPDIR/acc
        mkdir -p "$acc"
        echo k > "$acc/keepme"
        chmod 0444 "$acc/keepme"
        RUSTC=myrustc KACHE_DISABLED=1 "$wrapper" myrustc --crate-name x \
          --out-dir "$acc" -C extra-filename=-dead0000dead0000 --version 2>"$TMPDIR/err.accept"
        grep -q 'stub-myrustc ran' "$STUB_LOG" \
          || { echo "FAIL: bare exported RUSTC did not take the bypass" >&2; exit 1; }
        if grep -q 'not a recognized compiler' "$TMPDIR/err.accept"; then
          echo "FAIL: warning fired for an honored RUSTC" >&2; exit 1
        fi
        [ -e "$acc/keepme" ] \
          || { echo "FAIL: sweep matched outside its extra" >&2; exit 1; }

        # Compile-shaped foreign argv[1]: the loud warning fires, no
        # bypass arm runs, the fixture is untouched (the fall-through
        # exec hands argv to kache, which fails to spawn the fake
        # compiler — that exit is expected).
        KACHE_DISABLED=1 "$wrapper" not-a-compiler --crate-name x \
          --out-dir "$gi" -C extra-filename=-dead0000dead0000 --version \
          2>"$TMPDIR/err.warn" || true
        grep -q 'not a recognized compiler' "$TMPDIR/err.warn" \
          || { echo "FAIL: compile-shaped foreign argv[1] did not warn" >&2; exit 1; }
        [ -e "$gi/canary" ] \
          || { echo "FAIL: foreign argv[1] path swept files" >&2; exit 1; }

        # ---- Enabled-mode bypass arms (each scenario names the guard
        # line whose deletion turns it red) ----

        # Enabled -Z bypass — kills job 6's `[ "$zflag" = 1 ] &&
        # is_compiler` arm: no KACHE_DISABLED, a -Z token, RUSTC
        # escape hatch. Exactly ONE stub line with the original argv;
        # a deleted arm falls through to real kache, whose key probes
        # re-run the stub with probe argv (count != 1).
        : > "$STUB_LOG"
        RUSTC=myrustc "$wrapper" myrustc --crate-name zee -Zunpretty=hir \
          --out-dir "$TMPDIR/zd" -C extra-filename=-cafecafecafecafe --version
        [ "$(wc -l < "$STUB_LOG")" -eq 1 ] \
          || { echo "FAIL: enabled -Z bypass did not exec exactly once" >&2; exit 1; }
        grep -q 'stub-myrustc ran argv: --crate-name zee -Zunpretty=hir' "$STUB_LOG" \
          || { echo "FAIL: enabled -Z bypass argv not preserved" >&2; exit 1; }

        # Enabled online-sqlx bypass — kills job 6's `! sqlx_offline_truthy
        # && DATABASE_URL` arm. SQLX_OFFLINE=0 is FALSY by sqlx's own
        # parse (only "1"/"true" are offline), so this is online mode.
        : > "$STUB_LOG"
        DATABASE_URL=postgres://localhost/db SQLX_OFFLINE=0 "$wrapper" rustc \
          --crate-name esso --out-dir "$TMPDIR/sq" \
          -C extra-filename=-beefbeefbeefbeef --version
        [ "$(wc -l < "$STUB_LOG")" -eq 1 ] \
          || { echo "FAIL: enabled online-sqlx bypass did not exec exactly once" >&2; exit 1; }
        grep -q 'stub-rustc ran argv: --crate-name esso' "$STUB_LOG" \
          || { echo "FAIL: online-sqlx bypass argv not preserved" >&2; exit 1; }

        # No-HOME arm — kills the `if [ "$no_store" = 1 ]` run_real
        # bypass: the stub must run exactly once with KACHE_DISABLED
        # still unset (the `export KACHE_DISABLED=1` fall-through line
        # comes AFTER the bypass; a deleted bypass reaches it and the
        # stub's env dump shows '1').
        : > "$STUB_LOG"
        env -u HOME "$wrapper" rustc --crate-name nohome --version
        [ "$(wc -l < "$STUB_LOG")" -eq 1 ] \
          || { echo "FAIL: no-HOME bypass did not exec exactly once" >&2; exit 1; }
        grep -q 'KACHE_DISABLED=<unset>' "$STUB_LOG" \
          || { echo "FAIL: no-HOME bypass ran after the disabled fall-through export" >&2; exit 1; }
        grep -q 'KACHE_CACHE_DIR=<unset>' "$STUB_LOG" \
          || { echo "FAIL: no-HOME invocation exported a store anyway" >&2; exit 1; }

        # Foreign-env scrub — kills the job-1 allowlist loop: a
        # pre-exported KACHE_MAX_SIZE (kache's OWN var, not the RIO
        # one) must be scrubbed, while job 2's pinned KACHE_CONFIG and
        # job 4's wrapper-authored default KACHE_CACHE_DIR survive to
        # the exec (scrub-before-export ordering: a reorder would eat
        # both wrapper-authored values too).
        : > "$STUB_LOG"
        KACHE_CACHE_DIR=/foreign/store KACHE_MAX_SIZE=1KiB KACHE_DISABLED=1 \
          "$wrapper" rustc --crate-name scrubbed --version
        grep -q 'KACHE_MAX_SIZE=<unset>' "$STUB_LOG" \
          || { echo "FAIL: foreign KACHE_MAX_SIZE survived the scrub" >&2; exit 1; }
        grep -qF "KACHE_CACHE_DIR=$HOME/.cache/rio-build/kache/$salt " "$STUB_LOG" \
          || { echo "FAIL: KACHE_CACHE_DIR is not the wrapper-authored default (scrub/export order broken?)" >&2; exit 1; }
        grep -q 'KACHE_CONFIG=/nix/store/.*kache-config\.toml' "$STUB_LOG" \
          || { echo "FAIL: pinned KACHE_CONFIG missing at the exec boundary" >&2; exit 1; }

        # RIO_KACHE_MAX_SIZE mapping — kills the job-3 export (valid
        # value mapped) and its shape validation (garbage warned +
        # dropped, never mapped).
        : > "$STUB_LOG"
        RIO_KACHE_MAX_SIZE=123GiB KACHE_DISABLED=1 "$wrapper" rustc \
          --crate-name sized --version
        grep -q 'KACHE_MAX_SIZE=123GiB' "$STUB_LOG" \
          || { echo "FAIL: valid RIO_KACHE_MAX_SIZE not mapped" >&2; exit 1; }
        : > "$STUB_LOG"
        RIO_KACHE_MAX_SIZE=1.5GiB KACHE_DISABLED=1 "$wrapper" rustc \
          --crate-name badsize --version 2>"$TMPDIR/err.size"
        grep -q 'ignoring RIO_KACHE_MAX_SIZE' "$TMPDIR/err.size" \
          || { echo "FAIL: garbage RIO_KACHE_MAX_SIZE did not warn" >&2; exit 1; }
        grep -q 'KACHE_MAX_SIZE=<unset>' "$STUB_LOG" \
          || { echo "FAIL: garbage RIO_KACHE_MAX_SIZE was mapped anyway" >&2; exit 1; }

        # Job-4 liveness stamp on an ENABLED compile — kills the
        # rio_store_stamp call in job 4 (the -Z bypass keeps the
        # scenario hermetic; the stamp lands BEFORE routing).
        export HOME=$TMPDIR/home2
        mkdir -p "$HOME"
        RUSTC=myrustc "$wrapper" myrustc --crate-name stampy -Zfoo --version
        [ -e "$HOME/.cache/rio-build/kache/$salt/.last-used" ] \
          || { echo "FAIL: enabled compile did not stamp the default epoch" >&2; exit 1; }
        export HOME=$TMPDIR/home

        # RUSTC_WORKSPACE_WRAPPER chain — kills the is_compiler
        # workspace-wrapper equality arm: for workspace members cargo
        # invokes RUSTC_WRAPPER with argv[1] = the workspace-wrapper
        # value VERBATIM and argv[2] = rustc (verified empirically).
        # The bypass must exec the chain as cargo composed it (the
        # stub sees rustc as ITS argv[1]) with NO tripwire warning —
        # a deleted arm leaves every bypass dead and the compile-
        # shaped tripwire fires.
        : > "$STUB_LOG"
        RUSTC_WORKSPACE_WRAPPER=wkspc-wrap KACHE_DISABLED=1 "$wrapper" wkspc-wrap rustc \
          --crate-name member --out-dir "$TMPDIR/ww" \
          -C extra-filename=-feedfeedfeedfeed --version 2>"$TMPDIR/err.ww"
        if grep -q 'not a recognized compiler' "$TMPDIR/err.ww"; then
          echo "FAIL: tripwire fired for an honored RUSTC_WORKSPACE_WRAPPER" >&2; exit 1
        fi
        [ "$(wc -l < "$STUB_LOG")" -eq 1 ] \
          || { echo "FAIL: workspace-wrapper bypass did not exec exactly once" >&2; exit 1; }
        grep -q 'stub-wkspc-wrap ran argv: rustc --crate-name member' "$STUB_LOG" \
          || { echo "FAIL: workspace-wrapper chain not preserved (rustc must be its argv[1])" >&2; exit 1; }

        # ---- Tripwire shape gate (kills the compile-marker conjunct
        # `{ out_dir || extra_raw }`) ----

        # The repo's own documented CLI flow `kache purge --crate-name
        # <crate>` carries --crate-name but can carry neither --out-dir
        # nor -C extra-filename (its whole clap surface is
        # --crate-name/--help) — it must NOT warn. --help keeps it
        # side-effect-free: clap prints help, no purge executes.
        "$wrapper" purge --crate-name zzz-probe --help >/dev/null 2>"$TMPDIR/err.purge" || true
        if grep -q 'not a recognized compiler' "$TMPDIR/err.purge"; then
          echo "FAIL: tripwire fired on the documented kache purge flow" >&2; exit 1
        fi
        # Probe shape: --crate-name ___ + --print, no out-dir/extra
        # (cargo's toolchain probe carries --crate-name too) — silent
        # even with a foreign argv[1].
        KACHE_DISABLED=1 "$wrapper" not-a-compiler --crate-name ___ \
          --print=file-names 2>"$TMPDIR/err.probe" || true
        if grep -q 'not a recognized compiler' "$TMPDIR/err.probe"; then
          echo "FAIL: tripwire fired on a probe-shaped invocation (no out-dir/extra)" >&2; exit 1
        fi

        # Odd-width extra-filename — kills the exactly-16-hex length
        # check: an 8-hex value must be rejected by shape validation
        # and take the conservative -links +1 arm, so an nlink-1 0444
        # canary whose name matches the would-be `*-deadbeef*` glob
        # survives (a relaxed check renders the over-broad glob and
        # deletes it).
        ei=$TMPDIR/ei
        mkdir -p "$ei"
        echo c > "$ei/librio_x-deadbeef.rlib"
        chmod 0444 "$ei/librio_x-deadbeef.rlib"
        KACHE_DISABLED=1 "$wrapper" rustc --crate-name rio_x --out-dir "$ei" \
          -C extra-filename=-deadbeef --version 2>/dev/null
        [ -e "$ei/librio_x-deadbeef.rlib" ] \
          || { echo "FAIL: 8-hex extra-filename took the extra-anchored arm (length check relaxed?)" >&2; exit 1; }
        touch $out
      '';

  # The entry-time epoch GC (rio-kache-epoch-gc) the shellHook runs —
  # scratch-HOME simulations of the stamp/seed/prune contract,
  # including the merged_bug_011 shapes (nested keep-alive, disabled-
  # but-configured re-stamp, disabled seed gate via the passthru'd
  # salt, read-only HOME) and the canonical-resolution contract: ALL
  # store compares are canonical-vs-canonical (aliased config
  # spellings — intermediate-symlinked HOME/.cache, `//`, `/./`,
  # symlink VALUES — keep-alive and skip-protect the same physical
  # store), a symlinked FINAL root component is canonicalized before
  # the walk (epochs behind it are MANAGED: stale ones prune,
  # configured ones survive), exact-root/ancestor configs disable the
  # prune entirely with a warning, relative configs warn + fall back
  # to the default-seed arm (wrapper parity), deletion is tombstone-
  # first (stray .reap-* swept, none left behind), and depth-1
  # symlink epochs are unmanaged (never stamped through, never
  # deleted). Each scenario names the guard line whose deletion turns
  # it red.
  kache-epoch-gc-test = pkgs.runCommand "rio-kache-epoch-gc-test" { } ''
    set -euo pipefail
    gc=${rioKacheEpochGc}/bin/rio-kache-epoch-gc
    salt=${kacheEnvSalt}
    ref=$TMPDIR/ref
    touch -d '1 hour ago' "$ref"

    # S1: nested configured store root/<12hex>/store — depth-3 stamp
    # fresh, depth-2 stamp 20 days stale. While configured the parent
    # must survive AND its depth-2 stamp (the one the prune reads)
    # must be refreshed.
    export HOME=$TMPDIR/h1
    root=$HOME/.cache/rio-build/kache
    mkdir -p "$root/aaaaaaaaaaaa/store"
    touch "$root/aaaaaaaaaaaa/store/.last-used"
    touch -d '20 days ago' "$root/aaaaaaaaaaaa/.last-used"
    RIO_KACHE_CACHE_DIR="$root/aaaaaaaaaaaa/store" "$gc"
    [ -d "$root/aaaaaaaaaaaa/store" ] \
      || { echo "FAIL S1: configured nested store pruned" >&2; exit 1; }
    [ "$root/aaaaaaaaaaaa/.last-used" -nt "$ref" ] \
      || { echo "FAIL S1: depth-2 stamp not refreshed" >&2; exit 1; }

    # S2: same tree UNCONFIGURED → pruned (the documented idle-aging
    # contract; the fresh depth-3 stamp is invisible to the prune by
    # design).
    export HOME=$TMPDIR/h2
    root=$HOME/.cache/rio-build/kache
    mkdir -p "$root/bbbbbbbbbbbb/store"
    touch "$root/bbbbbbbbbbbb/store/.last-used"
    touch -d '20 days ago' "$root/bbbbbbbbbbbb/.last-used"
    "$gc"
    [ ! -e "$root/bbbbbbbbbbbb" ] \
      || { echo "FAIL S2: idle unconfigured epoch not pruned" >&2; exit 1; }

    # S3: disabled+configured → re-stamped (BOTH stamps), never
    # pruned: configuring the path is explicit intent.
    export HOME=$TMPDIR/h3
    root=$HOME/.cache/rio-build/kache
    mkdir -p "$root/cccccccccccc/store"
    touch -d '20 days ago' "$root/cccccccccccc/.last-used" \
      "$root/cccccccccccc/store/.last-used"
    KACHE_DISABLED=1 RIO_KACHE_CACHE_DIR="$root/cccccccccccc/store" "$gc"
    [ -d "$root/cccccccccccc/store" ] \
      || { echo "FAIL S3: disabled+configured store pruned" >&2; exit 1; }
    [ "$root/cccccccccccc/store/.last-used" -nt "$ref" ] \
      || { echo "FAIL S3: configured re-stamp still gated on KACHE_DISABLED" >&2; exit 1; }
    [ "$root/cccccccccccc/.last-used" -nt "$ref" ] \
      || { echo "FAIL S3: depth-2 stamp not refreshed while disabled" >&2; exit 1; }

    # S4: disabled+unconfigured does NOT seed the default salt epoch
    # (gate intact); enabled+unconfigured seeds it.
    export HOME=$TMPDIR/h4
    root=$HOME/.cache/rio-build/kache
    mkdir -p "$root"
    KACHE_DISABLED=1 "$gc"
    [ ! -e "$root/$salt" ] \
      || { echo "FAIL S4: disabled entry seeded the salt epoch" >&2; exit 1; }
    "$gc"
    [ -e "$root/$salt/.last-used" ] \
      || { echo "FAIL S4: enabled entry did not seed the salt epoch" >&2; exit 1; }

    # S5: hygiene — stale 12-hex sibling pruned; non-hex dirname
    # untouched even with a stale stamp; stampless 12-hex dir is
    # seeded, never deleted.
    export HOME=$TMPDIR/h5
    root=$HOME/.cache/rio-build/kache
    mkdir -p "$root/dddddddddddd" "$root/mystore" "$root/eeeeeeeeeeee"
    touch -d '20 days ago' "$root/dddddddddddd/.last-used" "$root/mystore/.last-used"
    "$gc"
    [ ! -e "$root/dddddddddddd" ] \
      || { echo "FAIL S5: stale 12-hex epoch survived" >&2; exit 1; }
    [ -d "$root/mystore" ] \
      || { echo "FAIL S5: non-hex dirname reaped" >&2; exit 1; }
    [ -e "$root/eeeeeeeeeeee/.last-used" ] \
      || { echo "FAIL S5: stampless 12-hex dir not seeded" >&2; exit 1; }

    # S6 (REWRITTEN for the canonical-root contract — kills the
    # rio_store_resolve canonicalization of the prune root): the
    # root's FINAL component is a symlink. The root is canonicalized
    # BEFORE the walk, so epochs behind the link are MANAGED — the
    # stale unconfigured one IS pruned (the old contract leaked it
    # forever), while a store CONFIGURED through the link survives via
    # canonical compare and refreshes the depth-2 stamp at the
    # CANONICAL location. With a lexical root, the walk would not
    # cross the link and the through-the-link config spelling would
    # never match.
    export HOME=$TMPDIR/h6
    mkdir -p "$HOME/.cache/rio-build" \
      "$TMPDIR/realtree/ffffffffffff" "$TMPDIR/realtree/gggggggggggg/store"
    touch -d '20 days ago' "$TMPDIR/realtree/ffffffffffff/.last-used" \
      "$TMPDIR/realtree/gggggggggggg/.last-used"
    ln -s "$TMPDIR/realtree" "$HOME/.cache/rio-build/kache"
    RIO_KACHE_CACHE_DIR="$HOME/.cache/rio-build/kache/gggggggggggg/store" "$gc"
    [ ! -e "$TMPDIR/realtree/ffffffffffff" ] \
      || { echo "FAIL S6: stale epoch behind a symlinked root not pruned (root not canonicalized?)" >&2; exit 1; }
    [ -d "$TMPDIR/realtree/gggggggggggg/store" ] \
      || { echo "FAIL S6: configured store behind a symlinked root pruned" >&2; exit 1; }
    [ "$TMPDIR/realtree/gggggggggggg/.last-used" -nt "$ref" ] \
      || { echo "FAIL S6: canonical depth-2 stamp not refreshed through the link" >&2; exit 1; }

    # S6b: INTERMEDIATE symlink (HOME/.cache → real dir) — pruning
    # works behind it (canonicalized the same as every component now;
    # the kernel resolved it even under the old lexical root).
    export HOME=$TMPDIR/h7
    mkdir -p "$HOME" "$TMPDIR/realcache/rio-build/kache/cafecafecafe"
    touch -d '20 days ago' "$TMPDIR/realcache/rio-build/kache/cafecafecafe/.last-used"
    ln -s "$TMPDIR/realcache" "$HOME/.cache"
    "$gc"
    [ ! -e "$TMPDIR/realcache/rio-build/kache/cafecafecafe" ] \
      || { echo "FAIL S6b: stale epoch behind an intermediate symlink not pruned" >&2; exit 1; }

    # S7: read-only HOME tree — every statement is failure-guarded,
    # so the GC must still exit 0 (set -e here would surface a bare
    # failing mkdir/touch).
    export HOME=$TMPDIR/h8
    root=$HOME/.cache/rio-build/kache
    mkdir -p "$root"
    chmod a-w "$root" "$HOME/.cache/rio-build" "$HOME/.cache" "$HOME"
    "$gc"
    chmod -R u+w "$HOME"

    # S8 (exact-root/ancestor — kills the RIO_STORE_COVERS_ROOT early
    # exit): a configured value equal to the prune root, or an
    # ancestor of it, makes every 12-hex child part of the configured
    # store — the GC must warn and skip the prune ENTIRELY. The
    # 20-day-stale child survives with NO fresh stamp anywhere, so
    # skipping alone is load-bearing; deleting the early exit prunes
    # it (the in-loop skip can't save it: root/ is not a prefix-match
    # of root/<child>/).
    export HOME=$TMPDIR/h9
    root=$HOME/.cache/rio-build/kache
    mkdir -p "$root/abcdefabcdef"
    touch -d '20 days ago' "$root/abcdefabcdef/.last-used"
    # Pre-existing tombstone: must be swept even on the COVERS_ROOT
    # path (kills the sweep line placed ABOVE the early exit —
    # .reap-* names are exclusively the GC's own debris).
    mkdir -p "$root/.reap-deadbeef0000.99"
    RIO_KACHE_CACHE_DIR="$root" "$gc" 2>"$TMPDIR/s8.err"
    grep -q 'covers the epoch root' "$TMPDIR/s8.err" \
      || { echo "FAIL S8: exact-root config did not warn" >&2; exit 1; }
    [ -d "$root/abcdefabcdef" ] \
      || { echo "FAIL S8: child epoch pruned under an exact-root config" >&2; exit 1; }
    [ ! -e "$root/.reap-deadbeef0000.99" ] \
      || { echo "FAIL S8: tombstone not swept under exact-root config" >&2; exit 1; }
    RIO_KACHE_CACHE_DIR="$HOME/.cache/rio-build" "$gc" 2>"$TMPDIR/s8b.err"
    grep -q 'covers the epoch root' "$TMPDIR/s8b.err" \
      || { echo "FAIL S8: ancestor config did not warn" >&2; exit 1; }
    [ -d "$root/abcdefabcdef" ] \
      || { echo "FAIL S8: child epoch pruned under an ancestor config" >&2; exit 1; }

    # S9 (relative override — kills the shared warn+unset+fall-back
    # posture in rio_store_resolve): the GC must warn (wrapper
    # parity), seed + stamp the DEFAULT salt epoch (the old code
    # silently no-op'd: no warning, no seed), and the prune must
    # still run normally (planted stale sibling pruned).
    export HOME=$TMPDIR/h10
    root=$HOME/.cache/rio-build/kache
    mkdir -p "$root/dddddddddddd"
    touch -d '20 days ago' "$root/dddddddddddd/.last-used"
    RIO_KACHE_CACHE_DIR=foo/bar "$gc" 2>"$TMPDIR/s9.err"
    grep -q 'ignoring relative RIO_KACHE_CACHE_DIR' "$TMPDIR/s9.err" \
      || { echo "FAIL S9: relative override did not warn" >&2; exit 1; }
    [ -e "$root/$salt/.last-used" ] \
      || { echo "FAIL S9: default salt epoch not seeded after relative fall-back" >&2; exit 1; }
    [ ! -e "$root/dddddddddddd" ] \
      || { echo "FAIL S9: stale sibling not pruned under a relative override" >&2; exit 1; }

    # S10 (aliasing trio — kills the canonical rewrite of
    # RIO_KACHE_CACHE_DIR in rio_store_resolve): every alias spelling
    # of a store parked under the root must keep-alive and
    # skip-protect the SAME physical directory the old lexical
    # compares deleted.
    # (i) intermediate-symlinked HOME/.cache + CANONICAL-real config
    # spelling (the spelling the lexical HOME-prefix gates rejected).
    export HOME=$TMPDIR/h11
    mkdir -p "$HOME" "$TMPDIR/realcache11/rio-build/kache/111111111111/store"
    touch -d '20 days ago' "$TMPDIR/realcache11/rio-build/kache/111111111111/.last-used"
    ln -s "$TMPDIR/realcache11" "$HOME/.cache"
    RIO_KACHE_CACHE_DIR="$TMPDIR/realcache11/rio-build/kache/111111111111/store" "$gc"
    [ -d "$TMPDIR/realcache11/rio-build/kache/111111111111/store" ] \
      || { echo "FAIL S10i: canonically-spelled store behind symlinked .cache pruned" >&2; exit 1; }
    [ "$TMPDIR/realcache11/rio-build/kache/111111111111/.last-used" -nt "$ref" ] \
      || { echo "FAIL S10i: depth-2 stamp not refreshed at the canonical location" >&2; exit 1; }
    # (ii) `//` (nested — the depth-2 stamp the prune reads must be
    # refreshed despite the empty path component) and `/./` (the old
    # lexical top-derivation produced "." and a stray root-level
    # stamp).
    export HOME=$TMPDIR/h12
    root=$HOME/.cache/rio-build/kache
    mkdir -p "$root/222222222222/store"
    touch -d '20 days ago' "$root/222222222222/.last-used"
    RIO_KACHE_CACHE_DIR="$root//222222222222/store" "$gc"
    [ -d "$root/222222222222/store" ] \
      || { echo "FAIL S10ii: //-spelled nested store pruned" >&2; exit 1; }
    [ "$root/222222222222/.last-used" -nt "$ref" ] \
      || { echo "FAIL S10ii: depth-2 stamp not refreshed for //-spelling" >&2; exit 1; }
    export HOME=$TMPDIR/h12b
    root=$HOME/.cache/rio-build/kache
    mkdir -p "$root/333333333333"
    touch -d '20 days ago' "$root/333333333333/.last-used"
    RIO_KACHE_CACHE_DIR="$root/./333333333333" "$gc"
    [ -d "$root/333333333333" ] \
      || { echo "FAIL S10ii: /./-spelled store pruned" >&2; exit 1; }
    [ "$root/333333333333/.last-used" -nt "$ref" ] \
      || { echo "FAIL S10ii: stamp not refreshed for /./-spelling" >&2; exit 1; }
    [ ! -e "$root/.last-used" ] \
      || { echo "FAIL S10ii: stray root-level stamp created (lexical top-derivation back?)" >&2; exit 1; }
    # (iii) symlink VALUE → root/<ep>: the TARGET must be stamped and
    # survive (the old gates mismatched on the link spelling and the
    # prune deleted the target, leaving the config dangling).
    export HOME=$TMPDIR/h13
    root=$HOME/.cache/rio-build/kache
    mkdir -p "$root/444444444444"
    touch -d '20 days ago' "$root/444444444444/.last-used"
    ln -s "$root/444444444444" "$TMPDIR/the-link"
    RIO_KACHE_CACHE_DIR="$TMPDIR/the-link" "$gc"
    [ -d "$root/444444444444" ] \
      || { echo "FAIL S10iii: symlink-valued config's target pruned" >&2; exit 1; }
    [ "$root/444444444444/.last-used" -nt "$ref" ] \
      || { echo "FAIL S10iii: symlink-valued config's target not stamped" >&2; exit 1; }

    # S11 (tombstone hygiene — kills the entry-time .reap-* sweep AND
    # the mv→rm pairing): a stray tombstone from an interrupted prune
    # is swept; a freshly pruned epoch leaves NO tombstone behind;
    # the tombstone name is never seeded as an epoch (dot-prefixed,
    # non-12hex).
    export HOME=$TMPDIR/h14
    root=$HOME/.cache/rio-build/kache
    mkdir -p "$root/.reap-cafecafecafe.999/junk" "$root/555555555555"
    touch -d '20 days ago' "$root/555555555555/.last-used"
    "$gc"
    [ ! -e "$root/555555555555" ] \
      || { echo "FAIL S11: stale epoch survived the tombstone-first prune" >&2; exit 1; }
    [ "$(find "$root" -maxdepth 1 -name '.reap-*' | wc -l)" -eq 0 ] \
      || { echo "FAIL S11: .reap-* tombstones left behind (sweep or post-mv rm deleted?)" >&2; exit 1; }

    # S12 (depth-1 symlink epoch — kills the seed loop's -L skip): a
    # salt-shaped symlink at depth 1 is UNMANAGED — the seed loop must
    # not stamp THROUGH it (the old loop touch'd elsewhere/.last-used
    # through the link, granting a foreign directory a 14-day lease
    # while find -P could never prune it), and the link itself stays.
    export HOME=$TMPDIR/h15
    root=$HOME/.cache/rio-build/kache
    mkdir -p "$root" "$TMPDIR/elsewhere"
    ln -s "$TMPDIR/elsewhere" "$root/666666666666"
    "$gc"
    [ ! -e "$TMPDIR/elsewhere/.last-used" ] \
      || { echo "FAIL S12: seed loop stamped through a depth-1 symlink epoch" >&2; exit 1; }
    [ -L "$root/666666666666" ] \
      || { echo "FAIL S12: depth-1 symlink epoch removed" >&2; exit 1; }

    # S13 (the prune-loop configured-skip `continue` is load-bearing —
    # kills exactly that line): construct "stale stamp + failed
    # refresh" with a DANGLING-SYMLINK depth-2 stamp backdated via
    # touch -h. rio_store_stamp's depth-2 touch follows the link and
    # fails (guarded); the seed loop's -e test follows it too (false)
    # and its touch also fails; find -P lstats the link itself —
    # 20 days old, name and regex match — and selects the epoch. Only
    # the in-loop skip keeps the configured store alive here; without
    # it the epoch is tombstoned and reaped while configured.
    export HOME=$TMPDIR/h16
    root=$HOME/.cache/rio-build/kache
    mkdir -p "$root/777777777777/store"
    ln -s "$TMPDIR/nonexistent-target/stamp" "$root/777777777777/.last-used"
    touch -h -d '20 days ago' "$root/777777777777/.last-used"
    RIO_KACHE_CACHE_DIR="$root/777777777777/store" "$gc"
    [ -d "$root/777777777777/store" ] \
      || { echo "FAIL S13: configured store reaped when the stamp refresh fails (skip not load-bearing?)" >&2; exit 1; }
    touch $out
  '';
}
