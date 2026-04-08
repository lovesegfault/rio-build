# Shared shell-snippet builders for splitting helm-rendered manifests
# into k3s auto-apply ordering. Used by helm-render.nix and
# envoy-gateway-render.nix.
{ lib }:
rec {
  # RBAC-ish kinds that must apply before workloads. Single source of
  # truth — adding e.g. ValidatingWebhookConfiguration goes HERE, not
  # in N render files.
  rbacKinds = [
    "Namespace"
    "ServiceAccount"
    "ClusterRole"
    "ClusterRoleBinding"
    "Role"
    "RoleBinding"
  ];

  # yq predicate: `.kind == "A" or .kind == "B" or ...`
  kindIs = kinds: lib.concatMapStringsSep " or " (k: ".kind == \"${k}\"") kinds;
  # yq predicate: `.kind != "A" and .kind != "B" and ...`
  kindIsNot = kinds: lib.concatMapStringsSep " and " (k: ".kind != \"${k}\"") kinds;

  # Shell snippet: concat YAML files with explicit `---` separators.
  # Generated CRD yamls don't end with document separators, so a bare
  # `cat` produces one malformed document — k3s silently applies only
  # part (or none) of it.
  concatYamlDocs = glob: dest: ''
    for f in ${glob}; do
      cat "$f"
      echo "---"
    done > ${dest}
  '';

  # Shell snippet: assert every *.yaml in `dir` is non-empty (empty
  # file = k3s silently skips it, test passes wrongly).
  assertNonEmpty = dir: ''
    for f in ${dir}/*.yaml; do
      test -s "$f" || { echo "rendered file $f is empty" >&2; exit 1; }
    done
  '';
}
