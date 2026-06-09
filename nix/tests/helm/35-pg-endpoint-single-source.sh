# In-cluster postgres endpoint placement — ONE namespace fact (bug_185).
#
# The bitnami subchart deploys into the helm RELEASE namespace (system;
# no namespaceOverride is set), so every egress allow that targets the
# in-cluster PG pod must match that namespace. Pre-fix the controller's
# open-coded copy matched the STORE namespace — zero endpoints, so under
# Cilium default-deny the controller's direct PG edge
# (nodeclaim_cell_state sketches, load-bearing at every leadership
# acquire) silently dropped, while the store's sibling rule was correct.
# The rule is now single-sourced through the rio.pgInClusterEgress
# helper; this fragment asserts the agreement LAW over the rendered
# output, so a future open-coded copy that drifts fails here whether or
# not it uses the helper.
#
# Self-test (no fail-open enforcement): the checker is first proven RED
# against a private chart copy with the defect re-planted at the helper,
# then run for real. A checker that cannot fail its planted fixture does
# not gate.
#
# (documentary — .sh is not tracey-scanned.)

root=$PWD
sysns=$(yq '.namespaces.system.name' values.yaml)
test -n "$sysns" -a "$sysns" != "null" || {
  echo "FAIL: cannot read namespaces.system.name from values.yaml" >&2
  exit 1
}

# The check, as a function: list every PG toEndpoints rule whose
# namespace label is NOT the system namespace.
pg_ns_violations() {
  yq -N 'select(.kind=="CiliumNetworkPolicy") | .metadata.name as $n
         | .spec.egress[]?.toEndpoints[]?.matchLabels
         | select(."k8s:app.kubernetes.io/name"=="postgresql")
         | select(."k8s:io.kubernetes.pod.namespace" != "'"$sysns"'")
         | $n + " -> ns=" + ."k8s:io.kubernetes.pod.namespace"' "$1"
}

# ── Planted RED: re-introduce the bug_185 defect at the helper in a
# private copy; the checker MUST report the controller rule.
priv=$TMPDIR/chart-185-red
rm -rf "$priv"
cp -r . "$priv"
sed -i 's/k8s:io.kubernetes.pod.namespace: {{ .Values.namespaces.system.name }}/k8s:io.kubernetes.pod.namespace: {{ .Values.namespaces.store.name }}/' \
  "$priv/templates/_helpers.tpl"
grep -q '{{ .Values.namespaces.store.name }}' "$priv/templates/_helpers.tpl" || {
  echo "FAIL: planted-red sed did not take — helper shape changed; update this fixture" >&2
  exit 1
}
cd "$priv"
out=$TMPDIR/pg-endpoint-red.yaml
helm template rio . --set global.image.tag=test --set postgresql.enabled=true >"$out"
red=$(pg_ns_violations "$out")
test -n "$red" || {
  echo "FAIL: planted wrong-namespace PG rule was NOT caught — the checker is fail-open" >&2
  exit 1
}
cd "$root"

# ── Real chart: zero violations, and BOTH known consumers carry the
# rule (guards against 'fixed' by deleting the controller's edge).
out=$TMPDIR/pg-endpoint.yaml
helm template rio . --set global.image.tag=test --set postgresql.enabled=true >"$out"
bad=$(pg_ns_violations "$out")
test -z "$bad" || {
  echo "FAIL: in-cluster PG egress rule(s) disagree with the subchart's namespace ($sysns):" >&2
  echo "$bad" >&2
  echo "      single-source via rio.pgInClusterEgress — the namespace fact lives there" >&2
  exit 1
}
for cnp in rio-controller-egress store-egress; do
  got=$(yq -N 'select(.metadata.name=="'"$cnp"'")
               | .spec.egress[]?.toEndpoints[]?.matchLabels
               | select(."k8s:app.kubernetes.io/name"=="postgresql")
               | ."k8s:io.kubernetes.pod.namespace"' "$out")
  test "$got" = "$sysns" || {
    echo "FAIL: $cnp lacks an in-cluster PG egress rule in $sysns (got '$got')" >&2
    exit 1
  }
done

# ── bug_063/bug_171: BOTH PG-egress optionality decisions live INSIDE
# the shared helpers — the agreement law is asserted over renders, so
# a future open-coded copy that re-forks a guard fails here.
#
# (a) blanked external CIDR renders ZERO external rules and no invalid
#     empty CIDR (bug_063 pre-fix: the store emitted {cidr: ""}, which
#     fails Cilium CRD validation — failed upgrade, or store left
#     under default-deny).
out=$TMPDIR/pg-blank-cidr.yaml
helm template rio . --set global.image.tag=test --set global.postgresCidr="" >"$out"
if grep -E '^[[:space:]]*-?[[:space:]]*\{?cidr: ""' "$out" | grep -qv '#'; then
  echo "FAIL: blanked global.postgresCidr renders an invalid empty CIDR (bug_063):" >&2
  grep -nE '^[[:space:]]*-?[[:space:]]*\{?cidr: ""' "$out" | grep -v '#' | head >&2
  exit 1
fi
# (b) postgresql disabled renders ZERO in-cluster PG endpoint rules in
#     EVERY consumer (bug_171 pre-fix: only the controller call site
#     was guarded; the helper doc claimed both were).
out=$TMPDIR/pg-disabled.yaml
helm template rio . --set global.image.tag=test --set postgresql.enabled=false >"$out"
# NB: count-then-filter — a trailing bare `$n` in yq v4 emits once per
# DOCUMENT even when the preceding pipe matched nothing (observed:
# ingress-only policies listed as violations); the array form below
# only names policies whose real match count is > 0.
stray=$(yq -N 'select(.kind=="CiliumNetworkPolicy")
       | [.metadata.name, ([.spec.egress[]? | .toEndpoints[]? | .matchLabels."k8s:app.kubernetes.io/name" // ""] | map(select(. == "postgresql")) | length)]
       | select(.[1] > 0) | .[0]' "$out")
test -z "$stray" || {
  echo "FAIL: postgresql.enabled=false still renders in-cluster PG egress (bug_171):" >&2
  echo "$stray" >&2
  exit 1
}
# (c) the external-PG rule renders IDENTICALLY in both consumers when
#     the CIDR is set (single-source proof — the rule body has exactly
#     one producer).
out=$TMPDIR/pg-external.yaml
helm template rio . --set global.image.tag=test >"$out"
# JSON output: comment-free and key-normalized — yq's YAML output
# round-trips call-site comments onto the extracted node, which would
# make byte-equality depend on prose.
ctl=$(yq -N -o=json 'select(.metadata.name=="rio-controller-egress") | .spec.egress[]? | select(.toCIDRSet != null)' "$out")
sto=$(yq -N -o=json 'select(.metadata.name=="store-egress") | .spec.egress[]? | select(.toCIDRSet != null)' "$out")
test -n "$ctl" || {
  echo "FAIL: controller renders no external-PG toCIDRSet rule (single-source regression)" >&2
  exit 1
}
test "$ctl" = "$sto" || {
  echo "FAIL: external-PG egress differs between controller and store (the rule must have ONE producer):" >&2
  printf 'controller:\n%s\nstore:\n%s\n' "$ctl" "$sto" >&2
  exit 1
}
