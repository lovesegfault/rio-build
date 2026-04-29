# ADR-023 §13b: builder NodePools are a single `rio-nodeclaim-shim`
# (limits.cpu: "0", budgets[0].nodes: "0") that Karpenter sees but never
# provisions from — rio-controller creates NodeClaims directly. The
# `nodePools` list (rio-fetcher / rio-general / rio-builder-metal) is
# unaffected.

karp_args=(
  --set karpenter.enabled=true
  --set karpenter.clusterName=ci
  --set karpenter.nodeRoleName=ci-role
  --set karpenter.amiTag=test
  --set global.image.tag=test
  --set postgresql.enabled=false
)

pools_of() { yq -N 'select(.kind=="NodePool") | .metadata.name' "$1" | sort; }

on=$TMPDIR/shim-on.yaml
helm template rio . "${karp_args[@]}" >"$on"

test "$(yq -N 'select(.kind=="NodePool" and .metadata.name=="rio-nodeclaim-shim")
               | .spec.limits.cpu' "$on")" = 0 || {
  echo "FAIL: rio-nodeclaim-shim missing or limits.cpu != \"0\"" >&2
  exit 1
}
test "$(yq -N 'select(.kind=="NodePool" and .metadata.name=="rio-nodeclaim-shim")
               | .spec.disruption.budgets[0].nodes' "$on")" = 0 || {
  echo "FAIL: rio-nodeclaim-shim disruption.budgets[0].nodes != \"0\"" >&2
  exit 1
}
n=$(pools_of "$on" | grep -Ec '^rio-builder-(hi|mid|lo)-' || true)
test "$n" -eq 0 || {
  echo "FAIL: rendered $n band×storage×arch pools (band-loop should be deleted):" >&2
  pools_of "$on" | grep -E '^rio-builder-(hi|mid|lo)-' >&2
  exit 1
}
for p in rio-fetcher rio-general rio-builder-metal; do
  pools_of "$on" | grep -qx "$p" || {
    echo "FAIL: dropped NodePool $p" >&2
    exit 1
  }
done
