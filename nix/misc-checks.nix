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

  # helm-env-schema-parity (merged_bug_161 class close): a Rust Config
  # field that is neither rendered as a chart env nor allowlisted with
  # a why-comment fails the merge gate — the
  # knob-in-Rust-forgotten-in-chart class (RIO_LOG_RETENTION_DAYS /
  # RIO_LOG_CORS_ALLOW_ORIGINS shipped exactly that way) cannot recur
  # silently. Formal-coverage rationale (none-sensible): this is an
  # external-equation tier (rendered YAML vs committed schema fixture);
  # the executable check IS the by-construction artifact.
  helm-env-schema-parity =
    let
      chart = pkgs.lib.cleanSource ../infra/helm/rio-build;
      fixtures = {
        rio-scheduler = ../rio-scheduler/tests/fixtures/config-schema.json;
        rio-store = ../rio-store/tests/fixtures/config-schema.json;
        rio-gateway = ../rio-gateway/tests/fixtures/config-schema.json;
        rio-controller = ../rio-controller/tests/fixtures/config-schema.json;
      };
      pairs = pkgs.lib.concatStringsSep " " (
        pkgs.lib.mapAttrsToList (component: fixture: "${component}=${fixture}") fixtures
      );
    in
    pkgs.runCommand "rio-helm-env-schema-parity"
      {
        nativeBuildInputs = [
          pkgs.kubernetes-helm
          pkgs.python3
        ];
      }
      ''
        cp -r ${chart} $TMPDIR/chart
        chmod -R +w $TMPDIR/chart
        cd $TMPDIR/chart
        mkdir -p charts
        ln -s ${subcharts.postgresql} charts/postgresql
        helm template rio . --set global.image.tag=test > $TMPDIR/render.yaml
        python3 ${./tests/helm-env-parity.py} $TMPDIR/render.yaml           ${./tests/helm/env-parity-allowlist.json} ${pairs}
        touch $out
      '';

  # merged_bug_284: the scheduler db mutator surface is crate-private
  # and dead_code keeps authority. The two module-level
  # #[allow(dead_code)] shields ("until Wave-3 wiring" — long expired)
  # came off; every shielded fn was deleted (insert_drv_execution, the
  # FencedTx::serving_generation accessor) or wired-with-justification
  # (#[cfg(test)] battery twins, each why-commented). ONE public
  # mutator is allowlisted: delete_samples_older_than — the bin
  # target's build-samples retention sweep calls it from outside the
  # lib crate. Formal-coverage rationale (none-sensible): visibility
  # policy; the tripwire is the closure.
  db-mutator-visibility =
    pkgs.runCommand "rio-db-mutator-visibility"
      {
        nativeBuildInputs = [ pkgs.gnugrep ];
        src = pkgs.lib.fileset.toSource {
          root = ../rio-scheduler/src/db;
          fileset = pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-scheduler/src/db;
        };
      }
      ''
        fail=0
        if grep -rn '^[[:space:]]*pub async fn' $src --include='*.rs' \
             | grep -v 'tests/' | grep -v 'delete_samples_older_than'; then
          echo "FAIL: public db mutator — db/ is pub(crate); only delete_samples_older_than (the main.rs retention sweep) is allowlisted" >&2
          fail=1
        fi
        if grep -rn 'allow(dead_code)' $src --include='*.rs' | grep -v 'tests/'; then
          echo "FAIL: dead_code shield in rio-scheduler/src/db/ — delete the fn or wire it with a recorded justification" >&2
          fail=1
        fi
        [[ $fail -eq 0 ]]
        touch $out
      '';

  # merged_bug_353 (lint half): every rio_* token a shipped dashboard
  # expr or PrometheusRule expr reads must be a LIVE described metric
  # (docs/gen/metrics.json .names — the describe_*! scrape). Histogram
  # _bucket/_sum/_count series resolve to their base name. Rule exprs
  # come from docs/gen/alerts.json (the same line-state scrape the
  # docs build renders — one parser, two consumers). Label-VALUE
  # validation is an explicit non-goal: this asserts name liveness,
  # not series shape. Formal-coverage rationale (none-sensible):
  # operator-mirror tier; the derived-alternation lint IS the
  # by-construction artifact.
  obs-surface-lint =
    pkgs.runCommand "rio-obs-surface-lint"
      {
        nativeBuildInputs = [
          pkgs.jq
          pkgs.gnugrep
        ];
        dashboards = ../infra/helm/rio-build/dashboards;
        alertsJson = ../docs/gen/alerts.json;
        metricsJson = ../docs/gen/metrics.json;
      }
      ''
        fail=0
        jq -r '.. | .expr? // empty' $dashboards/*.json \
          | grep -ohE '\brio_[a-z0-9_]+' \
          | sed -E 's/_(bucket|sum|count)$//' | sort -u > $TMPDIR/dash-tokens
        jq -r '.rules[].metrics[]' $alertsJson | sort -u > $TMPDIR/rule-tokens
        jq -r '.names[]' $metricsJson | sort -u > $TMPDIR/live
        for set in dash rule; do
          dead=$(comm -23 $TMPDIR/$set-tokens $TMPDIR/live)
          if [[ -n "$dead" ]]; then
            echo "FAIL: $set exprs read metrics no describe_*! macro declares (retired or typo'd):" >&2
            echo "$dead" >&2
            fail=1
          fi
        done
        [[ $fail -eq 0 ]]
        touch $out
      '';

  # merged_bug_236 (bughunt-2 slot 5, THE CLASS chokepoint): every
  # component whose metrics appear in a shipped alert/scaler expr must
  # carry the alert-parity adoption test (tests/alert_metrics.rs) — the
  # per-crate test then enforces seeding/ownership for every referenced
  # series (the bug_322 birth-gap class). This check makes ADOPTION
  # itself mechanical: a new component's first alert without the parity
  # test is CI-red here, not a silent birth gap in production.
  # Red-verified standalone pre-adoption (controller leg).
  alert-parity-adoption =
    let
      components = [
        "builder"
        "controller"
        "gateway"
        "scheduler"
        "store"
      ];
      adopted = builtins.filter (
        c: builtins.pathExists (../. + "/rio-${c}/tests/alert_metrics.rs")
      ) components;
    in
    pkgs.runCommand "rio-alert-parity-adoption"
      {
        nativeBuildInputs = [ pkgs.gawk ];
        templates = [
          ../infra/helm/rio-build/templates/prometheusrule.yaml
          ../infra/helm/rio-build/templates/store-scaledobject.yaml
          ../infra/helm/rio-build/templates/gateway-scaledobject.yaml
        ];
        inherit adopted;
      }
      ''
        # Extract rio_<component>_ prefixes from expr:/query: blocks only
        # (annotations/descriptions mentioning a metric do not count —
        # same scoping as the in-crate extractor).
        for t in $templates; do
          awk '
            function indent(line) { match(line, /[^ ]/); return RSTART - 1 }
            {
              if (intrigger) {
                # rio.promTrigger include args: quoted strings carry the
                # promql + threshold metric (same scoping as the in-crate
                # extractor).
                print
                if ($0 ~ /}}/) { intrigger = 0 }
                next
              }
              if ($0 ~ /include "rio\.promTrigger"/) {
                print
                if ($0 !~ /}}/) { intrigger = 1 }
                next
              }
              if (inblock) {
                if ($0 ~ /[^ ]/ && indent($0) <= key_indent) { inblock = 0 }
                else { print; next }
              }
              if ($0 ~ /^[ ]*(expr|query):/) {
                if ($0 ~ /:[ ]*[|>][-+]?[ ]*$/) { inblock = 1; key_indent = indent($0) }
                else { print }
              }
            }
          ' "$t"
        done | grep -ohE 'rio_[a-z]+_' | sort -u | sed -E 's/^rio_([a-z]+)_$/\1/' > $TMPDIR/referenced
        fail=0
        while read -r comp; do
          ok=0
          for a in $adopted; do
            [[ "$a" == "$comp" ]] && ok=1
          done
          if [[ $ok -eq 0 ]]; then
            echo "FAIL: rio_''${comp}_ metrics are referenced in shipped alert/scaler exprs but rio-''${comp}/tests/alert_metrics.rs does not exist — adopt the alert-parity test (see rio-scheduler/tests/alert_metrics.rs) so every referenced series is seeded/owned from boot" >&2
            fail=1
          fi
        done < $TMPDIR/referenced
        [[ $fail -eq 0 ]]
        touch $out
      '';

  # bug_030: migration bodies are DDL plus exactly one commentary
  # pointer. For NNN >= 082: line 1 is verbatim
  # `-- Commentary: see rio-migrations/src/migrations.rs M_NNN` (NNN
  # matching the filename), no other line-anchored `--` lines
  # (trailing inline `-- ...` after DDL is fine), and the M_NNN
  # doc-const exists. Rationale: sqlx checksums the full body —
  # commentary edits to a shipped migration brick persistent-DB
  # deploys with VersionMismatch, so prose lives in migrations.rs
  # where it can evolve. 082/083 were rewritten (unshipped on this
  # branch) and re-pinned; 084+ were born compliant. Formal-coverage
  # rationale (none-sensible): file-format policy; the lint is the
  # closure.
  migration-body-policy =
    pkgs.runCommand "rio-migration-body-policy"
      {
        nativeBuildInputs = [ pkgs.gnugrep ];
        migrations = ../rio-migrations/migrations;
        commentary = ../rio-migrations/src/migrations.rs;
      }
      ''
        fail=0
        for f in $migrations/*.sql; do
          base=$(basename "$f" .sql)
          nnn=''${base%%_*}
          [[ "$((10#$nnn))" -ge 82 ]] || continue
          want="-- Commentary: see rio-migrations/src/migrations.rs M_$nnn"
          first=$(head -n1 "$f")
          if [[ "$first" != "$want" ]]; then
            echo "FAIL: $base.sql line 1 is not the M_$nnn pointer:" >&2
            echo "  have: $first" >&2
            echo "  want: $want" >&2
            fail=1
          fi
          if tail -n +2 "$f" | grep -n '^--'; then
            echo "FAIL: $base.sql carries line-anchored comment lines beyond the pointer — commentary belongs in M_$nnn (rio-migrations/src/migrations.rs)" >&2
            fail=1
          fi
          if ! grep -q "pub const M_$nnn: () = ();" $commentary; then
            echo "FAIL: M_$nnn doc-const missing in rio-migrations/src/migrations.rs" >&2
            fail=1
          fi
        done
        [[ $fail -eq 0 ]]
        touch $out
      '';

  # bug_330 / merged_bug_353 (drift half): the chart's metric
  # semantics are RENDERED from the describe_*! HELP scrape
  # (`xtask regen helm-obs` → generated/metric-help.json + in-place
  # rioMetric panel descriptions). This re-runs the regen hermetically
  # (docsData runCommand shape, nix/docs.nix) and diffs — a HELP edit
  # without regen, or a hand-edited generated description, fails the
  # merge gate. Formal-coverage rationale (none-sensible): operator
  # mirror tier — the generation+drift pipeline IS the by-construction
  # artifact; a model would verify a transcription of the same scrape.
  helm-obs-drift =
    pkgs.runCommand "rio-helm-obs-drift"
      {
        nativeBuildInputs = [
          xtaskBin
          pkgs.diffutils
        ];
        src = pkgs.lib.fileset.toSource {
          root = unfilteredRoot;
          fileset = pkgs.lib.fileset.unions (
            [
              ../infra/helm/rio-build/dashboards
              ../infra/helm/rio-build/generated
            ]
            ++ map (m: pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") (../. + "/${m}/src")) (
              builtins.filter (m: pkgs.lib.hasPrefix "rio-" m)
                (builtins.fromTOML (builtins.readFile ../Cargo.toml)).workspace.members
            )
          );
        };
      }
      ''
        cp -r --no-preserve=mode $src work
        export RIO_REPO_ROOT=$PWD/work
        xtask regen helm-obs
        for d in generated dashboards; do
          diff -r work/infra/helm/rio-build/$d $src/infra/helm/rio-build/$d > $TMPDIR/diff-$d || {
            echo "FAIL: infra/helm/rio-build/$d is stale vs the describe_*! HELP scrape" >&2
            echo "Run: cargo xtask regen helm-obs" >&2
            cat $TMPDIR/diff-$d >&2
            exit 1
          }
        done
        touch $out
      '';

  # proxy_buffering off in dashboardNginxConf is LOAD-BEARING
  # (docker.nix:349): nginx default-buffers upstream → WatchBuild /
  # TailLog streams arrive as one blob at close. The config is a
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
    # The conf now carries ENVSUBST PLACEHOLDERS for the upstream
    # FQDNs (generated from files/dashboard-upstreams.json; the image
    # entrypoint substitutes at pod start) — run the SAME substitution
    # the entrypoint performs, with the env defaulted to a resolvable
    # address, so the guard checks the runtime artifact, not the
    # template. /dev/std{err,out} → TMPDIR (a remote build sandbox may
    # not provide /dev/std*). Everything else is checked verbatim.
    mkdir -p $TMPDIR/logs
    RIO_SCHEDULER_FQDN=127.0.0.1 RIO_STORE_FQDN=127.0.0.1 \
      ${pkgs.gettext}/bin/envsubst '$RIO_SCHEDULER_FQDN $RIO_STORE_FQDN' \
      < ${dockerImages.dashboardNginxConf} > $TMPDIR/nginx-subst.conf
    if grep -F '{RIO_' $TMPDIR/nginx-subst.conf; then
      echo "FAIL: unsubstituted upstream placeholder survived envsubst — keep the entrypoint var list in sync with dashboard-upstreams.json" >&2
      exit 1
    fi
    sed -e "s#/dev/stderr#$TMPDIR/logs/error.log#" \
        -e "s#/dev/stdout#$TMPDIR/logs/access.log#" \
      $TMPDIR/nginx-subst.conf > $TMPDIR/nginx.conf
    ${dockerImages.dashboardNginx}/bin/nginx -t -p $TMPDIR -c $TMPDIR/nginx.conf
    touch $out
  '';

  # B1 bounded-await-transport: the four ExecutorService unaries may be
  # called directly ONLY inside the two transport impls
  # (AuthedPullTransport in rio-builder/src/runtime/pull.rs;
  # SchedulerTransport in rio-store/src/materialize/client.rs). Every
  # other call site must go through the transport trait so the loop
  # consumes `rio_common::transport::bounded` outcomes — a bare
  # `.pull_assignment(req).await` in a loop is exactly the
  # accepted-never-answered hang class (merged_bug_167/189). Source-grep
  # enforcement (the rio-proto h2_throughput precedent): clippy
  # disallowed-methods cannot name tonic-generated generic methods
  # reliably.
  # B2 log-ingest-authority (merged_bug_111): the kind-filtered
  # `latest_build_exec` view (migration 089) is THE resolver for "the
  # derivation's latest build execution". A new raw `ORDER BY exec_id
  # DESC` read of drv_executions is a kind-blind copy of that
  # resolution waiting to serve a materialization mint's empty log —
  # ban it outside migrations (the view definition itself).
  log-no-raw-latest-exec =
    pkgs.runCommand "rio-log-no-raw-latest-exec"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-store/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-scheduler/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-gateway/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-cli/src)
          ];
        };
        nativeBuildInputs = [ pkgs.ripgrep ];
      }
      ''
        set +o pipefail
        hits=$(rg -n -i 'FROM drv_executions[^;]*ORDER BY exec_id DESC' --multiline           $src/rio-store/src $src/rio-scheduler/src $src/rio-gateway/src $src/rio-cli/src           || true)
        if [[ -n "$hits" ]]; then
          echo "FAIL: raw latest-exec resolution over drv_executions —" >&2
          echo "read the kind-filtered latest_build_exec view instead (M_089):" >&2
          echo "$hits" >&2
          exit 1
        fi
        touch $out
      '';

  transport-unary-ban =
    pkgs.runCommand "rio-transport-unary-ban"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-builder/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-store/src)
          ];
        };
        nativeBuildInputs = [ pkgs.ripgrep ];
      }
      ''
        set +o pipefail
        hits=$(rg -n '\.(pull_assignment|report_outcome|list_materialization_jobs|report_materialization_progress)\(' \
          $src/rio-builder/src $src/rio-store/src \
          | grep -v 'rio-builder/src/runtime/pull\.rs' \
          | grep -v 'rio-store/src/materialize/client\.rs' || true)
        if [[ -n "$hits" ]]; then
          echo "FAIL: direct ExecutorService unary call outside the two transport impls —" >&2
          echo "route it through the transport trait (rio_common::transport::bounded):" >&2
          echo "$hits" >&2
          exit 1
        fi
        touch $out
      '';

  # Streaming-open ban: no naked generated streaming-RPC open in a
  # daemon crate. The banned-method list is DERIVED AT CHECK TIME from
  # the FileDescriptorSet — protoc's own parse over rio-proto/proto —
  # so a NEW streaming rpc is born banned (multi-line declarations,
  # commented-out rpcs, and request-side-only streams are classified by
  # descriptor flags, not regex). Vehicle: misc-check source scan, NOT
  # clippy (disallowed-methods cannot name tonic-generated generic
  # methods — transport-unary-ban's recorded rationale) and NOT a
  # client seal (tonic emits one client struct per service; a seal
  # would still need this descriptor check to force bounding of new
  # methods — the sanctioned-combinator lookbehind is the soft seal).
  # A hit is allowed iff a sanctioned bounding combinator appears
  # within the preceding 6 lines (bounded_open / with_timeout_status /
  # with_timeout / transport::bounded) or the file is a sanctioned
  # wrapper (rio-builder/src/log_upload.rs — its AppendLog conformance
  # test is named in the allowlist below). Residual: a caller could
  # evade the 6-line lookbehind by spacing the combinator further away;
  # recorded, accepted (the combinator and the open read together in
  # every sanctioned shape). Test code is out of scope (cfg(test)
  # trailing modules stripped; /tests/ submodule dirs and
  # test_helpers.rs are cfg(test)-compiled). Homonym filter: FuseCache
  # get_path in rio-builder/src/fuse/ is not a gRPC open.
  # Born red on 5 census sites (2026-06-04): gateway log_tail.rs
  # tail_log + build.rs watch_build, store logs/service.rs proxy
  # tail_log, controller gc_schedule.rs trigger_gc, scheduler
  # admin/gc.rs trigger_gc — the hand census had missed the fifth;
  # the lint caught it, which is this chokepoint's own argument.
  # Keepalive single source: the h2/TCP keepalive knobs appear ONLY at
  # the two chokepoints (server: rio-common/src/server.rs; client:
  # rio-proto/src/client/mod.rs). A future per-daemon hand-chained
  # override (the rio-scheduler main.rs shape this check was born red
  # on) is CI-red, not review-caught.
  # r[verify proto.h2.keepalive-server]
  h2-keepalive-single-source =
    pkgs.runCommand "rio-h2-keepalive-single-source"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-gateway/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-store/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-scheduler/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-controller/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-builder/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-common/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-proto/src)
          ];
        };
        nativeBuildInputs = [ pkgs.ripgrep ];
      }
      ''
        set +o pipefail
        hits=$(rg -n 'http2_keep_?alive_|keep_alive_timeout|keep_alive_while_idle|tcp_keepalive'           $src --glob '*.rs'           | grep -v 'rio-common/src/server\.rs'           | grep -v 'rio-proto/src/client/mod\.rs'           | grep -v 'rio-common/src/grpc\.rs' || true)
        if [[ -n "$hits" ]]; then
          echo "FAIL: keepalive knob outside the two chokepoints — use" >&2
          echo "rio_common::server::tonic_builder / rio-proto with_h2_keepalive:" >&2
          echo "$hits" >&2
          exit 1
        fi
        # Negative self-test: a planted override MUST fire.
        mkdir -p planted && echo '.http2_keepalive_interval(Some(d))' > planted/sample.rs
        if ! rg -q 'http2_keep_?alive_' planted/sample.rs; then
          echo "FAIL: self-test — pattern missed a planted override" >&2
          exit 1
        fi
        touch $out
      '';

  # Reason-label <-> HELP sync: every literal (or same-file
  # helper-resolved) `"reason" => ...` label on a counter must appear
  # in that metric's describe_counter! HELP — an operator triaging a
  # labeled counter reads the HELP, so an unmentioned reason is an
  # undocumented failure mode. Born red on 16 drifts (2026-06-04):
  # the bug_110 headline (gap_observed missing from
  # rio_gateway_log_tail_reconnects_total) PLUS 15 siblings the class
  # lint caught across hmac_rejected / log_ingest_rejected /
  # log_ingest_streams_aborted / pull_rejected — including the
  # inbound_idle reason added by this very wave two commits earlier.
  # Out-of-scope shapes (method-call/variable reasons, dynamic metric
  # names, no-describe metrics) are CENSUSED in the build log, never
  # silently dropped. In-scanner planted self-test.
  metric-reason-help-sync =
    pkgs.runCommand "rio-metric-reason-help-sync"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-gateway/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-store/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-scheduler/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-controller/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-builder/src)
          ];
        };
        nativeBuildInputs = [ pkgs.python3 ];
      }
      ''
        python3 ${../nix/metric_reason_help_sync.py} $src
        touch $out
      '';

  # r[verify proto.client.streaming-open-bounded]
  streaming-open-ban =
    pkgs.runCommand "rio-streaming-open-ban"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-gateway/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-store/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-scheduler/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-controller/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-builder/src)
            ../rio-proto/proto
          ];
        };
        nativeBuildInputs = [
          pkgs.protobuf
          (pkgs.python3.withPackages (ps: [ ps.protobuf ]))
        ];
      }
      ''
        # 1. The banned list, from protoc's own parse.
        protoc -I $src/rio-proto/proto \
          --descriptor_set_out=fds.pb \
          $src/rio-proto/proto/*.proto

        # 2. Decode + scan + negative self-test.
        python3 ${../nix/streaming_open_ban.py} fds.pb $src
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
  # A1 fenced-write-discipline (bughunt wave) — the two textual policy
  # nets behind the FencedTx capability (the in-crate
  # db/tests/fence_coverage.rs enumeration pins db/'s own statements;
  # these pin the rest of the crate against re-introduction).
  #
  # fence-sql-canonical: the claims-floor GREATEST read
  # (MAX(generation) over assignments + leader_generation_claims)
  # exists in exactly two production files — db/mod.rs (claims_floor,
  # private to the capability) and db/recovery.rs
  # (max_known_generation, the pool-seeding read, allowlisted by
  # design). Any other occurrence is an open-coded floor read the
  # capability was built to delete (bug_269's class).
  fence-sql-canonical =
    pkgs.runCommand "rio-fence-sql-canonical"
      {
        srcDir = pkgs.lib.fileset.toSource {
          root = ../rio-scheduler/src;
          fileset = pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-scheduler/src;
        };
      }
      ''
        set -euo pipefail
        # The SQL literal signature: MAX(generation) FROM
        # leader_generation_claims (whitespace-insensitive via -z would
        # overfit; the FROM clause is the stable token).
        hits=$(grep -rl 'MAX(generation) FROM leader_generation_claims' $srcDir           | grep -v '/db/mod.rs$'           | grep -v '/db/recovery.rs$'           | grep -v '/tests/' || true)
        if [ -n "$hits" ]; then
          echo "FAIL: open-coded claims-floor SQL outside db/mod.rs + db/recovery.rs:" >&2
          echo "$hits" >&2
          echo "Decision-state writes go through SchedulerDb::begin_fenced (the FencedTx" >&2
          echo "capability); the floor read is private to it. See sched.evidence.durability." >&2
          exit 1
        fi
        touch $out
      '';

  # fence-no-raw-decision-sql: no raw write-verb SQL on the decision
  # tables (assignments, derivations, materialization_jobs,
  # build_wanted_outputs, drv_attempts) outside rio-scheduler/src/db/
  # — actor/grpc/admin code calls the fenced db-layer fns
  # (FencedTx::close_assignment, the *_fenced writers), never inline
  # SQL (merged_bug_231's class: the derivation-keyed close lived in
  # housekeeping.rs).
  fence-no-raw-decision-sql =
    pkgs.runCommand "rio-fence-no-raw-decision-sql"
      {
        srcDir = pkgs.lib.fileset.toSource {
          root = ../rio-scheduler/src;
          fileset = pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-scheduler/src;
        };
      }
      ''
        set -euo pipefail
        hits=$(grep -rlE 'UPDATE (assignments|derivations|materialization_jobs|build_wanted_outputs)|INSERT INTO drv_attempts|DELETE FROM (assignments|materialization_jobs|build_wanted_outputs)' $srcDir           | grep -v '/db/'           | grep -v '/tests/' || true)
        if [ -n "$hits" ]; then
          echo "FAIL: raw decision-table SQL outside rio-scheduler/src/db/:" >&2
          echo "$hits" >&2
          echo "Use the fenced db-layer writers (FencedTx::close_assignment," >&2
          echo "SchedulerDb::*_fenced) — see sched.evidence.durability +" >&2
          echo "db/tests/fence_coverage.rs." >&2
          exit 1
        fi
        touch $out
      '';

  # A2 kind-partition-completion (bughunt wave) — bug_266's class:
  # `assignments` joins are kind-discipline surfaces (a kind-blind
  # holder join stamps a build attempt's identity onto a
  # materialization job, or vice versa). db/open_attempts.rs is the
  # single sanctioned home for assignment joins — every join there
  # carries its kind discipline in one reviewable file (binding on
  # A3's later single-row variant too). The allowlist names the
  # grandfathered NON-HOLDER uses, each with why it is not a holder
  # join; a new `FROM/JOIN assignments` anywhere else fails the
  # policy. (Red-verified at introduction: a planted
  # `JOIN assignments` outside the allowlist fails the pipeline.)
  # A3 materialization-lifecycle-kernel (bughunt wave) — bug_067/020's
  # class: the `materialization_infra` charge class is constructible
  # ONLY inside the charge→verdict chokepoint module
  # (actor/materialize.rs, charge_materialization_infra) — a charging
  # channel that skips the park decision must not compile-and-hide.
  # state/derivation.rs (the enum definition) and retry_policy.rs (the
  # kernel mappings) NAME the variant without constructing rows; tests
  # may reference it freely.
  no-preboot-instant =
    pkgs.runCommand "rio-no-preboot-instant"
      {
        srcDir = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            ../rio-scheduler/src
            ../rio-store/src
            ../rio-builder/src
            ../rio-gateway/src
            ../rio-controller/src
            ../rio-common/src
            ../rio-lease/src
          ];
        };
      }
      ''
        set -euo pipefail
        # merged_bug_300: the silent clock re-anchor. Reconstructing a
        # past Instant via checked_sub with a fallback to "now" makes a
        # pre-boot moment read as fresh — a recovered build gets a new
        # timeout window, a recovered park restarts its dwell, a poison
        # extends its TTL. The sanctioned representation is
        # rio_scheduler::state::RecoveredInstant (age carried as data;
        # elapsed() is total). Zero allowlist.
        hits=$(grep -rn -A2 'checked_sub' $srcDir --include='*.rs'           | grep -E 'unwrap_or(_else)?\(\s*(std::time::)?Instant::now' || true)
        if [ -n "$hits" ]; then
          echo "FAIL: checked_sub with an Instant::now fallback (the silent re-anchor):" >&2
          echo "$hits" >&2
          echo "Use rio-scheduler state::RecoveredInstant (or carry the age as data)." >&2
          exit 1
        fi
        touch $out
      '';

  mat-charge-chokepoint =
    pkgs.runCommand "rio-mat-charge-chokepoint"
      {
        srcDir = pkgs.lib.fileset.toSource {
          root = ../rio-scheduler/src;
          fileset = pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-scheduler/src;
        };
      }
      ''
        set -euo pipefail
        hits=$(grep -rl 'OutcomeClass::MaterializationInfra' $srcDir           | grep -v '/actor/materialize.rs$'           | grep -v '/state/derivation.rs$'           | grep -v '/retry_policy.rs$'           | grep -v '/tests/' || true)
        if [ -n "$hits" ]; then
          echo "FAIL: OutcomeClass::MaterializationInfra outside the charge chokepoint module:" >&2
          echo "$hits" >&2
          echo "Every materialization_infra charge routes through" >&2
          echo "charge_materialization_infra (actor/materialize.rs) — the" >&2
          echo "charge+park-verdict fusion (bug_067, owner-signed Q5 reversal)." >&2
          exit 1
        fi
        touch $out
      '';

  assignments-join-policy =
    pkgs.runCommand "rio-assignments-join-policy"
      {
        srcDir = pkgs.lib.fileset.toSource {
          root = ../rio-scheduler/src;
          fileset = pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-scheduler/src;
        };
      }
      ''
        set -euo pipefail
        # Allowlisted non-holder uses:
        #   db/mod.rs        — claims-floor GREATEST read (fence capability
        #                      internals; pinned by fence-sql-canonical).
        #   db/recovery.rs   — DAG-rebuild status/exec loaders + the
        #                      pool-seeding floor read; they read the
        #                      DERIVATION's slot (any kind occupies the one
        #                      active row), never a materialization-job
        #                      holder identity.
        #   db/attempts.rs   — GC sweep NOT-EXISTS guard (E4 conjunct):
        #                      an existence anti-join, no identity read.
        #   db/derivations.rs — NOT-EXISTS guard, same shape.
        #   db/materialization.rs — claimable-list NOT-EXISTS anti-join:
        #                      ANY-kind active assignment occupies the
        #                      single active-row slot, so the job is not
        #                      claimable regardless of kind (conservative
        #                      under-offer by design; no identity read).
        hits=$(grep -rlE 'FROM assignments|JOIN assignments' $srcDir           | grep -v '/db/open_attempts.rs$'           | grep -v '/db/mod.rs$'           | grep -v '/db/recovery.rs$'           | grep -v '/db/attempts.rs$'           | grep -v '/db/derivations.rs$'           | grep -v '/db/materialization.rs$'           | grep -v '/tests/' || true)
        if [ -n "$hits" ]; then
          echo "FAIL: assignments join outside db/open_attempts.rs (the sanctioned join surface):" >&2
          echo "$hits" >&2
          echo "Holder/attempt joins against assignments carry kind discipline and live in" >&2
          echo "db/open_attempts.rs (bug_266); non-holder uses get a why-comment allowlist" >&2
          echo "entry in nix/misc-checks.nix:assignments-join-policy." >&2
          exit 1
        fi
        touch $out
      '';

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
        alertsJson = ../docs/gen/alerts.json;
        migrationsJson = ../docs/gen/migrations.json;
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
        # Bughunt-wave extension (merged_bug_019/284/203 recurrence;
        # E2 deletion-campaign): EVERY structural-deletion commit
        # appends its dead symbols here (see CLAUDE.md). Wave tokens:
        # the ReadyQueue family (A3 [H]), rearm_materialization_job
        # (A3), trim_chunk + DEFAULT_PEER_URL_TEMPLATE (B3),
        # tick_publish_gauges (A2.4), pull_attempt_seen_open (C2),
        # closure_vouched (A4), FencedWrite (A1), rollback_assignment
        # (pull-mode sweep), COLLECT_CURSOR/COLLECT_BACKLOG_ESTIMATE
        # (D1), the retired workers_active/queue_depth metric names,
        # and the deleted rio-scheduler/src/logs/ tree. Recorded
        # narrowing: the stream-era CONCEPT tokens (hard_filter,
        # assign_to_worker, BuildExecution, correlation-TTL) are NOT
        # blanket-banned — their misleading-as-live narrations were
        # rewritten to live names (placeable()/the spawn-intent
        # exclusion/the pull mint), but dozens of legitimate historical
        # citations remain (incident records, ledger-class provenance
        # docs, spec retirement framing) and a 30-entry allowlist
        # would bury the signal.
        #
        # Split into shared/docs/cross (R7-m025): a single alternation
        # over both scan sets is the structural reason "widen pattern X
        # → false-positive in the other scan set" recurs. deny_shared
        # is identifiers retired everywhere; deny_docs adds doc-only
        # phrases (legitimately appear in code as historical context);
        # deny_cross adds case/separator variants needed for nix/infra
        # that would FP docs' "Squid FOD proxy is deleted" prose.
        deny_shared='\bBuilderPool\b|\bFetcherPools?\b|rio-cli bps\b|`bps`|vm-lifecycle-bps|RIO_TLS__|\bTlsError\b|rio-common/src/tls\.rs|load_client_tls|init_client_tls|spec\.sizing|Sizing::|fuseCacheBudget|logBudget|migration-lock mechanism|trigger-gc|--grace-period-hours|mTLS client[- ]cert|mTLS cert mount|mTLS main port|VMs: mTLS|plaintext-health listener|TLS and plaintext ports|mTLS bypass|mTLS-identified|mTLS identifies|falls? back to mTLS|mTLS peer cert|\bplaintext port\b|CN-allowlist\)|\(gateway cert|dev-mode/dev-mode|TLS is env-only|\bTLS init\b|without relying on service tokens|replacement for the service-HMAC|RIO_JWT_SIGNING_KEY_PATH|rio\.jwt(Verify|Sign)Env|worker\.seccomp|`tls` / `metrics_addr`|\brio-worker\b|\bReadyQueue\b|\bpush_ready\b|\bqueue_priority\b|\bINTERACTIVE_BOOST\b|\bseed_ready_queue\b|\brearm_materialization_job\b|\btrim_chunk\b|\bDEFAULT_PEER_URL_TEMPLATE\b|\btick_publish_gauges\b|\bpull_attempt_seen_open\b|\bclosure_vouched\b|\bFencedWrite\b|\brollback_assignment\b|\bCOLLECT_CURSOR\b|\bCOLLECT_BACKLOG_ESTIMATE\b|rio_scheduler_workers_active|rio_scheduler_queue_depth|rio-scheduler/src/logs/|store-side 4096|[Tt]emplate brackets \\{pod\\}|Bracketed for v6-only'
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
             | grep -vE 'rio-crds/src/pool\.rs:([4-9]|1[01]):|flake\.nix:.*Before this assert|misc-checks\.nix|actor/tests/misc\.rs:.*workers_active'; then
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
        # Raw backtick alert name — must go through #(refs.alert) so
        # the gen/alerts.json membership assert fires (merged_bug_001
        # recurrence class; alternation derived from alerts.json so a
        # new alert auto-extends the lint).
        anames=$(jq -r '.names | join("|")' $alertsJson)
        if grep -rn --include='*.typ' -E "\`($anames)\`" $typSrc \
             | grep -v 'lib/refs\.typ'; then
          echo "FAIL: raw alert name — use #(refs.alert)(\"…\")" >&2
          fail=1
        fi
        # Operator runbooks must not hand-roll attempt-ledger SQL
        # (merged_bug_010: the drv_executions join came back NULL for
        # exactly the establishment rows the runbook clusters).
        # Maintained SQL functions only (the 094_establishment_clusters
        # pattern); spec chapters narrate schema freely — ops/ is the
        # enforced operator tier.
        if grep -rn -E 'FROM drv_|JOIN drv_' $typSrc/ops; then
          echo "FAIL: raw drv_* SQL in an ops runbook — wrap it in a maintained SQL function" >&2
          fail=1
        fi
        # Migration references (merged_bug_122 — the +2 renumber made
        # bare numbers silently wrong):
        #  (a) ops runbooks: no bare "migration NNN" — slug stems only;
        #  (b) everywhere: the "(0NN)" paren and "migration-0NN" hyphen
        #      shorthands are banned (the two shapes the renumber
        #      corrupted: open_attempts' (071)/(072) meant 073/074;
        #      "migration-066" meant 068);
        #  (c) every written 0NN_slug token must prefix-match a real
        #      migrations/ filename (gen/migrations.json).
        # Spec chapters keep prose "migration NNN" history (recorded
        # narrowing: ~140 correct historical citations; the corrupted
        # classes are (a)/(b)/(c)). rio-migrations/ is exempt (frozen
        # bodies; the M_NNN module IS the commentary home).
        if grep -rn -E '\bmigrations?[- ][0-9]{2,3}([^0-9_]|$)' $typSrc/ops; then
          echo "FAIL: bare migration number in an ops runbook — use the NNN_slug stem" >&2
          fail=1
        fi
        if grep -rn -E '\(0[0-9]{2}\)|\bmigrations?-0[0-9]{2}\b' $typSrc $crossSrc \
             | grep -vE '/rio-migrations/|misc-checks\.nix'; then
          echo "FAIL: (0NN)/migration-0NN shorthand — use the NNN_slug stem" >&2
          fail=1
        fi
        while IFS= read -r tok; do
          if ! jq -e --arg t "$tok" '.stems | map(startswith($t)) | any' $migrationsJson > /dev/null; then
            echo "FAIL: migration slug token '$tok' matches no migrations/ filename" >&2
            fail=1
          fi
        done < <(grep -rohE '\b0[0-9]{2}_[a-z][a-z0-9_]*' $typSrc $crossSrc \
          | sort -u)
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
  # Both implement r[dash.auth.method-gate+4]; before this check the
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

        # Both readonly HTTPRoutes: rio-scheduler-readonly (AdminService
        # + SchedulerService → rio-scheduler:9001) and
        # rio-store-logs-readonly (LogService/TailLog → rio-store:9002,
        # a separate route because a rule's backendRefs are
        # all-or-nothing and the store is a different backend in a
        # different namespace).
        helm template rio . \
          --set dashboard.enabled=true \
          --set global.image.tag=test \
          --set postgresql.enabled=false \
          | yq --no-doc 'select(.kind=="HTTPRoute" and
                       (.metadata.name=="rio-scheduler-readonly" or
                        .metadata.name=="rio-store-logs-readonly"))
                | .spec.rules[].matches[].path.value' \
          | sort > $TMPDIR/gateway-side

        sort ${nginxSide} > $TMPDIR/nginx-side

        diff $TMPDIR/nginx-side $TMPDIR/gateway-side || {
          echo "FAIL: nginx readonly allow-list (docker.nix dashboardReadonly{Admin,Scheduler,StoreLogs})" >&2
          echo "      diverged from the readonly HTTPRoutes (dashboard-gateway.yaml)." >&2
          echo "      Both implement r[dash.auth.method-gate+4] — keep them in sync." >&2
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
}
# The quint/TLC protocol-model checks and the mbt-* conformance checks
# used to be spliced in here; they are now imported directly by
# flake.nix (the `quintChecks` binding) so the CI matrix can give them
# their own `formal` kind. checks.* still contains them.
