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
  rustStable,
  rustPlatformStable,
  traceyPkg,
  subcharts,
  dockerImages,
  nodeAmi,
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
  deny = pkgs.stdenv.mkDerivation {
    pname = "rio-deny";
    inherit version;
    src = pkgs.lib.fileset.toSource {
      root = unfilteredRoot;
      fileset = pkgs.lib.fileset.unions [
        ../.cargo/deny.toml
        workspaceFileset
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

  # Design-book build. mdbook exits non-zero on broken
  # SUMMARY.md links, malformed front-matter, or a failing
  # preprocessor (mdbook-mermaid). rustdoc (`doc` check) only
  # covers code docs — without this, docs/src/ breakage is
  # invisible until someone runs `mdbook serve` locally.
  #
  # Source is scoped to docs/ so unrelated edits don't rebuild.
  # Output is the rendered HTML (browsable via `result/index.html`).
  mdbook =
    pkgs.runCommand "rio-mdbook"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../docs;
          fileset = ../docs;
        };
        nativeBuildInputs = with pkgs; [
          mdbook
          mdbook-mermaid
        ];
      }
      ''
        mdbook build $src -d $out
      '';

  # Spec-coverage validation: fails on broken r[...]
  # references, duplicate requirement IDs, or unparseable
  # include files. Does NOT fail on uncovered/untested — those
  # are informational.
  #
  # tracey scans docs/**/*.md (spec) + .rs/.nix/.py for
  # `r[impl/verify ...]` annotations + .config/tracey/config.styx.
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
            (pkgs.lib.fileset.fileFilter (f: f.hasExt "rs" || f.hasExt "nix" || f.hasExt "py") unfilteredRoot)
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

        for f in ${fragments}/*.sh; do
          echo "▸ helm-lint: $(basename "$f" .sh)" >&2
          bash -euo pipefail "$f"
        done
        touch $out
      '';

  # proxy_buffering off in dashboardNginxConf is LOAD-BEARING
  # (docker.nix:349): nginx default-buffers upstream → WatchBuild /
  # GetBuildLogs streams arrive as one blob at close. The config is a
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
    # `nginx -t` resolves upstream hostnames at parse time, which the
    # sandbox can't do for the cluster FQDN — sed it to a sandbox-
    # resolvable address. Everything else (directives, regex, njs
    # module presence, js script syntax) is checked verbatim.
    mkdir -p $TMPDIR/logs
    sed 's/rio-scheduler\.rio-system\.svc\.cluster\.local/127.0.0.1/' \
      ${dockerImages.dashboardNginxConf} > $TMPDIR/nginx.conf
    ${dockerImages.dashboardNginx}/bin/nginx -t -p $TMPDIR -c $TMPDIR/nginx.conf
    touch $out
  '';

  # CRD drift: crdgen output (split per-CRD) must equal the
  # committed infra/helm/crds/. Catches the "Rust CRD struct
  # changed but nobody ran cargo xtask regen crds" drift — the committed
  # YAML is what Argo syncs, so a stale file means the deployed
  # schema diverges from what the controller expects.
  #
  # Calls scripts/split-crds.py (same script xtask uses) to split
  # multi-doc → one file per metadata.name.
  crds-drift = mkDriftCheck {
    name = "crds-drift";
    nativeBuildInputs = [ (pkgs.python3.withPackages (p: [ p.pyyaml ])) ];
    generate = ''
      mkdir -p $TMPDIR/gen
      python3 ${../scripts/split-crds.py} ${config.packages.crds} $TMPDIR/gen
    '';
    committed = ../infra/helm/crds;
    what = "crdgen output drifted from infra/helm/crds/";
    regenHint = "cargo xtask regen crds";
  };

  # infra/eks/generated.auto.tfvars.json must match nix/pins.nix.
  # jq -S on both sides so key-order and whitespace don't matter
  # (committed file is pretty-printed, writeText output is compact).
  tfvars-fresh = mkDriftCheck {
    name = "tfvars-fresh";
    nativeBuildInputs = [ pkgs.jq ];
    generate = ''
      jq -S . ${config.packages.tfvars} > $TMPDIR/gen
      jq -S . ${../infra/eks/generated.auto.tfvars.json} > $TMPDIR/committed
    '';
    committed = "$TMPDIR/committed";
    what = "nix/pins.nix drifted from infra/eks/generated.auto.tfvars.json";
    regenHint = "nix build .#tfvars && jq -S . result > infra/eks/generated.auto.tfvars.json";
  };
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

  # nginx allow-list (docker.nix dashboardReadonlyMethods) MUST equal
  # the Cilium Gateway rio-scheduler-readonly HTTPRoute's Exact paths.
  # Both implement r[dash.auth.method-gate+2]; before this check the
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
          echo "      Both implement r[dash.auth.method-gate+2] — keep them in sync." >&2
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
