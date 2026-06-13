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
  #
  # STRICT-DECODE TIER (merged_bug_004, R26 structural form): a value
  # query on a duplicate-tolerant parser is an INCOMPLETE VIEW of the
  # document — fragment asserts alone cannot see a duplicated mapping
  # key (yq resolves it by walk-order accident while kubectl strict /
  # ArgoCD / kubeconform reject the manifest). The driver therefore
  # wraps `helm` on PATH: every fragment's SUCCESSFUL `helm template`
  # render also passes strict_decode.py (duplicate mapping keys and
  # parse errors are structurally red regardless of which value wins).
  # Tier grammar (H7″): (1) strict pass = SafeLoader + reject-dup-keys
  # over every rendered document of every fragment render; (2) the
  # rendered key-population baseline (rendered-key-population.txt,
  # [GEN-SET]) diffs the canonical default+karpenter profile renders'
  # full key-path census so ADJACENCY drift (a key appearing in or
  # vanishing from a rendered document) is a reviewable diff;
  # regenerate via nix/tests/helm/regen-key-population.sh. Staged per
  # (vvvvv): the tool and the baseline ride the fragments fileset.
  helm-lint =
    let
      chart = pkgs.lib.cleanSource ../infra/helm/rio-build;
      fragments = pkgs.lib.fileset.toSource {
        root = ./tests/helm;
        fileset = pkgs.lib.fileset.fileFilter (
          f: f.hasExt "sh" || f.hasExt "py" || f.hasExt "txt"
        ) ./tests/helm;
      };
    in
    pkgs.runCommand "rio-helm-lint"
      {
        nativeBuildInputs = [
          pkgs.kubernetes-helm
          pkgs.yq-go
          pkgs.jq
          pkgs.gnugrep
          # strict_decode.py (the strict-decode tier + key census).
          (pkgs.python3.withPackages (ps: [ ps.pyyaml ]))
          # promtool (fragment 34): syntax-checks the rendered
          # PrometheusRule and replays the alert-contract unit tests.
          # promtool ships in the cli output, not out.
          pkgs.prometheus.cli
          # python3 (fragment 42): the reason census derives from the
          # shared rust lexer's const-array span primitive (bug_111 —
          # extraction from owned machinery, not hand regexes).
          pkgs.python3
        ];
        # merged_bug_067 (fragment 39 leg (i)): the cross-boundary
        # cluster-identity normalization fixture — ONE committed file
        # consumed by BOTH the helm leg and the Rust constructor
        # golden test, so the two languages' predicates cannot drift
        # (the nix/dashboard.nix env-input wiring precedent).
        clusterIdentityFixture = ../rio-controller/tests/golden/cluster_identity_normalization.json;
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
        # 42-reason-alert-sync.sh derives the closed reason set from
        # INTENT_DROP_REASONS — stage the const's source file so the
        # check quantifies over the real surface, never a hand copy
        # (the (vvvvv) staging discipline; the runbook precedent),
        # plus the shared lexer whose const-array span primitive does
        # the extraction (bug_111).
        cp ${../rio-controller/src/observability.rs} $TMPDIR/chart/.observability-source.rs
        cp ${../nix/rust_strip.py} $TMPDIR/chart/.rust-strip.py

        # Strict-decode tier (merged_bug_004): wrap helm so every
        # fragment's successful `helm template` output strict-decodes
        # before the fragment sees it. Non-template subcommands and
        # intentionally-failing renders pass through untouched.
        mkdir -p $TMPDIR/bin
        real_helm=$(command -v helm)
        cat > $TMPDIR/bin/helm <<SHIM
        #!${pkgs.runtimeShell}
        out=\$(mktemp)
        trap 'rm -f "\$out"' EXIT
        "$real_helm" "\$@" > "\$out"
        rc=\$?
        if [ "\$rc" -eq 0 ] && [ "\''${1:-}" = template ]; then
          python3 ${fragments}/strict_decode.py strict "\$out" >&2 || exit 1
        fi
        cat "\$out"
        exit \$rc
        SHIM
        chmod +x $TMPDIR/bin/helm
        export PATH=$TMPDIR/bin:$PATH


        # Fragment-number uniqueness gate (merged_bug_149: two wave-9
        # slots independently minted fragment 42 — parallel-slot
        # sequence-number minting had no structural collision check;
        # the NN- prefix is the fragment namespace, so a collision is
        # a process defect this driver now refuses). Quantifies over
        # the same numbered namespace the runner loop consumes.
        dupes=$(for f in ${fragments}/[0-9][0-9]-*.sh; do
          basename "$f" .sh | sed -n 's/^\([0-9][0-9]*\)-.*/\1/p'
        done | sort | uniq -d)
        if [ -n "$dupes" ]; then
          echo "FAIL: duplicate helm fragment number(s): $dupes — every NN- prefix is unique; rename to the next free number" >&2
          exit 1
        fi

        # Numbered files are fragments; unnumbered .sh (the
        # regen-key-population.sh ritual) are dev-side tools, not
        # sandbox assertions.
        for f in ${fragments}/[0-9][0-9]-*.sh; do
          echo "▸ helm-lint: $(basename "$f" .sh)" >&2
          bash -euo pipefail "$f"
        done

        # Rendered key-population baseline ([GEN-SET], merged_bug_004's
        # adjacency-drift census): the canonical default + karpenter
        # profile renders' full key-path population must match the
        # committed baseline, so a key appearing in or vanishing from a
        # rendered document is a reviewable diff, never a silent render
        # change. The renders above go through the strict shim too.
        echo "▸ helm-lint: rendered-key-population baseline" >&2
        helm template rio . --set global.image.tag=test > $TMPDIR/kp-default.yaml
        helm template rio . \
          --set karpenter.enabled=true \
          --set karpenter.clusterName=ci \
          --set karpenter.nodeRoleName=ci-role \
          --set karpenter.amiTag=test \
          --set global.image.tag=test > $TMPDIR/kp-karpenter.yaml
        {
          python3 ${fragments}/strict_decode.py keys $TMPDIR/kp-default.yaml \
            | sed 's/^/default\t/'
          python3 ${fragments}/strict_decode.py keys $TMPDIR/kp-karpenter.yaml \
            | sed 's/^/karpenter\t/'
        } > $TMPDIR/key-population.txt
        diff -u ${fragments}/rendered-key-population.txt $TMPDIR/key-population.txt || {
          echo "FAIL: rendered key population drifted from the committed baseline." >&2
          echo "If the render change is intended, regenerate the baseline:" >&2
          echo "  nix/tests/helm/regen-key-population.sh" >&2
          echo "and commit nix/tests/helm/rendered-key-population.txt with the template change." >&2
          exit 1
        }
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
          pkgs.yq-go
        ];
        dashboards = ../infra/helm/rio-build/dashboards;
        alertsJson = ../docs/gen/alerts.json;
        metricsJson = ../docs/gen/metrics.json;
        valuesYaml = ../infra/helm/rio-build/values.yaml;
      }
      ''
        fail=0
        # bug_279: dashboard exprs MUST NOT hardcode rio namespaces —
        # a values override (namespaces.store.name) silently breaks
        # every hardcoded panel. Placeholders (__RIO_NS_<KEY>__) are
        # substituted by the values-ranged replace in
        # dashboards-configmap.yaml. Natural red at introduction: 5
        # literal namespace="rio-store" matchers in store.json.
        lits=$(jq -r '.. | .expr? // empty' $dashboards/*.json \
          | grep -ohE 'namespace="rio-[a-z]*"' | sort | uniq -c || true)
        if [[ -n "$lits" ]]; then
          echo "FAIL: literal rio namespace matcher in a dashboard expr —" >&2
          echo "use namespace=\"__RIO_NS_<KEY>__\" (values-ranged substitution):" >&2
          echo "$lits" >&2
          fail=1
        fi
        # …and every placeholder must name a real namespaces key, or
        # the ranged replace never fires and Grafana queries a
        # namespace named __RIO_NS_TYPO__.
        yq e '.namespaces | to_entries | .[]
              | select(.value | type == "!!map") | .key' $valuesYaml \
          | tr '[:lower:]' '[:upper:]' | sort -u > $TMPDIR/ns-keys
        for ph in $(grep -rohE '__RIO_NS_[A-Z]+__' $dashboards/*.json \
                      | sed -E 's/__RIO_NS_([A-Z]+)__/\1/' | sort -u); do
          if ! grep -qx "$ph" $TMPDIR/ns-keys; then
            echo "FAIL: dashboard placeholder __RIO_NS_''${ph}__ has no" >&2
            echo "      .Values.namespaces key '$(tr '[:upper:]' '[:lower:]' <<<"$ph")'" >&2
            fail=1
          fi
        done
        jq -r '.. | .expr? // empty' $dashboards/*.json \
          | grep -ohE '\brio_[a-z0-9_]+' \
          | sed -E 's/_(bucket|sum|count)$//' | sort -u > $TMPDIR/dash-tokens
        jq -r '.rules[].metrics[]' $alertsJson | sort -u > $TMPDIR/rule-tokens
        jq -r '.names[]' $metricsJson | sort -u > $TMPDIR/live
        # merged_bug_235 companion: a PER-REPLICA scheduler gauge in an
        # alert expr must be aggregated across the fleet (min/max/sum/
        # avg/count), or the standby's copy pages on its own series
        # (the RioSlaHwCostStale class; contract pair in fragment 34).
        jq -r '.by_component.scheduler[]?
               | select(.aggregation? == "per-replica") | .name' \
          $metricsJson | sort -u > $TMPDIR/per-replica
        while IFS=$'\t' read -r alert expr; do
          for m in $(grep -ohE '\brio_scheduler_[a-z0-9_]+' <<<"$expr" | sort -u); do
            if grep -qx "$m" $TMPDIR/per-replica \
               && ! grep -qE "(min|max|sum|avg|count)(\s+by\s*\([^)]*\))?\s*\(\s*(rate\(|increase\()?\s*$m" <<<"$expr"; then
              echo "FAIL: alert $alert reads per-replica gauge $m unaggregated" >&2
              echo "      (every replica exports its own series; wrap it in" >&2
              echo "      min()/max()/sum() — merged_bug_235)" >&2
              fail=1
            fi
          done
        done < <(jq -r '.rules[] | [.name, .expr] | @tsv' $alertsJson)
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
    # The conf carries ENVSUBST PLACEHOLDERS for the upstream FQDNs
    # (generated from files/dashboard-upstreams.json; the image
    # entrypoint substitutes at pod start). The guard's env
    # assignments AND its envsubst var list are DERIVED from the same
    # dashboardUpstreams expression the entrypoint uses
    # (docker.nix: dashboardGuardEnv / dashboardEnvsubstVars) — the
    # substitution is structurally the entrypoint's, never a
    # hand-mirrored twin (merged_bug_160: the old literal lists made a
    # third registry record fail CI blaming the entrypoint list, the
    # one list that was derived and correct). /dev/std{err,out} →
    # TMPDIR (a remote build sandbox may not provide /dev/std*).
    mkdir -p $TMPDIR/logs

    # Self-test (no fail-open enforcement): the leftover-placeholder
    # tripwire must fire on a planted conf whose placeholder the
    # registry list does NOT cover before the real conf may pass.
    printf 'upstream bogus { server ''${RIO_BOGUS_FQDN}:1; }\n' > $TMPDIR/planted.conf
    env ${dockerImages.dashboardGuardEnv} \
      ${pkgs.gettext}/bin/envsubst '${dockerImages.dashboardEnvsubstVars}' \
      < $TMPDIR/planted.conf > $TMPDIR/planted-subst.conf
    grep -F '{RIO_' $TMPDIR/planted-subst.conf >/dev/null || {
      echo "FAIL: planted out-of-registry placeholder was NOT detected — the conf-guard tripwire is fail-open" >&2
      exit 1
    }

    env ${dockerImages.dashboardGuardEnv} \
      ${pkgs.gettext}/bin/envsubst '${dockerImages.dashboardEnvsubstVars}' \
      < ${dockerImages.dashboardNginxConf} > $TMPDIR/nginx-subst.conf
    if grep -F '{RIO_' $TMPDIR/nginx-subst.conf; then
      echo "FAIL: the conf references an upstream env var the registry does not define —" >&2
      echo "      add the record to dashboard-upstreams.json (the conf, env wiring, and" >&2
      echo "      both policy sides derive from it); hand-edited placeholders cannot ship" >&2
      exit 1
    fi
    sed -e "s#/dev/stderr#$TMPDIR/logs/error.log#" \
        -e "s#/dev/stdout#$TMPDIR/logs/access.log#" \
      $TMPDIR/nginx-subst.conf > $TMPDIR/nginx.conf
    ${dockerImages.dashboardNginx}/bin/nginx -t -p $TMPDIR -c $TMPDIR/nginx.conf

    # ns-parity arm (merged_bug_160 second half): docker.nix's
    # dashboardNsDefaults are a CHECKED mirror of values.yaml
    # namespaces.{system,store}.name — nix cannot parse YAML, so the
    # literals stay, and THIS comparator turns silent drift into a red.
    # Self-test first: a perturbed copy must fail the comparator.
    ns_json='${builtins.toJSON dockerImages.dashboardNsDefaults}'
    v_sys=$(${pkgs.yq-go}/bin/yq '.namespaces.system.name' ${../infra/helm/rio-build/values.yaml})
    v_sto=$(${pkgs.yq-go}/bin/yq '.namespaces.store.name' ${../infra/helm/rio-build/values.yaml})
    ns_check() {
      j_sys=$(echo "$1" | ${pkgs.jq}/bin/jq -r .system)
      j_sto=$(echo "$1" | ${pkgs.jq}/bin/jq -r .store)
      [ "$j_sys" = "$v_sys" ] && [ "$j_sto" = "$v_sto" ]
    }
    if ns_check "$(echo "$ns_json" | ${pkgs.jq}/bin/jq '.system="rio-wrong"')"; then
      echo "FAIL: ns-parity comparator accepted a perturbed copy — fail-open" >&2
      exit 1
    fi
    ns_check "$ns_json" || {
      echo "FAIL: docker.nix dashboardNsDefaults drifted from values.yaml" >&2
      echo "      namespaces.{system,store}.name ($ns_json vs $v_sys/$v_sto) —" >&2
      echo "      update docker.nix's checked mirror to match values.yaml" >&2
      exit 1
    }
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
            # merged_bug_173 (ban v2): doc prose lives in rio-migrations
            # too (schema.rs narrated the raw resolution as live).
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-migrations/src)
            # merged_bug_293: the offline query cache is a source tree
            # too — an orphaned .sqlx entry caching the banned resolver
            # is a ready-made revival kit (two such orphans shipped:
            # the retired kind-blind resolver + the assignments-join
            # lookup). Scan it with the same pattern.
            ../.sqlx
          ];
        };
        nativeBuildInputs = [ pkgs.ripgrep ];
      }
      ''
        set +o pipefail
        hits=$(rg -n -i 'FROM drv_executions[^;]*ORDER BY exec_id DESC' --multiline           $src/rio-store/src $src/rio-scheduler/src $src/rio-gateway/src $src/rio-cli/src $src/.sqlx           || true)
        if [[ -n "$hits" ]]; then
          echo "FAIL: raw latest-exec resolution over drv_executions —" >&2
          echo "read the kind-filtered latest_build_exec view instead (M_089):" >&2
          echo "$hits" >&2
          exit 1
        fi
        # v2 (merged_bug_173): prose evades the FROM-window — a doc
        # comment citing the raw resolution shapes reader behavior as
        # much as code does (executions.rs and schema.rs both narrated
        # it as live). Bare-phrase scan with a content-keyed allowlist:
        # a line may mention the raw form ONLY while citing the
        # replacement view, the ban, or the M_089 record on that line.
        hits2=$(rg -n -i 'ORDER BY exec_id DESC' \
          $src/rio-store/src $src/rio-scheduler/src $src/rio-gateway/src \
          $src/rio-cli/src $src/rio-migrations/src $src/.sqlx \
          | rg -v 'latest_build_exec|log-no-raw-latest-exec|M_089|089_log_authority' || true)
        if [[ -n "$hits2" ]]; then
          echo "FAIL: bare 'ORDER BY exec_id DESC' without a same-line citation" >&2
          echo "of latest_build_exec / the ban / M_089 — reword to the view truth:" >&2
          echo "$hits2" >&2
          exit 1
        fi
        touch $out
      '';

  # bughunt2 slot 4 (merged_bug_108): the consumer registry
  # (rio-store/src/authz.rs METHOD_CONSUMERS) declares every production
  # surface of the tenant-authenticated store methods with a grep
  # anchor. This check pins the anchors against the real files — a
  # renamed or deleted consumer breaks here instead of silently
  # rotting the registry. Pairing is positional (anchor_file then
  # anchor_symbol per struct literal, enforced by a count equality).
  # bughunt2 slot 6 (bug_068 + merged_bug_164): the log plane's status
  # and loss-counter chokepoints. Three producer-set pins, each grep
  # planted-red-verified at introduction:
  #  1. `resource_exhausted` in rio-store/src/logs/ only in gate.rs
  #     (replica_capacity_status) — a per-execution cap hand-rolled as
  #     RESOURCE_EXHAUSTED re-creates the builder's 1 Hz re-dial storm.
  #  2. x-rio-log-reject metadata inserts only in gate.rs
  #     (cap_rejection) — one constructor stamps the class alphabet.
  #  3. the loss-counter name only in logs/loss.rs (note_hole, the
  #     dedup) and lib.rs (describe/alphabet/seed) — any other file
  #     mentioning it is a new increment path dodging hole-identity
  #     dedup (or a comment that should say "the loss counter").
  log-cap-status-chokepoint =
    pkgs.runCommand "rio-log-cap-status-chokepoint"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = ../rio-store/src;
        };
        nativeBuildInputs = [ pkgs.ripgrep ];
      }
      ''
        fail=0
        cd $src/rio-store/src
        bad=$(rg -l 'resource_exhausted' logs/ | rg -v '^(\./)?logs/gate\.rs$' || true)
        if [[ -n "$bad" ]]; then
          echo "FAIL: resource_exhausted outside gate.rs in the log plane:" >&2
          echo "$bad" >&2
          echo "  (per-replica capacity goes through gate::replica_capacity_status;" >&2
          echo "   per-execution caps through gate::cap_rejection — store.log.cap-reject-class)" >&2
          fail=1
        fi
        bad=$(rg -lU 'metadata_mut\(\)\s*\.insert\(\s*rio_proto::LOG_REJECT_METADATA_KEY' . | rg -v '^(\./)?logs/gate\.rs$' || true)
        if [[ -n "$bad" ]]; then
          echo "FAIL: x-rio-log-reject inserted outside gate.rs:" >&2
          echo "$bad" >&2
          fail=1
        fi
        bad=$(rg -lU 'metrics::counter!\(\s*"rio_store_log_read_data_loss_total"' . | rg -v '^(\./)?logs/loss\.rs$' || true)
        if [[ -n "$bad" ]]; then
          echo "FAIL: a loss-counter increment site outside loss.rs:" >&2
          echo "$bad" >&2
          echo "  (increments go through loss::note_hole — store.log.loss-event-identity)" >&2
          fail=1
        fi
        [[ $fail -eq 0 ]]
        touch $out
      '';

  consumer-registry-anchors =
    pkgs.runCommand "rio-consumer-registry-anchors"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            ../rio-store/src/authz.rs
            ../rio-gateway/src/handler/log_tail.rs
            ../rio-gateway/src/quota.rs
            ../rio-cli/src/logs.rs
            ../rio-dashboard/src/lib/logStream.svelte.ts
          ];
        };
        nativeBuildInputs = [ pkgs.ripgrep ];
      }
      ''
        set +o pipefail
        reg=$src/rio-store/src/authz.rs
        mapfile -t files < <(rg -o 'anchor_file: "([^"]+)"' -r '$1' "$reg")
        mapfile -t symbols < <(rg -o 'anchor_symbol: "([^"]+)"' -r '$1' "$reg")
        if [[ ''${#files[@]} -lt 4 || ''${#files[@]} -ne ''${#symbols[@]} ]]; then
          echo "FAIL: registry parse drift (''${#files[@]} files vs ''${#symbols[@]} symbols)" >&2
          exit 1
        fi
        fail=0
        for i in "''${!files[@]}"; do
          f=''${files[$i]}; sym=''${symbols[$i]}
          if [[ ! -f "$src/$f" ]]; then
            echo "FAIL: METHOD_CONSUMERS anchor_file $f does not exist" >&2
            fail=1
          elif ! rg -q -F "$sym" "$src/$f"; then
            echo "FAIL: anchor symbol $sym not found in $f (consumer moved/renamed?)" >&2
            fail=1
          fi
        done
        [[ $fail -eq 0 ]]
        touch $out
      '';

  # bughunt2 slot 4 (merged_bug_064): derivations.tenant_id was never
  # production-written (migration 095 census, M_095) and is DROPPED;
  # ownership is build-membership over builds.tenant_id
  # (store.log.tail-ownership). The vacuity failure mode was test
  # fixtures stamping the dead column so ownership suites proved a
  # truth production never exercised. This lint makes the dead fixture
  # shape unwritable workspace-wide (allowlist: rio-migrations/, which
  # legitimately mentions the column in frozen history). Its born-red
  # baseline IS the census: at the pre-rewrite tree (372c7719e) it
  # fires on the four legacy logs/service.rs fixture writes.
  authz-fixture-policy =
    pkgs.runCommand "rio-authz-fixture-policy"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-store)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-scheduler)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-gateway)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-cli)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-builder)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-controller)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-common)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-test-support)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../xtask)
          ];
        };
        nativeBuildInputs = [ pkgs.ripgrep ];
      }
      ''
        set +o pipefail
        fail=0
        # INSERT column lists naming the dropped column (multiline:
        # SQL string literals wrap; the column list ends at ')').
        hits=$(rg -nU 'INSERT INTO derivations\s*\([^)]*tenant_id' $src || true)
        if [[ -n "$hits" ]]; then
          echo "FAIL: fixture INSERTs derivations.tenant_id — the column was never" >&2
          echo "production-written and is dropped (migration 095, M_095). Ownership" >&2
          echo "fixtures use the production shape: seed_production_ownership" >&2
          echo "(builds.tenant_id + build_derivations), per store.log.tail-ownership:" >&2
          echo "$hits" >&2
          fail=1
        fi
        # UPDATE ... SET targeting the dropped column.
        hits=$(rg -nU 'UPDATE derivations\s+SET[^"]{0,300}tenant_id' $src || true)
        if [[ -n "$hits" ]]; then
          echo "FAIL: fixture UPDATEs derivations.tenant_id (dropped by migration 095" >&2
          echo "— see M_095); use seed_production_ownership instead:" >&2
          echo "$hits" >&2
          fail=1
        fi
        [[ $fail -eq 0 ]]
        touch $out
      '';

  # bughunt2 slot 4 riders (merged_bug_168 + bug_362): the log-gate
  # authority model is CLAIMED-EXEC (check 3 at logs/gate.rs) — the
  # pre-wave "latest assignment" / "live assignment" rule is retired,
  # and the "chunks before the lifecycle INSERT" ordering claim is
  # false (append admission requires the drv_executions row to exist,
  # so chunks can never precede it). Narration restating either
  # retired concept regressed twice via comment-copy; this lint makes
  # the phrases unwritable. The ordering law is single-homed at
  # logs/gate.rs check 3 — cite it instead of restating.
  # Born RED on the pre-rider tree (7 sites); the rewordings in the
  # same commit are the green.
  retired-phrase-lint =
    pkgs.runCommand "rio-retired-phrase-lint"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-store/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-scheduler/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-gateway/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-cli/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-builder/src)
          ];
        };
        nativeBuildInputs = [ pkgs.ripgrep ];
      }
      ''
        set +o pipefail
        fail=0
        # The retired assignment-currency vocabulary, scoped to the log
        # subsystem (the scheduler legitimately reasons about latest
        # assignments — ITS tables own that question). [^a-zA-Z]{0,3}
        # catches markup-interposed forms (*latest* assignment).
        hits=$(rg -n -i '(latest|live)[^a-zA-Z]{0,3}assignment' $src/rio-store/src/logs || true)
        if [[ -n "$hits" ]]; then
          echo "FAIL: retired log-authority vocabulary ('latest/live assignment') in" >&2
          echo "rio-store/src/logs — authority is keyed on the CLAIMED exec; cite" >&2
          echo "logs/gate.rs check 3 instead of restating the retired rule:" >&2
          echo "$hits" >&2
          fail=1
        fi
        # The false ordering claim, workspace-wide.
        hits=$(rg -nU -i "chunks before the[^a-zA-Z]+(scheduler.s[^a-zA-Z]+)?lifecycle INSERT" $src || true)
        if [[ -n "$hits" ]]; then
          echo "FAIL: 'chunks before the lifecycle INSERT' is a false ordering claim" >&2
          echo "(append admission requires the drv_executions row; see logs/gate.rs" >&2
          echo "check 3, the single home of the ordering law):" >&2
          echo "$hits" >&2
          fail=1
        fi
        [[ $fail -eq 0 ]]
        touch $out
      '';

  amendment-status-coherence =
    pkgs.runCommand "rio-amendment-status-coherence"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            ../docs/spec
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-store/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-scheduler/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-evidence-kernel/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-gateway/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-builder/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-cli/src)
          ];
        };
        nativeBuildInputs = [ pkgs.ripgrep ];
      }
      ''
        set +o pipefail
        # bug_109: amendment status lives at exactly ONE anchor (the
        # counter-signature record at scheduler.typ's pull-contract
        # amendment anchor); every other site POINTS there instead of
        # restating the state. Both halves run against planted red and
        # green fixtures below before the real tree is scanned.
        scan() {
          local dir=$1 fail=0 hits pending signed both
          # (1) Zero stale status restatements. MULTILINE-tolerant
          # (rg -U): the wave sweep found a live site whose phrase
          # wrapped across a comment-line boundary and was invisible
          # to single-line grep. The interposed class tolerates
          # comment leaders between the words.
          hits=$(rg -nU -i 'pending[[:space:]/*#-]+owner[[:space:]/*#-]+counter-signature' "$dir" || true)
          if [[ -n "$hits" ]]; then
            echo "FAIL: stale 'PENDING owner counter-signature' restatement —" >&2
            echo "amendment status lives only at scheduler.typ's pull-contract" >&2
            echo "amendment anchor; point there instead of restating the state:" >&2
            echo "$hits" >&2
            fail=1
          fi
          # (2) Coherence: one amendment id carrying BOTH a PENDING
          # marker and a SIGNED counter-signature record is
          # self-contradictory — one of the two is stale.
          pending=$(rg -oNU -i '(rule-[a-z0-9]+)[[:space:]]+amendment[[:space:]]*\((PENDING|AWAITING)' "$dir" -r '$1' | tr '[:upper:]' '[:lower:]' | sort -u || true)
          signed=$(rg -oNU -i 'counter-signature[[:space:]]+for[[:space:]]+the[[:space:]]+(rule-[a-z0-9]+)[[:space:]]+amendment:[[:space:]]+SIGNED' "$dir" -r '$1' | tr '[:upper:]' '[:lower:]' | sort -u || true)
          both=$(comm -12 <(echo "$pending") <(echo "$signed") | sed '/^$/d')
          if [[ -n "$both" ]]; then
            echo "FAIL: amendment id(s) recorded BOTH pending and signed:" >&2
            echo "$both" >&2
            echo "(sweep the stale marker; the signature block is the record)" >&2
            fail=1
          fi
          [[ $fail -eq 0 ]]
        }
        # Self-test: the planted red must FAIL (both halves trip), the
        # clean fixture must PASS — only then is the scanner trusted.
        mkdir -p "$TMPDIR/red" "$TMPDIR/green"
        {
          echo 'The Rule-9 amendment (PENDING'
          echo 'owner counter-signature) is recorded here.'
          echo 'Owner counter-signature for the rule-9 amendment: SIGNED 2026-01-01.'
        } > "$TMPDIR/red/doc.md"
        {
          echo 'The rule-9 amendment — status at the single anchor.'
          echo 'Owner counter-signature for the rule-9 amendment: SIGNED 2026-01-01.'
        } > "$TMPDIR/green/doc.md"
        if scan "$TMPDIR/red" 2>/dev/null; then
          echo "SELF-TEST FAIL: the planted-red fixture passed" >&2
          exit 1
        fi
        scan "$TMPDIR/green" || { echo "SELF-TEST FAIL: the clean fixture failed" >&2; exit 1; }
        scan "$src"
        touch $out
      '';

  # merged_bug_075: cross-task leadership signals in rio-lease carry the
  # tenure that issued them (epoch-stamped AtomicU64 — the
  # recovery_completed_for / step_down_for pattern). A bare AtomicBool
  # across an acquire/rebound edge is the banned shape: it cannot tell a
  # request from tenure N apart from one for tenure N+1, which is how a
  # stale step-down demoted a healthy rebounded successor. The ban is a
  # MECHANISM, not a comment: every AtomicBool token in the two lease
  # source files must match the explicit allowlist (the import line, the
  # is_leader belief flag consumed through the generation protocol, the
  # marks single-flight in_flight slot — a task mutex, not a leadership
  # signal — and comment prose). A new bare flag fails this check; a new
  # legitimate use extends the allowlist in a visible diff.
  lease-signal-tenure-stamped =
    pkgs.runCommand "rio-lease-signal-tenure-stamped"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            ../rio-lease/src/lib.rs
            ../rio-lease/src/election.rs
          ];
        };
        nativeBuildInputs = [ pkgs.ripgrep ];
      }
      ''
        set +o pipefail
        scan() {
          local dir=$1
          local hits
          # Allowlist: the import line; comment prose; is_leader (the
          # belief flag consumed through the generation protocol);
          # in_flight + its SlotRelease drop guard (the marks
          # single-flight slot — a task mutex, not a leadership signal).
          hits=$(rg -n 'AtomicBool' "$dir" \
            | rg -v '^[^:]+:[0-9]+:\s*//' \
            | rg -v '^[^:]+:[0-9]+:use ' \
            | rg -v 'is_leader' \
            | rg -v 'in_flight' \
            | rg -v 'SlotRelease' \
            || true)
          if [[ -n "$hits" ]]; then
            echo "FAIL: bare AtomicBool in rio-lease outside the allowlist —" >&2
            echo "cross-task leadership signals must be tenure-stamped AtomicU64" >&2
            echo "(the recovery_completed_for / step_down_for pattern); a task-local" >&2
            echo "flag needs an allowlisted identifier or an allowlist extension:" >&2
            echo "$hits" >&2
            return 1
          fi
        }
        # Self-test: the planted red (a bare leadership flag) must FAIL;
        # the green fixture (every allowlisted shape) must PASS — only
        # then is the scanner trusted on the real tree.
        mkdir -p "$TMPDIR/red" "$TMPDIR/green"
        printf '    step_down: Arc<AtomicBool>,\n' > "$TMPDIR/red/lib.rs"
        {
          printf 'use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};\n'
          printf '    is_leader: Arc<AtomicBool>,\n'
          printf '    let marks_patch_in_flight = Arc::new(AtomicBool::new(false));\n'
          printf '    // prose mention of AtomicBool in a comment\n'
        } > "$TMPDIR/green/lib.rs"
        if scan "$TMPDIR/red" 2>/dev/null; then
          echo "SELF-TEST FAIL: the planted bare-flag fixture passed" >&2
          exit 1
        fi
        scan "$TMPDIR/green" || { echo "SELF-TEST FAIL: the clean fixture failed" >&2; exit 1; }
        scan "$src/rio-lease/src"
        touch $out
      '';

  # merged_bug_016: a backslash-continued string literal re-joined onto
  # one line keeps the continuation's alignment spaces INSIDE the
  # literal — error messages, log lines, SQL, and metric HELP then carry
  # 10-30 garbage spaces verbatim (18 live sites repaired at
  # introduction; natural red recorded in the commit body). The
  # describe-HELP scraper additionally hard-errors at 2+ interior spaces
  # (xtask docs_data garbled_interior_run) since HELP flows to docs/gen
  # and the rendered chart; this generic check holds the ≥8 line at
  # every other literal.
  string-interior-spaces =
    pkgs.runCommand "rio-string-interior-spaces"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-auth/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-authz-kernel/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-builder/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-cli/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-common/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-controller/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-crds/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-evidence-kernel/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-gateway/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-lease/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-log-kernel/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-migrations/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-nix/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-proto/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-retry-kernel/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-scheduler/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-store/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-test-support/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../xtask/src)
          ];
        };
        nativeBuildInputs = [ pkgs.python3 ];
        scanScript = ../nix/string_interior_spaces.py;
        sharedLexer = ../nix/rust_strip.py;
      }
      ''
        # bughunt-3 S1 (merged_bug_193): lexer-exact string spans —
        # the scanner blanks comments and tracks raw/non-raw literal
        # boundaries, so quote parity is structural. Arm A: single-line
        # 8+ interior runs (minus \n-template indents). Arm B:
        # mid-string `\<LF>` continuation mixed with a BARE newline
        # (a prose join that lost a backslash); SQL bare-newline style
        # and the leading-`"\` fixture idiom stay exempt.
        # merged_bug_049: the token grammar lives in the SHARED exact
        # lexer (rust_strip.py), staged beside the scanner so `import
        # rust_strip` resolves; its span/blank selftest plus the
        # per-arm planted red+green self-tests run in the script
        # before the real scan may gate (banner (b)).
        cp "$sharedLexer" rust_strip.py
        cp "$scanScript" string_interior_spaces.py
        python3 string_interior_spaces.py "$src" \
          rio-auth/src rio-authz-kernel/src rio-builder/src rio-cli/src \
          rio-common/src rio-controller/src rio-crds/src \
          rio-evidence-kernel/src rio-gateway/src rio-lease/src \
          rio-log-kernel/src rio-migrations/src rio-nix/src rio-proto/src \
          rio-retry-kernel/src rio-scheduler/src rio-store/src \
          rio-test-support/src xtask/src
        touch $out
      '';

  # R13 fixture-provenance lint (bughunt-5 WO-S8-11): machine witnesses
  # are minted through production constructors — test fixtures may NOT
  # hand-roll wire/identity shapes the producing crate cannot emit.
  # Four NARROW arms from the round-5 corpus (executor_id overrides,
  # exposure-uid inputs, literal ObjectMeta uids, direct
  # handle_completion calls); the closed `r13-allow(<lane>)` sanction
  # grammar (refusal-probe | frozen-legacy | opaque-consumer) is the
  # ONLY pressure valve — arms never weaken. Selftests (one planted
  # red + green per arm, plus the UNSANCTIONED-lane red) run before
  # the real scan may gate; the shared exact lexer's selftest gates
  # first (token-accurate matching: comments cannot fire arms).
  fixture-provenance =
    pkgs.runCommand "rio-fixture-provenance"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.unions [
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-controller/src)
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs") ../rio-scheduler/src)
          ];
        };
        nativeBuildInputs = [ pkgs.python3 ];
        scanScript = ../nix/fixture_provenance.py;
        sharedLexer = ../nix/rust_strip.py;
      }
      ''
        cp "$sharedLexer" rust_strip.py
        cp "$scanScript" fixture_provenance.py
        python3 fixture_provenance.py "$src"
        touch $out
      '';

  # Census-enrollment lint (round-8 WO-S6-5 — the author-census kill):
  # load-bearing prose MEMBERSHIP claims in rust comments (the
  # call-graph family: "both call sites", "the only caller(s)",
  # "all callers", "no other call sites", "exactly N callers",
  # "never sends", ...) are UNSHIPPABLE unless the same line carries
  # census[test: <fn>] / census[gen: <path>] binding the claim to an
  # artifact the membership change breaks — or the line is
  # grandfathered in nix/census-grandfather.txt (FROZEN, burn-down:
  # machine-minted by the scanner's own --mint-grandfather mode at
  # the round-8 final slot tree; content-keyed, so editing a
  # grandfathered census line evicts it and the file only ever
  # shrinks; this lint never suggests grandfathering). Every
  # census[...] tag anywhere must RESOLVE (a test fn in the scanned
  # trees / a committed file) — enrollment binds, never decorates.
  # Four planted self-test arms (unenrolled-red, enrolled-green,
  # dangling-red, stale-grandfather-red) run before the real scan may
  # gate; the shared exact lexer's selftest gates first (comment-lane
  # matching: string literals can never fire the grammar).
  rule-citation-versions =
    pkgs.runCommand "rio-rule-citation-versions"
      {
        # Full-tree staging ((vvvvv)): the lint quantifies over EVERY
        # blind tier (helm yaml comments, scenario files, sh
        # fragments, migration rationale) against the rule mints in
        # docs/spec — a narrower fileset would make exactly the blind
        # tiers it exists for premise-unreachable in the sandbox.
        src = pkgs.lib.cleanSource ../.;
        nativeBuildInputs = [ pkgs.python3 ];
        scanScript = ../nix/rule_citation_versions.py;
      }
      ''
        cp "$scanScript" rule_citation_versions.py
        python3 rule_citation_versions.py "$src"
        touch $out
      '';

  # R22 tier-2 (the round-9 banner instance): the census GENERATORS
  # are themselves censused. The registry in the script is the closed
  # enrollment set (per generator: anchor, embedded-plant pattern,
  # axes covered, NAMED axis gaps under burn-down semantics); a
  # generator-shaped scanner outside the registry fails (the reverse
  # direction over the enrollment set itself). Rides two enforcement
  # arms with embedded plants: the MODEL-DIVERGENCE header grammar
  # (the model-tier drift grep) and the negative refusal census
  # (merged_bug_059 — open-coded matches! folds over tonic codes
  # banned outside the adjudication authority; founding plant = the
  # pre-fix builder fold, quoted verbatim in the script).
  census-corpora =
    pkgs.runCommand "rio-census-corpora"
      {
        # Full-tree staging ((vvvvv)): the registry quantifies over
        # nix scripts, helm fragments, in-crate sources, and
        # docs/spec/models.
        src = pkgs.lib.cleanSource ../.;
        nativeBuildInputs = [ pkgs.python3 ];
        scanScript = ../nix/census_corpora.py;
        # The refusal census routes through the shared exact lexer
        # (merged_bug_009), staged beside the scanner so `import
        # rust_strip` resolves; its selftest gates first.
        sharedLexer = ../nix/rust_strip.py;
      }
      ''
        cp "$sharedLexer" rust_strip.py
        cp "$scanScript" census_corpora.py
        python3 census_corpora.py "$src"
        touch $out
      '';

  # WO-S8-2 (merged_bug_088, R22″): the cfg-pruner PARITY gate. The
  # nix pruners' production-population predicate is differentially
  # pinned against the canonical xtask semantics (cfg_pred_gates_test,
  # xtask/src/lint.rs): (1) the canonical's SOURCE is pinned at
  # nix/cfg-pruner-canonical.pin ([GEN-SET]: `rust_strip.py
  # --extract-canonical xtask/src/lint.rs`) — canonical drift reds
  # this check until the python port is re-derived; (2) over EVERY
  # cfg attribute in rio-*/src + xtask/src, the flat spelling table
  # (enumeration axis) and the ported canonical predicate (recursion
  # axis) must agree, and a spelling outside the table — the fourth
  # spelling — is a named red, never a silent one-sided
  # classification (the merged_bug_088 class: two "canonical" pruners
  # diverging one spelling at a time).
  cfg-pruner-parity =
    pkgs.runCommand "rio-cfg-pruner-parity"
      {
        # Full-tree staging ((vvvvv)): the gate quantifies over every
        # crate's cfg attributes AND the canonical's own source.
        src = pkgs.lib.cleanSource ../.;
        nativeBuildInputs = [ pkgs.python3 ];
        sharedLexer = ../nix/rust_strip.py;
      }
      ''
        cp "$sharedLexer" rust_strip.py
        python3 rust_strip.py --parity "$src"
        touch $out
      '';

  # merged_bug_076: the node_informer drop classifier's producer-cite
  # rationale, machine-diffed against the Status constructors the
  # AdminService module actually mints. A `producer-census: <code> =
  # emitted` row with zero matching constructors is the FALSE-CITE
  # defect this rationale shipped with; a `never-emitted` row with a
  # live constructor is emitter-set drift (the classifier arm needs
  # re-derivation). Planted self-test arms (incl. the shipped defect
  # verbatim) run before the real scan. Cross-crate by nature
  # (controller classifier vs scheduler server module) — lives HERE,
  # not in-crate, because a unit test cannot read the other crate's
  # source under the per-member fileset (the embed+pin face's
  # cross-crate sibling).
  exposure-producer-census =
    pkgs.runCommand "rio-exposure-producer-census"
      {
        # Full-tree staging ((vvvvv)): the scan quantifies over two
        # crates' sources.
        src = pkgs.lib.cleanSource ../.;
        nativeBuildInputs = [ pkgs.python3 ];
        scanScript = ../nix/exposure_producer_census.py;
        # Constructor scan routes through the shared exact lexer
        # (merged_bug_009) — staged beside the scanner.
        sharedLexer = ../nix/rust_strip.py;
      }
      ''
        cp "$sharedLexer" rust_strip.py
        cp "$scanScript" exposure_producer_census.py
        python3 exposure_producer_census.py "$src"
        touch $out
      '';

  # WO-S8-14 (the round-12 banner enforcement bodies, R31/R32/R33/
  # R29'): the reader-census registry (union rows + enrollment
  # totality), the obligation+clock census (pending rows flip at the
  # wave-close --verify-landed), and the duplicate-derivation lint +
  # rationale-rot sweep. Full-tree staging ((vvvvv)): every body
  # quantifies over workspace sources and committed [GEN-SET]
  # expectation files (nix/census/*.union, the grandfathers).
  reader-census-registry =
    pkgs.runCommand "rio-reader-census-registry"
      {
        src = pkgs.lib.cleanSource ../.;
        nativeBuildInputs = [ pkgs.python3 ];
        scanScript = ../nix/reader_census_registry.py;
        sharedLexer = ../nix/rust_strip.py;
        censusLib = ../nix/census_corpora.py;
      }
      ''
        cp "$sharedLexer" rust_strip.py
        cp "$censusLib" census_corpora.py
        cp "$scanScript" reader_census_registry.py
        python3 reader_census_registry.py "$src"
        touch $out
      '';

  obligation-clock-census =
    pkgs.runCommand "rio-obligation-clock-census"
      {
        src = pkgs.lib.cleanSource ../.;
        nativeBuildInputs = [ pkgs.python3 ];
        scanScript = ../nix/obligation_clock_census.py;
        sharedLexer = ../nix/rust_strip.py;
        censusLib = ../nix/census_corpora.py;
      }
      ''
        cp "$sharedLexer" rust_strip.py
        cp "$censusLib" census_corpora.py
        cp "$scanScript" obligation_clock_census.py
        python3 obligation_clock_census.py "$src" --verify-landed
        touch $out
      '';

  # The R31' predicate-derivation registry + the K-mutation standing
  # check (round-13 WO-S9-8(i), the bug_047 born-broken lesson made
  # standing): every enrolled census generator declares its predicate
  # provenance — derived(anchor) / planted(battery, anchor-verified) /
  # dated debt (shrink-only retrofit queue) — and the registry's own
  # K-mutation battery runs through the shared harness on every
  # invocation. Full-tree staging ((vvvvv)): the anchors quantify over
  # nix/ artifact sources.
  predicate-derivation-registry =
    pkgs.runCommand "rio-predicate-derivation-registry"
      {
        src = pkgs.lib.cleanSource ../.;
        nativeBuildInputs = [ pkgs.python3 ];
        scanScript = ../nix/predicate_derivation_registry.py;
        sharedLexer = ../nix/rust_strip.py;
        censusLib = ../nix/census_corpora.py;
      }
      ''
        cp "$sharedLexer" rust_strip.py
        cp "$censusLib" census_corpora.py
        cp "$scanScript" predicate_derivation_registry.py
        python3 predicate_derivation_registry.py "$src"
        touch $out
      '';

  # The R34 (periodic-event, bound) census + the R33' polarity/units
  # rider registry (round-13 WO-S9-8(ii)): enrolled gate-clock pairs
  # and producer-quantity riders with the pending/anchored lifecycle
  # (rows flip at the wave-close --verify-landed), plus the R34(ii)
  # no-op stamp grammar enforced from birth. Full-tree staging
  # ((vvvvv)).
  cadence-polarity-registries =
    pkgs.runCommand "rio-cadence-polarity-registries"
      {
        src = pkgs.lib.cleanSource ../.;
        nativeBuildInputs = [ pkgs.python3 ];
        scanScript = ../nix/cadence_polarity_registries.py;
        sharedLexer = ../nix/rust_strip.py;
        censusLib = ../nix/census_corpora.py;
      }
      ''
        cp "$sharedLexer" rust_strip.py
        cp "$censusLib" census_corpora.py
        cp "$scanScript" cadence_polarity_registries.py
        # --verify-landed from the round-13 close onward: every row is
        # anchored at this tree; a future pending row must flip at its
        # own wave's close or red here, never linger silently.
        python3 cadence_polarity_registries.py "$src" --verify-landed
        touch $out
      '';

  # P9 — the model-letter reachability lint (round-13 WO-S9-8(iii);
  # the F2 founding class): variant letters consumed by the invariant
  # tier need an action/run constructor occurrence, a p9-vacuity
  # exemption, or the shrink-only content-keyed grandfather.
  model-letter-reachability =
    pkgs.runCommand "rio-model-letter-reachability"
      {
        src = pkgs.lib.cleanSource ../.;
        nativeBuildInputs = [ pkgs.python3 ];
        scanScript = ../nix/model_letter_reachability.py;
        sharedLexer = ../nix/rust_strip.py;
        censusLib = ../nix/census_corpora.py;
      }
      ''
        cp "$sharedLexer" rust_strip.py
        cp "$censusLib" census_corpora.py
        cp "$scanScript" model_letter_reachability.py
        python3 model_letter_reachability.py "$src"
        touch $out
      '';

  duplicate-derivation-lint =
    pkgs.runCommand "rio-duplicate-derivation-lint"
      {
        src = pkgs.lib.cleanSource ../.;
        nativeBuildInputs = [ pkgs.python3 ];
        scanScript = ../nix/duplicate_derivation_lint.py;
        sharedLexer = ../nix/rust_strip.py;
        censusLib = ../nix/census_corpora.py;
      }
      ''
        cp "$sharedLexer" rust_strip.py
        cp "$censusLib" census_corpora.py
        cp "$scanScript" duplicate_derivation_lint.py
        python3 duplicate_derivation_lint.py "$src" --verify-landed
        touch $out
      '';

  census-enrollment =
    pkgs.runCommand "rio-census-enrollment"
      {
        # The WHOLE flake source, not a rust-only fileset: the scan
        # walks rio-*/src *.rs, but `census[gen: <path>]` enrollment
        # binds to ARBITRARY repo-relative committed files (the first
        # in-tree adopter points at a helm test fragment), so the
        # check's verdict genuinely depends on those files existing —
        # a narrower staging made gen tags premise-unreachable in the
        # sandbox while resolving locally (caught live at the round-8
        # wave close: the gate refused a tag the working tree
        # satisfied). The lint is a sub-second lexer pass; rebuilding
        # it on any repo change is the correct dependency semantics,
        # not waste.
        src = pkgs.lib.cleanSource ../.;
        nativeBuildInputs = [ pkgs.python3 ];
        scanScript = ../nix/census_enrollment.py;
        sharedLexer = ../nix/rust_strip.py;
      }
      ''
        cp "$sharedLexer" rust_strip.py
        cp "$scanScript" census_enrollment.py
        python3 census_enrollment.py \
          --grandfather "$src/nix/census-grandfather.txt" "$src"
        touch $out
      '';

  # R23' (the round-10 banner; WO-S8-7): emphatic-uppercase quantifier
  # claims (the lexicon words) in rust comments, helm narration, spec
  # prose, and script comments bind to machinery
  # (`quantifier: census(...)`), demote (lowercase / non-normative
  # tag), or ride the frozen burn-down grandfather. Self-test plants
  # derive from the LEXICON x TIERS product (R22').
  quantifier-lexicon =
    pkgs.runCommand "rio-quantifier-lexicon"
      {
        # Full-tree staging ((vvvvv)): the lint quantifies over four
        # tiers spanning rust sources, helm charts, docs/spec, and
        # nix scripts — plus the grandfather ledger itself.
        src = pkgs.lib.cleanSource ../.;
        nativeBuildInputs = [ pkgs.python3 ];
        scanScript = ../nix/quantifier_lexicon.py;
        sharedLexer = ../nix/rust_strip.py;
        censusLib = ../nix/census_corpora.py;
      }
      ''
        cp "$sharedLexer" rust_strip.py
        cp "$censusLib" census_corpora.py
        cp "$scanScript" quantifier_lexicon.py
        python3 quantifier_lexicon.py "$src"
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
        scanScript = ../nix/metric_reason_help_sync.py;
        # Comment stripping routes through the shared exact lexer
        # (merged_bug_009), staged beside the scanner so `import
        # rust_strip` resolves; its selftest gates first.
        sharedLexer = ../nix/rust_strip.py;
      }
      ''
        cp "$sharedLexer" rust_strip.py
        cp "$scanScript" metric_reason_help_sync.py
        python3 metric_reason_help_sync.py $src
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

        # 2. Decode + scan + negative self-test. merged_bug_049: the
        # token grammar lives in the SHARED exact lexer
        # (rust_strip.py), staged beside the scanner so `import
        # rust_strip` resolves; its selftest gates first.
        cp ${../nix/rust_strip.py} rust_strip.py
        cp ${../nix/streaming_open_ban.py} streaming_open_ban.py
        python3 streaming_open_ban.py fds.pb $src
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
  # design). Directive-2 record (bug_269, relocated from the retired
  # fence map): no model — the property is TEXTUAL (the literal
  # exists in exactly two files), not behavioral; this check is the
  # carrier, red-verified against the pre-A1 base (housekeeping.rs,
  # pull.rs, open_attempts.rs carried the literal). Any other occurrence is an open-coded floor read the
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
  # — directive-2 record (bug_273's coverage half, relocated from
  # the retired fence map): no model — the property is an
  # ENUMERATION over the crate's SQL surface; the carriers are
  # db/tests/fence_coverage.rs (source-enumerating) plus this check
  # for the actor/grpc/admin side, red-verified against the base
  # (housekeeping.rs, pull.rs, materialize.rs carried raw decision
  # SQL).
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
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "ts" || f.hasExt "svelte") ../rio-dashboard)
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
                # bug_048: dashboard panel descriptions (.json) carry the
                # same operator-facing prose drift surface as the yaml —
                # the "lands in P0539d; No Data expected" class lived
                # exactly here, invisible to every scan.
                "json"
              ]
            ) ../infra)
            ../.github
            ../.cargo
          ];
        };
        # merged_bug_201: the reserved-proto-field prose tier reads the
        # `reserved "name"` declarations as its truth source — a field
        # removal fails docs-lint until every spec citation is re-swept.
        protoSrc = pkgs.lib.fileset.toSource {
          root = ../rio-proto/proto;
          fileset = pkgs.lib.fileset.fileFilter (f: f.hasExt "proto") ../rio-proto/proto;
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
        protosJson = ../docs/gen/protos.json;
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
        # bug_323: every `message X {` quoted inside a ```protobuf
        # fence in docs/spec must exist in rio-proto/proto (the
        # docs/gen/protos.json message inventory) — a deleted wire
        # message's spec listing goes red instead of surviving as dead
        # protocol prose. Natural red at introduction: SchedulerMessage
        # (deleted with the BuildExecution stream; its proto.typ
        # listing survived until this check's commit). Removed-section
        # treatments quote retired messages in PROSE or reserved-field
        # comments, never as a fresh `message X {` listing.
        livemsgs=$(jq -r '[.[].messages[]] | unique | .[]' $protosJson | paste -sd'|')
        if [[ -z "$livemsgs" ]]; then
          echo "FAIL: protos.json carries no message inventory — regen docs-data" >&2
          fail=1
        fi
        fencehits=$(find $typSrc/spec -name '*.typ' -exec awk '
          /^```protobuf/ { f = 1; next }
          /^```/         { f = 0 }
          f && /^message [A-Za-z_]+/ { print FILENAME ":" FNR ":" $2 }
        ' {} + || true)
        while IFS=: read -r mf ml mname; do
          [[ -z "$mname" ]] && continue
          if ! grep -qxE "($livemsgs)" <<<"$mname"; then
            echo "FAIL: $mf:$ml quotes \`message $mname\` but no .proto defines it —" >&2
            echo "the listing documents a deleted wire message (bug_323 class)." >&2
            echo "Convert the section to a removed-treatment or delete the fence." >&2
            fail=1
          fi
        done <<<"$fencehits"
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
        # merged_bug_227: book-pdf.typ stitches from a #let chapters
        # array (one quoted path per line) — extract from the array,
        # and fail LOUDLY if the shape drifts (an empty extraction
        # would make this subset check vacuously green).
        # merged_bug_193: the extraction requires the exact
        # two-space-indent + trailing-comma line shape, so a valid but
        # non-conforming entry (e.g. a comma-free last element) was
        # silently DROPPED while the loud-fail only covered the
        # all-lines-drifted case. Cross-check the extracted count
        # against the RAW quoted-.typ count inside the array span —
        # any deviating line is now a hard error, not a silent skip.
        chapters_extract() {
          grep -oE '^  "[^"]+\.typ",' "$1" | sed 's/^  "//;s/",$//'
        }
        chapters_raw_count() {
          sed -n '/^#let chapters = (/,/^)/p' "$1" | grep -cE '"[^"]+\.typ"'
        }
        # Planted red (banner (b)): a comma-free last entry must be a
        # loud mismatch, never a silent drop.
        plant="$TMPDIR/book-pdf-planted.typ"
        printf '%s\n' '#let chapters = (' '  "a.typ",' '  "b.typ"' ')' > "$plant"
        if [[ "$(chapters_extract "$plant" | grep -c . || true)" -eq "$(chapters_raw_count "$plant")" ]]; then
          echo "SELF-TEST FAIL: comma-free chapter entry not detected by the count cross-check" >&2
          exit 1
        fi
        pdf=$(chapters_extract $typSrc/book-pdf.typ)
        if [[ -z "$pdf" ]]; then
          echo "FAIL: no chapters extracted from book-pdf.typ's chapters array — the stitch shape changed; update this extraction WITH it" >&2
          fail=1
        fi
        raw_n=$(chapters_raw_count $typSrc/book-pdf.typ)
        got_n=$(printf '%s\n' "$pdf" | grep -c . || true)
        if [[ "$raw_n" -ne "$got_n" ]]; then
          echo "FAIL: book-pdf.typ chapters array carries $raw_n quoted .typ entries but the subset extraction matched $got_n —" >&2
          echo "      a line deviates from the '  \"…\",' shape (comma-free last entry?); fix the array line or this extraction WITH it" >&2
          fail=1
        fi
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
        # and the deleted rio-scheduler/src/logs/ tree.
        #
        # META-RULE (merged_bug_140 close): a narrowing record — any
        # comment here explaining why a token is NOT denied — MUST
        # embed its verification grep and that grep's output AT THE
        # TIME OF WRITING. The previous record here claimed the
        # stream-era concept tokens could not be denied because
        # "dozens of legitimate historical citations remain and a
        # 30-entry allowlist would bury the signal" — with no
        # verification attached. The slot-12 sweep falsified it: 78
        # bare-token lines were live narrations, not citations, and
        # after rewording them the survivors all fit one ESCAPE
        # pattern (same-line retirement qualifier), needing zero
        # allowlist entries. The concept tier below is that close.
        # Two narrowing records, verification embedded:
        #  - bare `Heartbeat` is NOT in deny_concept: the lease domain
        #    owns the word live. Verified 2026-06-06:
        #      rg -in '\bheartbeats?\b' --type rust \
        #        rio-{lease,scheduler,controller,store,builder}/src | wc -l
        #      => 248 live hits (395 tree-wide) — only the
        #    HeartbeatRequest/Response message names and the
        #    "Heartbeat RPC/unary" phrase are retired.
        #  - `cilium-gateway-` is NOT denied: it is Cilium's live
        #    naming for generated Gateway workloads. Verified
        #    2026-06-06: rg -n 'cilium-gateway-' =>
        #      infra/helm/rio-build/templates/dashboard-gateway.yaml:11,
        #      nix/tests/fixtures/k3s-full.nix:543,
        #      nix/tests/scenarios/dashboard-gateway.nix:53 (3 hits,
        #    all live-true). docker.nix's falsity was the envsubst
        #    claim, denied exactly via deny_concept.
        #
        # Split into shared/docs/cross (R7-m025): a single alternation
        # over both scan sets is the structural reason "widen pattern X
        # → false-positive in the other scan set" recurs. deny_shared
        # is identifiers retired everywhere; deny_docs adds doc-only
        # phrases (legitimately appear in code as historical context);
        # deny_cross adds case/separator variants needed for nix/infra
        # that would FP docs' "Squid FOD proxy is deleted" prose.
        deny_shared='\bBuilderPool\b|\bFetcherPools?\b|rio-cli bps\b|`bps`|vm-lifecycle-bps|RIO_TLS__|\bTlsError\b|rio-common/src/tls\.rs|load_client_tls|init_client_tls|spec\.sizing|Sizing::|fuseCacheBudget|logBudget|migration-lock mechanism|trigger-gc|--grace-period-hours|mTLS client[- ]cert|mTLS cert mount|mTLS main port|VMs: mTLS|plaintext-health listener|TLS and plaintext ports|mTLS bypass|mTLS-identified|mTLS identifies|falls? back to mTLS|mTLS peer cert|\bplaintext port\b|CN-allowlist\)|\(gateway cert|dev-mode/dev-mode|TLS is env-only|\bTLS init\b|without relying on service tokens|replacement for the service-HMAC|RIO_JWT_SIGNING_KEY_PATH|rio\.jwt(Verify|Sign)Env|worker\.seccomp|`tls` / `metrics_addr`|\brio-worker\b|\bReadyQueue\b|\bpush_ready\b|\bqueue_priority\b|\bINTERACTIVE_BOOST\b|\bseed_ready_queue\b|\brearm_materialization_job\b|\btrim_chunk\b|\bDEFAULT_PEER_URL_TEMPLATE\b|\btick_publish_gauges\b|\bpull_attempt_seen_open\b|\bclosure_vouched\b|\bFencedWrite\b|\brollback_assignment\b|\bCOLLECT_CURSOR\b|\bCOLLECT_BACKLOG_ESTIMATE\b|rio_scheduler_workers_active|rio_scheduler_queue_depth|rio-scheduler/src/logs/|store-side 4096|[Tt]emplate brackets \\{pod\\}|Bracketed for v6-only|\bfold_tenant_reprobes\b'
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
        # CONCEPT tier (merged_bug_140 keystone): retired *concepts* —
        # protocol names, design phrases, falsified claims — may be
        # cited historically but never narrated as live. A line
        # matching deny_concept passes ONLY if the SAME LINE carries a
        # retirement qualifier (concept_escape). Same-line is the
        # point: a qualifier on the adjacent line reads as live text
        # when quoted alone — the sweep that introduced this tier
        # tripped its own author three times exactly that way.
        # Self-allowlist: misc-checks.nix only (this file names the
        # tokens to deny them).
        deny_concept='\bBuildExecution\b|\bCancelSignal\b|\bHeartbeatRequests?\b|\bHeartbeatResponses?\b|Heartbeat.{0,2}(RPC|unary)|\b[Rr]eady[- ]queues?\b|\bready_queue\b|\bDrainExecutor\b|terminationGracePeriodSeconds: 7200|blocks until its single in-flight build|Baked-in beats runtime envsubst|Forward-compat.*lands in P[0-9]|lands in P[0-9].*No Data|prox(y|ies|ying).{0,60}(Cilium|Envoy) Gateway|\(no series\).*never fires|[Uu]ndefined means .{0,4}.whole.{0,3}build|whole.?build view \(drvPath undefined\)|no derivation filter on the log stream'
        # merged_bug_081: every escape token WORD-BOUND — the old
        # unanchored vocabulary legalized live narration via substrings
        # ("unremoved", "pre-pulling") and via unrelated matches in the
        # composite grep stream.
        concept_escape='\blegacy\b|\bstream-era\b|\bremoved\b|\bretired\b|\bdeleted\b|\bno longer\b|\bwas the\b|\bnever sent\b|\bpre-pull\b|\bhistorical(ly)?\b|\breplaced\b|\bgone from\b'
        concept_scan() {
          local dir=$1 label=$2 hits="" hit content
          # merged_bug_081 arm fixes: (1) the self-allowlist is a
          # filename --exclude on the INITIAL grep — the old
          # `grep -v misc-checks.nix` matched in CONTENT, exempting any
          # prose that merely cited this lint file; (2) the escape
          # filter runs on the CONTENT FIELD ONLY — the old pipeline
          # filtered grep's composite path:line:content stream, so an
          # escape word in a file PATH exempted the whole file.
          while IFS= read -r hit; do
            [[ -z $hit ]] && continue
            content=''${hit#*:}
            content=''${content#*:}
            if ! grep -qiE "$concept_escape" <<<"$content"; then
              hits+="$hit"$'\n'
            fi
          done < <(grep -rn --exclude=misc-checks.nix -E "$deny_concept" "$dir" || true)
          if [[ -n "$hits" ]]; then
            echo "FAIL: retired concept narrated as live in $label —" >&2
            echo "add a same-line retirement qualifier ($concept_escape)" >&2
            echo "or rewrite to the live mechanism:" >&2
            printf '%s' "$hits" >&2
            fail=1
          fi
        }
        # Self-test before the real scans (banner (b): one red per
        # filter arm): planted red MUST trip; qualified green MUST
        # pass; an escape word in a file PATH must NOT exempt
        # (merged_bug_081 arm 1); content citing misc-checks.nix must
        # NOT exempt (arm 2); an escape-token SUBSTRING must NOT
        # exempt (arm 3, word-bounding).
        mkdir -p "$TMPDIR/c1red" "$TMPDIR/c1green" "$TMPDIR/c1path/removed-docs" "$TMPDIR/c1cite" "$TMPDIR/c1substr"
        echo 'the scheduler routes work over the BuildExecution stream' > "$TMPDIR/c1red/doc.typ"
        # bug_048 token: the panel-description deferral class.
        echo 'metric lands in P0539d; shows No Data until then' > "$TMPDIR/c1red/panel.json"
        # merged_bug_076 token: the retired nginx->Gateway east-west hop.
        echo 'nginx proxies gRPC-Web POSTs to the Envoy Gateway listener' > "$TMPDIR/c1red/hop.nix"
        # merged_bug_006 token: the removed DrainExecutor RPC.
        echo 'operators drain a worker via the DrainExecutor RPC' > "$TMPDIR/c1red/drain.rs"
        # merged_bug_063 token: the retired whole-build LogViewer mode
        # narrated as live (.svelte is in-scope since the crossSrc
        # fileset widening that shipped with this token).
        echo 'Undefined means "whole build" (no derivation filter on the log stream)' > "$TMPDIR/c1red/prop.svelte"
        echo 'the removed DrainExecutor RPC once drained workers' > "$TMPDIR/c1green/drain.rs"
        echo 'the retired whole-build view (drvPath undefined) no longer mounts' > "$TMPDIR/c1green/prop.svelte"
        echo 'the removed BuildExecution stream routed work (stream-era)' > "$TMPDIR/c1green/doc.typ"
        echo 'historical note: this text said it lands in P0539d; shows No Data — removed' > "$TMPDIR/c1green/panel.json"
        echo 'nginx no longer proxies to the Cilium Gateway listener (registry-direct now)' > "$TMPDIR/c1green/hop.nix"
        echo 'the scheduler routes work over the BuildExecution stream' > "$TMPDIR/c1path/removed-docs/doc.typ"
        echo 'the BuildExecution stream is checked by misc-checks.nix today' > "$TMPDIR/c1cite/doc.typ"
        echo 'an unremoved BuildExecution stream still routes work' > "$TMPDIR/c1substr/doc.typ"
        prevfail=$fail
        fail=0
        concept_scan "$TMPDIR/c1red" "self-test" 2>/dev/null
        if [[ $fail -eq 0 ]]; then
          echo "SELF-TEST FAIL: concept tier missed the planted red" >&2
          exit 1
        fi
        fail=0
        concept_scan "$TMPDIR/c1green" "self-test"
        if [[ $fail -ne 0 ]]; then
          echo "SELF-TEST FAIL: concept tier flagged the qualified fixture" >&2
          exit 1
        fi
        fail=0
        concept_scan "$TMPDIR/c1path" "self-test" 2>/dev/null
        if [[ $fail -eq 0 ]]; then
          echo "SELF-TEST FAIL: an escape word in the file PATH exempted a live-narration line (merged_bug_081 arm 1)" >&2
          exit 1
        fi
        fail=0
        concept_scan "$TMPDIR/c1cite" "self-test" 2>/dev/null
        if [[ $fail -eq 0 ]]; then
          echo "SELF-TEST FAIL: content citing misc-checks.nix exempted a live-narration line (merged_bug_081 arm 2)" >&2
          exit 1
        fi
        fail=0
        concept_scan "$TMPDIR/c1substr" "self-test" 2>/dev/null
        if [[ $fail -eq 0 ]]; then
          echo "SELF-TEST FAIL: an escape-token substring (unremoved) exempted a live-narration line (merged_bug_081 arm 3)" >&2
          exit 1
        fi
        fail=$prevfail
        concept_scan "$typSrc" "docs"
        concept_scan "$crossSrc" "non-doc sources"
        # merged_bug_201: spec prose must not cite a `Message.field`
        # whose field name is RESERVED in the .proto files unless the
        # same line carries a retirement qualifier — so a field-removal
        # sweep fails docs-lint until every spec citation is re-swept.
        # Dotted CamelCase-prefixed citations only (`Foo.bar_baz`): the
        # bare names (generation, factor, ack, ...) are ordinary words,
        # and ALL-CAPS prefixes (SQL `EXCLUDED.generation`) are not
        # proto messages.
        reserved_rx=$(grep -rhoE 'reserved "[a-z_]+"' "$protoSrc" \
          | sed 's/reserved "//;s/"//' | sort -u | paste -sd'|')
        reserved_scan() {
          local dir=$1 label=$2 hits="" hit content
          [[ -z $reserved_rx ]] && return 0
          while IFS= read -r hit; do
            [[ -z $hit ]] && continue
            content=''${hit#*:}
            content=''${content#*:}
            if ! grep -qiE "$concept_escape" <<<"$content"; then
              hits+="$hit"$'\n'
            fi
          done < <(grep -rn -E "\b[A-Z][a-z][A-Za-z]*\.($reserved_rx)\b" "$dir" || true)
          if [[ -n "$hits" ]]; then
            echo "FAIL: prose in $label cites a RESERVED proto field as live —" >&2
            echo "add a same-line retirement qualifier ($concept_escape) or re-sweep the site:" >&2
            printf '%s' "$hits" >&2
            fail=1
          fi
        }
        # R7: one red fixture per arm — planted red trips, qualified
        # green passes (uses the first reserved name so the fixture
        # auto-tracks the alphabet).
        first_reserved=''${reserved_rx%%|*}
        mkdir -p "$TMPDIR/r2red" "$TMPDIR/r2green"
        echo "the scheduler still reports SomeMessage.$first_reserved every tick" > "$TMPDIR/r2red/doc.typ"
        echo "the removed SomeMessage.$first_reserved field is reserved in the proto" > "$TMPDIR/r2green/doc.typ"
        prevfail=$fail
        fail=0
        reserved_scan "$TMPDIR/r2red" "self-test" 2>/dev/null
        if [[ $fail -eq 0 ]]; then
          echo "SELF-TEST FAIL: reserved-field tier missed the planted red" >&2
          exit 1
        fi
        fail=0
        reserved_scan "$TMPDIR/r2green" "self-test"
        if [[ $fail -ne 0 ]]; then
          echo "SELF-TEST FAIL: reserved-field tier flagged the qualified fixture" >&2
          exit 1
        fi
        fail=$prevfail
        reserved_scan "$typSrc" "spec docs"
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
        # bug_136: the 0-prefixed shapes silently expired at migration
        # 100 (a "(103)" shorthand passed unchallenged). Widened to
        # [0-9]{3} where the form is unambiguous: the migration-NNN
        # hyphen shape everywhere, the (NNN) paren shape in ops
        # runbooks (where the corruption class lived). Spec chapters
        # legitimately parenthesize 3-digit protocol literals
        # (gateway.typ activity ids), so the paren ban stays 0NN
        # outside ops — the slug validation below is 3-digit
        # everywhere and is the load-bearing check; the canary at the
        # bottom forces a revisit before 4 digits.
        if grep -rn -E '\bmigrations?-[0-9]{3}\b' $typSrc $crossSrc \
             | grep -vE '/rio-migrations/|misc-checks\.nix'; then
          echo "FAIL: migration-NNN shorthand — use the NNN_slug stem" >&2
          fail=1
        fi
        if grep -rn -E '\([0-9]{3}\)' $typSrc/ops; then
          echo "FAIL: (NNN) shorthand in an ops runbook — use the NNN_slug stem" >&2
          fail=1
        fi
        if grep -rn -E '\(0[0-9]{2}\)' $typSrc $crossSrc \
             | grep -vE '/rio-migrations/|misc-checks\.nix'; then
          echo "FAIL: (0NN) shorthand — use the NNN_slug stem" >&2
          fail=1
        fi
        while IFS= read -r tok; do
          if ! jq -e --arg t "$tok" '.stems | map(startswith($t)) | any' $migrationsJson > /dev/null; then
            echo "FAIL: migration slug token '$tok' matches no migrations/ filename" >&2
            fail=1
          fi
        done < <(grep -rohE '\b[0-9]{3}_[a-z][a-z0-9_]*' $typSrc $crossSrc \
          | grep -vE '^[0-9]{3}_(u(8|16|32|64|128|size)|i(8|16|32|64|128|size)|f(32|64))$' \
          | sort -u)
        # bug_136 canary: these regexes hardcode a 3-digit stem width.
        # Synthesized from the live inventory so it expires LOUDLY
        # while there is still headroom (max+6 crossing 1000) instead
        # of silently un-validating migration 1000_*.
        maxstem=$(jq -r '.stems | map(split("_")[0] | tonumber) | max' $migrationsJson)
        if [ "$((maxstem + 6))" -ge 1000 ]; then
          echo "FAIL: migration numbering is approaching 4 digits" >&2
          echo "  (max stem $maxstem). Re-widen the docs-lint slug and" >&2
          echo "  shorthand regexes in nix/misc-checks.nix (bug_136" >&2
          echo "  canary — they hardcode a 3-digit width) and bump this" >&2
          echo "  canary's boundary." >&2
          fail=1
        fi
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
  # Both implement r[dash.auth.method-gate+5]; before this check the
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
          echo "      Both implement r[dash.auth.method-gate+5] — keep them in sync." >&2
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

  # live_056-a (bughunt-9 W9-CQ, the founding plant): the cilium
  # identity-label filter is a COMMITTED default in both render paths.
  # 8.3K per-build-Job CiliumIdentities (k8s:job-name/controller-uid
  # identity-relevant by default × the Job-per-build plane) collapsed
  # policy enforcement and blackholed builder→store cluster-wide; the
  # k3s fixture reproduced the explosion shape at small scale,
  # unasserted — this check is the assertion. It renders the SAME
  # chart the VM tests boot (nix/cilium-render.nix — whose share-pin
  # assert keys the chart to nix/pins.toml's cilium version, so a pin
  # bump re-validates the filter semantics by ritual) and asserts:
  #   (i)  the cilium-config carries the labels filter line;
  #   (ii) the filter is EXCLUSIONS-ONLY — every pattern is
  #        `!`-prefixed (v1.19.4 semantics: exclusions SUBTRACT from
  #        the default identity-relevant set; ONE non-`!` inclusion
  #        pattern flips the whole filter to whitelist-mode and
  #        REPLACES the default set — the unsafe shape this guard
  #        makes unshippable).
  # bug_104 (bughunt-10): ONE value, THREE consumers. The filter is
  # single-sourced at nix/pins.toml addons.cilium.identity_label_filter
  # and this check asserts every consumer renders that one pin:
  #   (a) the k3s/VM chart render (cilium-render.nix reads the pin);
  #   (b) infra/eks/addons.tf, which consumes
  #       var.addons.cilium.identity_label_filter — the value flows
  #       pins.toml → `cargo xtask regen tfvars` →
  #       generated.auto.tfvars.json (the tfvars-fresh check pins THAT
  #       edge); this check STAGES addons.tf and the generated tfvars
  #       ((vvvvv): they are part of the quantified universe, so they
  #       are build inputs — the OLD check rendered only the nix chart,
  #       and an edit through the CI-enforced nix+want pair stranded
  #       the production copy with CI green);
  #   (c) this check's own `want`, which IS the pin now, not a third
  #       copy.
  # Scoped honestly: this certifies "the committed pin renders into
  # every lane"; the live identity-cardinality readback rides the
  # owner-queue act list (the W9-CR record, unchanged by this close).
  cilium-labels-filter =
    let
      rendered = import ./cilium-render.nix {
        inherit pkgs;
        inherit (inputs) nixhelm;
        system = pkgs.stdenv.hostPlatform.system;
      };
      pins = import ./pins.nix;
      want = pins.addons.cilium.identity_label_filter;
    in
    pkgs.runCommand "rio-cilium-labels-filter"
      {
        addonsTf = ../infra/eks/addons.tf;
        generatedTfvars = ../infra/eks/generated.auto.tfvars.json;
        nativeBuildInputs = [ pkgs.jq ];
      }
      ''
        cfg=${rendered}/02-cilium.yaml
        line=$(grep -E '^  labels: ' "$cfg" || true)
        if [ -z "$line" ]; then
          echo "FAIL: rendered cilium-config carries NO identity-label filter —" >&2
          echo "every ephemeral builder Job mints a fresh CiliumIdentity" >&2
          echo "(live_056-a: 8.3K identities collapsed policy enforcement)." >&2
          echo "Expected from nix/pins.toml identity_label_filter:" >&2
          echo "  labels: ${want}" >&2
          exit 1
        fi
        case "$line" in
          *"${want}"*) : ;;
          *)
            echo "FAIL: rendered cilium labels filter drifted from the pins.toml value:" >&2
            echo "  got:  $line" >&2
            echo "  want: labels: ${want}" >&2
            exit 1
            ;;
        esac
        # Whitelist-mode guard (exclusions-only law): every pattern
        # token must be !-prefixed after the k8s: source prefix.
        for tok in $(printf '%s' "$line" | sed 's/^  labels: //; s/"//g'); do
          case "$tok" in
            k8s:!*) : ;;
            *)
              echo "FAIL: labels filter contains a non-exclusion pattern '$tok' —" >&2
              echo "one inclusion pattern flips cilium to whitelist-mode and" >&2
              echo "REPLACES the default identity-relevant set (v1.19.4 semantics)." >&2
              exit 1
              ;;
          esac
        done
        # (b) the production tf lane: addons.tf consumes the variable
        # and carries NO open-coded copy of the filter.
        grep -q 'labels = var.addons.cilium.identity_label_filter' "$addonsTf" || {
          echo "FAIL: infra/eks/addons.tf no longer consumes" >&2
          echo "var.addons.cilium.identity_label_filter — the production render" >&2
          echo "lane detached from the pins.toml single source (bug_104)." >&2
          exit 1
        }
        if grep -nE 'labels *= *"k8s:' "$addonsTf"; then
          echo "FAIL: infra/eks/addons.tf carries an open-coded labels filter" >&2
          echo "copy — the single source is nix/pins.toml" >&2
          echo "addons.cilium.identity_label_filter (bug_104)." >&2
          exit 1
        fi
        # The tfvars emission carries the pin verbatim (the value
        # addons.tf actually reads at plan time).
        got=$(jq -r '.addons.cilium.identity_label_filter // empty' "$generatedTfvars")
        if [ "$got" != "${want}" ]; then
          echo "FAIL: infra/eks/generated.auto.tfvars.json identity_label_filter" >&2
          echo "does not match nix/pins.toml:" >&2
          echo "  got:  $got" >&2
          echo "  want: ${want}" >&2
          echo "run: cargo xtask regen tfvars" >&2
          exit 1
        fi
        touch $out
      '';

  # merged_bug_024: the quota-volume selection's unit tier. Runs the
  # REAL nix/nixos-node/quota-volume-select.sh (staged — (vvvvv)/(wwwww):
  # the script file is the check input AND the check asserts
  # eks-node.nix still consumes it, so the embed is pinned both ways)
  # against fixture by-id namespaces DERIVED FROM THE PRODUCERS:
  #
  #   - the partition layouts are the locked nixpkgs
  #     make-disk-image.nix recipes (legacy+gpt: part1 = bios_grub,
  #     no fs, never mounted; part2 = ext4 root | efi: part1 = ESP
  #     fat32 at /boot; part2 = ext4 root) — the populations the
  #     ami-bios/ami variants boot with;
  #   - the by-id link shapes are udev's persistent-storage grammar
  #     (one whole-disk link `…vol<id>` plus one `…vol<id>-partN` per
  #     partition; nsid twins `…vol<id>_1`).
  #
  # The pre-fix selection (children/mountpoint side effects only)
  # counted the bios_grub partition as a bare candidate: n_bare=2 →
  # exit 1 → kubelet Requires= hard-fail on EVERY ami-bios boot (the
  # I-205 churn shape), and with the quota volume late/absent SELECTED
  # bios_grub for mkfs.xfs -f. The typed predicate (name-class
  # `*-part[0-9]*` reject + lsblk TYPE==disk) makes both impossible by
  # class; the plant battery below spans the predicate's shape space
  # so a degenerate mutation of any clause turns at least one
  # population red (R31'(v): fixture populations derive from
  # producers; the K-mutation discipline for an authored predicate).
  quota-volume-select =
    pkgs.runCommand "rio-quota-volume-select"
      {
        selectScript = ../nix/nixos-node/quota-volume-select.sh;
        eksNode = ../nix/nixos-node/eks-node.nix;
        nativeBuildInputs = [ pkgs.bash ];
      }
      ''
        # Bidirectional pin: the unit must consume the tested script.
        grep -q 'quota-volume-select.sh' "$eksNode" || {
          echo "FAIL: eks-node.nix no longer references quota-volume-select.sh —" >&2
          echo "the unit and this check have detached (the selection logic" >&2
          echo "under test is not the logic the fleet boots)" >&2
          exit 1
        }

        # Mock lsblk: consults LSBLK_TABLE rows
        # `<resolved-path>|<TYPE>|<children, space-sep>|<mountpoints>`.
        # Implements exactly the three invocations the script makes.
        mkdir -p bin
        cat > bin/lsblk << 'MOCK'
        #!/usr/bin/env bash
        dev="''${@: -1}"
        row=$(grep -F "$dev|" "$LSBLK_TABLE" | head -n1)
        IFS='|' read -r _ type children mounts <<< "$row"
        case "$*" in
          *-ndo\ TYPE*) echo "$type" ;;
          *-nro\ NAME*) echo self; for c in $children; do echo "$c"; done ;;
          *-nro\ MOUNTPOINTS*) echo "$mounts" ;;
        esac
        MOCK
        # The sandbox has no /usr/bin/env — point the shebang at the
        # staged bash directly.
        sed -i "1s|.*|#!$(command -v bash)|" bin/lsblk
        chmod +x bin/lsblk
        export PATH=$PWD/bin:$PATH

        # The script canonicalizes candidates (readlink -f); key the
        # mock table on the canonical base so a symlinked sandbox
        # build dir cannot desynchronize the lookup.
        base=$(readlink -f "$PWD")

        prefix=nvme-Amazon_Elastic_Block_Store_

        # mkpop NAME 'link:dev' ... — builds dev tree pop-NAME/dev with
        # by-id links; devices created as plain files; the table is
        # written by the caller.
        mkpop() {
          local name=$1; shift
          mkdir -p "pop-$name/dev/disk/by-id"
          local spec link devf
          for spec in "$@"; do
            link=''${spec%%:*}; devf=''${spec#*:}
            : > "pop-$name/dev/$devf"
            ln -sf "../../$devf" "pop-$name/dev/disk/by-id/$prefix$link"
          done
        }

        run() { # run POP -> sel = stdout selection; stderr to err; rc
          rc=0
          sel=$(bash "$selectScript" "$base/pop-$1/dev/disk/by-id/$prefix"'vol*' 2>err) || rc=$?
        }

        fail() { echo "FAIL($1): $2" >&2; cat err >&2 || true; exit 1; }

        # ── A: ami-bios (legacy+gpt) full population ──────────────────
        mkpop A volR:R 'volR-part1:Rp1' 'volR-part2:Rp2' volQ:Q
        cat > tA << EOF
        $base/pop-A/dev/R|disk|p1 p2|
        $base/pop-A/dev/Rp1|part||
        $base/pop-A/dev/Rp2|part||/
        $base/pop-A/dev/Q|disk||
        EOF
        export LSBLK_TABLE=$PWD/tA
        run A
        [ "$rc" = 0 ] && [ "$sel" = "$base/pop-A/dev/Q" ] \
          || fail A "ami-bios population must select the quota volume (got rc=$rc sel=$sel) — the bios_grub partition re-entered the bare fold"
        grep -q '2 partition-link' err || fail A "the operator trail must count both partition links"

        # ── B: ami-bios, quota volume late/absent — the mkfs corner ──
        mkpop B volR:R 'volR-part1:Rp1' 'volR-part2:Rp2'
        cat > tB << EOF
        $base/pop-B/dev/R|disk|p1 p2|
        $base/pop-B/dev/Rp1|part||
        $base/pop-B/dev/Rp2|part||/
        EOF
        export LSBLK_TABLE=$PWD/tB
        run B
        [ "$rc" = 1 ] || fail B "no-quota population must refuse (got rc=$rc sel=$sel) — pre-fix this SELECTED bios_grub for mkfs.xfs -f"
        grep -q 'NO bare quota volume' err || fail B "refusal must keep the live_060 operator message"
        grep -q '2 partition-link' err || fail B "refusal trail must show the partitions died by class"

        # ── C: uefi (efi recipe) — the winner is byte-stable ─────────
        mkpop C volR:R 'volR-part1:Rp1' 'volR-part2:Rp2' volQ:Q
        cat > tC << EOF
        $base/pop-C/dev/R|disk|p1 p2|
        $base/pop-C/dev/Rp1|part||/boot
        $base/pop-C/dev/Rp2|part||/
        $base/pop-C/dev/Q|disk||
        EOF
        export LSBLK_TABLE=$PWD/tC
        run C
        [ "$rc" = 0 ] && [ "$sel" = "$base/pop-C/dev/Q" ] \
          || fail C "uefi population must keep the pre-fix winner (got rc=$rc sel=$sel)"

        # ── D: the plant battery (predicate shape space; each row
        #      kills one clause mutation) ───────────────────────────────
        # D1 name-class: a -part1 link whose TYPE reads disk (taxonomy
        # alone would admit it) still dies by name.
        mkpop D1 'volL-part1:L1' volQ:Q
        cat > tD1 << EOF
        $base/pop-D1/dev/L1|disk||
        $base/pop-D1/dev/Q|disk||
        EOF
        export LSBLK_TABLE=$PWD/tD1
        run D1
        [ "$rc" = 0 ] && [ "$sel" = "$base/pop-D1/dev/Q" ] && grep -q '1 partition-link' err \
          || fail D1 "deleting the name-class clause must turn this population ambiguous (got rc=$rc sel=$sel)"

        # D2 taxonomy: a suffix-free link resolving to TYPE=part (the
        # name class alone would admit it) dies by taxonomy.
        mkpop D2 volALIAS:P volQ:Q
        cat > tD2 << EOF
        $base/pop-D2/dev/P|part||
        $base/pop-D2/dev/Q|disk||
        EOF
        export LSBLK_TABLE=$PWD/tD2
        run D2
        [ "$rc" = 0 ] && [ "$sel" = "$base/pop-D2/dev/Q" ] && grep -q '1 non-disk' err \
          || fail D2 "deleting the TYPE clause must turn this population ambiguous (got rc=$rc sel=$sel)"

        # D3 taxonomy, loop class.
        mkpop D3 volLOOP:LP volQ:Q
        cat > tD3 << EOF
        $base/pop-D3/dev/LP|loop||
        $base/pop-D3/dev/Q|disk||
        EOF
        export LSBLK_TABLE=$PWD/tD3
        run D3
        [ "$rc" = 0 ] && [ "$sel" = "$base/pop-D3/dev/Q" ] || fail D3 "loop devices must die by taxonomy (got rc=$rc sel=$sel)"

        # D4 name-class, multi-digit suffix.
        mkpop D4 'volR-part10:P10' volQ:Q
        cat > tD4 << EOF
        $base/pop-D4/dev/P10|part||
        $base/pop-D4/dev/Q|disk||
        EOF
        export LSBLK_TABLE=$PWD/tD4
        run D4
        [ "$rc" = 0 ] && [ "$sel" = "$base/pop-D4/dev/Q" ] && grep -q '1 partition-link' err \
          || fail D4 "-part10 must die by name class (got rc=$rc sel=$sel)"

        # D5 ambiguity: two bare whole disks refuse loudly.
        mkpop D5 volQ:Q volSECOND:S
        cat > tD5 << EOF
        $base/pop-D5/dev/Q|disk||
        $base/pop-D5/dev/S|disk||
        EOF
        export LSBLK_TABLE=$PWD/tD5
        run D5
        [ "$rc" = 1 ] && grep -q '2 bare candidate volumes' err \
          || fail D5 "two bare disks must refuse as ambiguous (got rc=$rc sel=$sel)"

        # D6 dedup: the nsid twin link must not double-count.
        mkpop D6 volQ:Q volQ_1:Q
        cat > tD6 << EOF
        $base/pop-D6/dev/Q|disk||
        EOF
        export LSBLK_TABLE=$PWD/tD6
        run D6
        [ "$rc" = 0 ] && [ "$sel" = "$base/pop-D6/dev/Q" ] || fail D6 "nsid twin links must dedup to one candidate (got rc=$rc sel=$sel)"

        # D7 mounted: a mounted whole disk is somebody's filesystem.
        mkpop D7 volM:M volQ:Q
        cat > tD7 << EOF
        $base/pop-D7/dev/M|disk||/data
        $base/pop-D7/dev/Q|disk||
        EOF
        export LSBLK_TABLE=$PWD/tD7
        run D7
        [ "$rc" = 0 ] && [ "$sel" = "$base/pop-D7/dev/Q" ] && grep -q '1 mounted' err \
          || fail D7 "mounted disks must be excluded (got rc=$rc sel=$sel)"

        # ── E: the VM shape — a direct non-by-id whole-disk path ─────
        mkdir -p pop-E/dev
        : > pop-E/dev/vdb
        cat > tE << EOF
        $base/pop-E/dev/vdb|disk||
        EOF
        export LSBLK_TABLE=$PWD/tE
        rc=0; sel=$(bash "$selectScript" "$base/pop-E/dev/vdb" 2>err) || rc=$?
        [ "$rc" = 0 ] && [ "$sel" = "$base/pop-E/dev/vdb" ] || fail E "the VM-config direct-path shape must keep selecting (got rc=$rc sel=$sel)"

        touch $out
      '';
}
# The quint/TLC protocol-model checks and the mbt-* conformance checks
# used to be spliced in here; they are now imported directly by
# flake.nix (the `quintChecks` binding) so the CI matrix can give them
# their own `formal` kind. checks.* still contains them.
