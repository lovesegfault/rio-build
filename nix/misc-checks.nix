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

  # Spec-coverage validation: fails on broken r[...]
  # references, duplicate requirement IDs, or unparseable
  # include files. Does NOT fail on uncovered/untested — those
  # are informational.
  #
  # Uses cleanSource because tracey needs docs/**/*.md and
  # .config/tracey/config.styx. tracey's daemon writes
  # .tracey/daemon.sock under the working dir, so we cp to a
  # writable tmpdir first.
  #
  # .claude/ is excluded via fileset.difference — tracey's config
  # doesn't scan it, so including it in the drv src causes spurious
  # rebuilds on every tooling-file edit.
  tracey-validate =
    pkgs.runCommand "rio-tracey-validate"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.difference (pkgs.lib.fileset.fromSource (pkgs.lib.cleanSource ../.)) ../.claude;
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

  # CRD drift: crdgen output (split per-CRD) must equal the
  # committed infra/helm/crds/. Catches the "Rust CRD struct
  # changed but nobody ran cargo xtask regen crds" drift — the committed
  # YAML is what Argo syncs, so a stale file means the deployed
  # schema diverges from what the controller expects.
  #
  # Calls scripts/split-crds.py (same script xtask uses) to split
  # multi-doc → one file per metadata.name, into $TMPDIR.
  # diff -r: recursive, exits non-zero on any difference.
  crds-drift =
    let
      crdsYaml = config.packages.crds;
      py = pkgs.python3.withPackages (p: [ p.pyyaml ]);
    in
    pkgs.runCommand "rio-crds-drift"
      {
        nativeBuildInputs = [
          py
          pkgs.diffutils
        ];
      }
      ''
        mkdir -p $TMPDIR/split
        python3 ${../scripts/split-crds.py} ${crdsYaml} $TMPDIR/split
        diff -r $TMPDIR/split ${../infra/helm/crds} > $TMPDIR/diff || {
          echo "FAIL: crdgen output drifted from infra/helm/crds/" >&2
          echo "Run: cargo xtask regen crds" >&2
          cat $TMPDIR/diff >&2
          exit 1
        }
        touch $out
      '';

  # infra/eks/generated.auto.tfvars.json must match nix/pins.nix.
  # Same pattern as crds-drift: regenerate-then-diff. jq on both
  # sides so key-order and whitespace don't matter (the committed
  # file is pretty-printed, writeText output is compact).
  tfvars-fresh =
    pkgs.runCommand "rio-tfvars-fresh"
      {
        nativeBuildInputs = [
          pkgs.jq
          pkgs.diffutils
        ];
      }
      ''
        jq -S . ${config.packages.tfvars} > $TMPDIR/gen
        jq -S . ${../infra/eks/generated.auto.tfvars.json} > $TMPDIR/committed
        diff $TMPDIR/gen $TMPDIR/committed || {
          echo "FAIL: nix/pins.nix drifted from infra/eks/generated.auto.tfvars.json" >&2
          echo "Run: nix build .#tfvars && jq -S . result > infra/eks/generated.auto.tfvars.json" >&2
          exit 1
        }
        touch $out
      '';
}
# Linux-only checks (dockerTools, nixosSystem). On Darwin
# miscChecks degrades to the policy checks above.
// pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
  # Seed↔ECR-push layer-digest parity. The seed warms
  # containerd's content store; the warm only works if the
  # seed's layer digests match what push.rs puts in ECR
  # (containerd checks blobs by digest). This rebuilds the
  # same skopeo transcode push.rs would do and asserts every
  # resulting blob is present in the seed. Builds the
  # builder+fetcher images — but those are already in the
  # VM-test closure, so no extra cold-cache cost in .#ci.
  executor-seed-layer-parity = dockerImages.executorSeedLayerParity;

  # Eval-only: instantiate both AMI nixosSystems so a typo in
  # the seedImages wiring (or any nixos-node module change)
  # fails .#ci without building the multi-GB disk image.
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
}
