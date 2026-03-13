# Render the Helm chart at Nix eval time for VM tests.
#
# `helm template` runs in a derivation → rendered YAML → split into
# numbered files → fed into services.k3s.manifests. k3s auto-applies
# in filename order (00-crds before 01-rbac before 02-workloads), which
# solves the RBAC bootstrap ordering that phase3a.nix:21-26 avoided by
# running the controller as systemd.
#
# vmtest disables the PG subchart (postgresql.enabled=false), but
# `helm template` still checks for its presence in charts/ — the
# `condition:` gates RENDERING, not the presence check. Symlink the
# nixhelm-fetched chart in, same as helm-lint does.
{
  pkgs,
  nixhelm,
  system,
}:
let
  subcharts = import ./helm-charts.nix { inherit nixhelm system; };
in
{
  # Path to a values file (typically values/vmtest.yaml).
  valuesFile,
  # --set overrides. Each becomes a "--set key=value" arg. Use for
  # per-test controller env (RIO_SCHEDULER_ADDR=control:9001, autoscaler
  # tuning, coverage toggle).
  extraSet ? { },
}:
let
  chart = pkgs.lib.cleanSource ../infra/helm/rio-build;
  crds = pkgs.lib.cleanSource ../infra/helm/crds;
  setArgs = pkgs.lib.concatMapStringsSep " " (
    k: "--set ${pkgs.lib.escapeShellArg "${k}=${toString extraSet.${k}}"}"
  ) (builtins.attrNames extraSet);
in
pkgs.runCommand "rio-helm-rendered"
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

    helm template rio . -f ${valuesFile} ${setArgs} > all.yaml

    mkdir -p $out
    # CRDs come from infra/helm/crds/ (not the chart — Helm's crds/ dir
    # is install-only, wrong for a dev-phase project). Cat them into
    # 00-crds.yaml so k3s applies them first.
    cat ${crds}/*.yaml > $out/00-crds.yaml
    # yq-go: filter by kind. k3s applies in filename order — CRDs must
    # establish before the controller pod tries to watch them, RBAC
    # must bind before the pod's SA token is validated against it.
    yq 'select(.kind == "ServiceAccount" or
               .kind == "ClusterRole" or
               .kind == "ClusterRoleBinding" or
               .kind == "Role" or
               .kind == "RoleBinding")' all.yaml > $out/01-rbac.yaml
    yq 'select(.kind != "ServiceAccount" and
               .kind != "ClusterRole" and
               .kind != "ClusterRoleBinding" and
               .kind != "Role" and
               .kind != "RoleBinding")' all.yaml > $out/02-workloads.yaml

    # Sanity: all three files have content (empty file = k3s silently
    # skips it, test passes wrongly).
    for f in $out/*.yaml; do
      test -s "$f" || { echo "rendered file $f is empty" >&2; exit 1; }
    done
  ''
