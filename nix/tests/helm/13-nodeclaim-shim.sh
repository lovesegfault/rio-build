# ADR-023 §13b: builder NodePools are a single `rio-nodeclaim-shim`
# (limits.cpu: "0", budgets[0].nodes: "0") that Karpenter sees but never
# provisions from — rio-controller creates NodeClaims directly. §13c:
# pre-§13c static metal NodePool deleted — kvm builds are `metal-*` hwClasses.
# §13e: static fetcher NodePool deleted — FODs are `fetcher-*` hwClasses.
# The only remaining static NodePool is `rio-general` (control-plane).

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
# r39: capture pools_of output before grep -q. Same SIGPIPE shape as
# the yq | grep -q pipes swept in 12-priorityclass.sh — `grep -q`
# exits at first match → `sort` (the function's last stage) SIGPIPE
# (141) → pipefail flags the pipeline. The here-string avoids the pipe.
all_pools=$(pools_of "$on")
for p in rio-general; do
  grep -qx "$p" <<<"$all_pools" || {
    echo "FAIL: dropped NodePool $p" >&2
    exit 1
  }
done
# §13e: rio-fetcher must NOT render — fetcher capacity is now NodeClaim-
# managed via `fetcher-*` hwClasses. If it reappears, the static
# NodePool's `limits:{cpu:5000}` and the per-class `maxFleetCores`
# DOUBLE the fetcher fanout budget.
if grep -qx rio-fetcher <<<"$all_pools"; then
  echo "FAIL: static rio-fetcher NodePool re-rendered (deleted in §13e)" >&2
  exit 1
fi

# §13c: rendered scheduler.toml has metal hwClasses with nodeClass
# rio-metal, providesFeatures=[kvm], capacityTypes=[spot, on-demand]
# (M1, owner-signed: metal joined the spot+od doctrine; the od-only
# carve-out died with the bughunt-9 wave — see
# 43-metal-capacity-doctrine.sh for the doctrine pin).
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
  # §13d (r30 bug_007): nixos-test added — `requiredSystemFeatures =
  # ["nixos-test", "kvm"]` is the standard nixpkgs `nixosTest` set.
  # 18-metal-feature-routing.sh asserts the full superset; this is the
  # rendering shape check (a JSON array literal).
  echo "$block" | grep -q 'provides_features = \[.*"kvm".*\]' || {
    echo "FAIL: $h missing provides_features ⊇ [kvm]" >&2; exit 1; }
  echo "$block" | grep -q 'capacity_types = \["spot","on-demand"\]' || {
    echo "FAIL: $h missing capacity_types=[spot, on-demand] (M1)" >&2; exit 1; }
  echo "$block" | grep -q 'taints = \[' || {
    echo "FAIL: $h missing taints" >&2; exit 1; }
  echo "$block" | grep -q '"rio.build/kvm"' || {
    echo "FAIL: $h missing rio.build/kvm taint" >&2; exit 1; }
done

# §13c D4a + §13e fleet-budget invariant. Each fold of a static
# NodePool into NodeClaim-managed hwClasses adds that pool's
# `limits.cpu` to the shared `sla.maxFleetCores`; the per-class
# `maxFleetCores` caps mean the new classes can't crowd out the rest.
#   §13c: metal `limits:{cpu:10000}` → bump 10000, cap Σ(metal) ≤ 10000.
#   §13e: fetcher `limits:{cpu:5000}` → fetcher hwClasses cap at 5000
#         each (Σ=10000) → bump 10000, cap Σ(fetcher) ≤ 10000.
# Combined: Σ(metal) + Σ(fetcher) ≤ maxFleetCores − 10000, where the
# 10000 RHS slack is the pre-§13c floor for non-metal non-fetcher
# (general builder) classes.
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
fetcher_sum=$(printf '%s\n' "$sched_toml" | awk '
  /^\[sla\.hw_classes\./ { h=$0; sub(/.*"/,"",h); sub(/".*/,"",h); is_fetcher=0 }
  h && /^provides_features = \[.*"fetcher".*\]/ { is_fetcher=1 }
  is_fetcher && /^max_fleet_cores = / { sum+=$3 }
  END { print sum+0 }
')
test "$metal_sum" -le 10000 || {
  echo "FAIL: Σ metal max_fleet_cores ($metal_sum) > 10000 (D4a cap)" >&2
  exit 1
}
test "$fetcher_sum" -le 10000 || {
  echo "FAIL: Σ fetcher max_fleet_cores ($fetcher_sum) > 10000 (§13e cap)" >&2
  exit 1
}
floor=$((global_fc - 10000))
test "$((metal_sum + fetcher_sum))" -le "$floor" || {
  echo "FAIL: Σ(metal)+Σ(fetcher) ($metal_sum+$fetcher_sum=$((metal_sum + fetcher_sum))) > maxFleetCores−10000 ($floor)" >&2
  echo "  general-builder classes would lose their pre-§13c 10000 floor" >&2
  exit 1
}
test "$metal_sum" -gt 0 || {
  echo "FAIL: Σ metal max_fleet_cores is 0 — metal hwClasses missing maxFleetCores" >&2
  exit 1
}
test "$fetcher_sum" -gt 0 || {
  echo "FAIL: Σ fetcher max_fleet_cores is 0 — fetcher hwClasses missing maxFleetCores" >&2
  exit 1
}
