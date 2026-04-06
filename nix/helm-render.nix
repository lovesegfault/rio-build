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
  subcharts = import ./helm-charts.nix { inherit pkgs nixhelm system; };
in
{
  # Path to a values file (typically values/vmtest.yaml).
  valuesFile,
  # Additional values files layered AFTER valuesFile (-f is
  # repeatable; last wins on key conflict). Used by the
  # privileged-hardening-e2e scenario to layer vmtest-full-nonpriv.
  # yaml on top of vmtest-full.yaml — the overlay stays minimal
  # (only the privileged:false + devicePlugin:enabled flip) instead
  # of duplicating the full 196-line base.
  extraValuesFiles ? [ ],
  # --set-string overrides. Each becomes a "--set-string key=value"
  # arg. --set-string (not --set): Helm's --set coerces "3" to int 3,
  # which breaks EnvVar.value (must be string). All current callers
  # pass env var values; if a caller needs int semantics later, add
  # a separate extraSetInt param rather than switching back.
  extraSet ? { },
  # --set overrides (typed). For bools/ints that must NOT be coerced
  # to string. --set-string coverage.enabled=true would make the
  # string "true" (truthy), but =false would make "false" (ALSO
  # truthy — non-empty string). --set respects YAML types.
  extraSetTyped ? { },
  # Passed as -n to helm template. The rio-build chart uses
  # .Values.namespace.name (not .Release.Namespace), so -n doesn't
  # affect rio-* resources. But the bitnami PG subchart uses
  # .Release.Namespace — without this, PG lands in `default` while
  # rio-* land in rio-system, and waitReady's `-n ${ns}` can't see it.
  namespace ? "default",
}:
let
  chart = pkgs.lib.cleanSource ../infra/helm/rio-build;
  crds = pkgs.lib.cleanSource ../infra/helm/crds;
  setStringArgs = pkgs.lib.concatMapStringsSep " " (
    k: "--set-string ${pkgs.lib.escapeShellArg "${k}=${toString extraSet.${k}}"}"
  ) (builtins.attrNames extraSet);
  # builtins.toJSON renders Nix bools/ints as YAML-parseable (true not
  # "true", 3 not "3"). Helm's --set parser reads YAML syntax.
  setTypedArgs = pkgs.lib.concatMapStringsSep " " (
    k: "--set ${pkgs.lib.escapeShellArg "${k}=${builtins.toJSON extraSetTyped.${k}}"}"
  ) (builtins.attrNames extraSetTyped);
  setArgs = "${setStringArgs} ${setTypedArgs}";
  extraValuesArgs = pkgs.lib.concatMapStringsSep " " (f: "-f ${f}") extraValuesFiles;
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
    ln -s ${subcharts.rustfs} charts/rustfs

    helm template rio . -n ${namespace} -f ${valuesFile} ${extraValuesArgs} ${setArgs} > all.yaml

    mkdir -p $out
    # CRDs come from infra/helm/crds/ (not the chart — Helm's crds/ dir
    # is install-only, wrong for a dev-phase project). Concatenate with
    # explicit --- separators: the generated CRD yamls don't end with
    # document separators, so a bare `cat` produces one malformed
    # document — k3s silently applies only part (or none) of it.
    for f in ${crds}/*.yaml; do
      cat "$f"
      echo "---"
    done > $out/00-crds.yaml
    # yq-go: filter by kind. k3s applies in filename order — CRDs must
    # establish before the controller pod tries to watch them; Namespace
    # must exist before ServiceAccounts can be created in it; RBAC must
    # bind before the pod's SA token is validated against it.
    yq 'select(.kind == "Namespace" or
               .kind == "ServiceAccount" or
               .kind == "ClusterRole" or
               .kind == "ClusterRoleBinding" or
               .kind == "Role" or
               .kind == "RoleBinding")' all.yaml > $out/01-rbac.yaml
    yq 'select(.kind != "Namespace" and
               .kind != "ServiceAccount" and
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
