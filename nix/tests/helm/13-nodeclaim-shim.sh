# ADR-023 §13b: builder NodePools are a single `rio-nodeclaim-shim`
# (limits.cpu: "0", budgets[0].nodes: "0") that Karpenter sees but never
# provisions from — rio-controller creates NodeClaims directly. §13c:
# pre-§13c static metal NodePool deleted — kvm builds are `metal-*` hwClasses.
# The remaining `nodePools` list (rio-fetcher / rio-general) is
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
n=$(pools_of "$on" | grep -Ec '^rio-builder-' || true)
test "$n" -eq 0 || {
  echo "FAIL: rendered $n rio-builder-* NodePools (deleted in §13b/§13c):" >&2
  pools_of "$on" | grep -E '^rio-builder-' >&2
  exit 1
}
for p in rio-fetcher rio-general; do
  pools_of "$on" | grep -qx "$p" || {
    echo "FAIL: dropped NodePool $p" >&2
    exit 1
  }
done

# §13c: rendered scheduler.toml has metal hwClasses with nodeClass
# rio-metal, providesFeatures=[kvm], capacityTypes=[on-demand].
sched_toml=$(yq -N 'select(.kind=="ConfigMap" and .metadata.name=="rio-scheduler-config")
                    | .data."scheduler.toml"' "$on")
for h in metal-x86 metal-arm; do
  block=$(printf '%s\n' "$sched_toml" | awk -v h="$h" '
    $0 == "[sla.hw_classes.\"" h "\"]" { in_h=1; next }
    in_h && /^\[/ { exit }
    in_h { print }
  ')
  test -n "$block" || { echo "FAIL: scheduler.toml missing hw_classes.$h" >&2; exit 1; }
  echo "$block" | grep -q 'node_class = "rio-metal"' || {
    echo "FAIL: $h node_class != rio-metal" >&2; exit 1; }
  echo "$block" | grep -q 'provides_features = \["kvm"\]' || {
    echo "FAIL: $h missing provides_features=[kvm]" >&2; exit 1; }
  echo "$block" | grep -q 'capacity_types = \["on-demand"\]' || {
    echo "FAIL: $h missing capacity_types=[on-demand]" >&2; exit 1; }
  echo "$block" | grep -q 'taints = \[' || {
    echo "FAIL: $h missing taints" >&2; exit 1; }
  echo "$block" | grep -q '"rio.build/kvm"' || {
    echo "FAIL: $h missing rio.build/kvm taint" >&2; exit 1; }
done

# §13c D4a invariant: Σ hwClasses[nodeClass==rio-metal].max_fleet_cores
# ≤ (sla.maxFleetCores − 10000). The pre-§13c static metal NodePool's
# `limits:{cpu:10000}` was a SEPARATE budget; folding metal in adds its
# capacity to the global pool. Bump global by 10000 and cap Σ(metal) at
# 10000 so non-metal keeps its pre-§13c floor.
global_fc=$(printf '%s\n' "$sched_toml" | awk '
  /^\[sla\]/ { in_sla=1 }
  /^\[sla\./ { in_sla=0 }
  in_sla && /^max_fleet_cores = / { print $3; exit }
')
metal_sum=$(printf '%s\n' "$sched_toml" | awk '
  /^\[sla\.hw_classes\./ { h=$0; sub(/.*"/,"",h); sub(/".*/,"",h); is_metal=0 }
  h && /^node_class = "rio-metal"/ { is_metal=1 }
  is_metal && /^max_fleet_cores = / { sum+=$3 }
  END { print sum+0 }
')
floor=$((global_fc - 10000))
test "$metal_sum" -le "$floor" || {
  echo "FAIL: Σ metal max_fleet_cores ($metal_sum) > maxFleetCores−10000 ($floor)" >&2
  echo "  D4a: non-metal would lose its pre-§13c 10000 floor" >&2
  exit 1
}
test "$metal_sum" -gt 0 || {
  echo "FAIL: Σ metal max_fleet_cores is 0 — metal hwClasses missing maxFleetCores" >&2
  exit 1
}
