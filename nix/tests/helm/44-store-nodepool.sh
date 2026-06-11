# D1 (bughunt-9 W9-BS): the store fleet's own NodePool. Store pods
# previously rode rio-general (Drifted-0 budget + the template's
# blanket WhenEmpty policy — a BUILDER rationale that never applied to
# the store), so a 3-millicore store pod pinned a whole 32-vCPU node
# indefinitely. Both faces pinned here:
#   (i)  rio-store NodePool renders with STORE-plane reclaim
#        (WhenEmptyOrUnderutilized + the rio.build/store taint) and
#        the store Deployment targets it (selector + toleration);
#   (ii) the builder-default face survives: rio-general (and every
#        other pool entry) keeps WhenEmpty — the per-pool override
#        must not flip the default.
# Plus the k3s face: with karpenter disabled (chart default) the store
# Deployment carries NO nodeSelector — VM tests stay schedulable.
#
# Pre-fix RED (the shipped truth): no rio-store NodePool rendered.

out=$TMPDIR/store-nodepool.yaml
helm template rio . \
  --set karpenter.enabled=true \
  --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role \
  --set karpenter.amiTag=test \
  --set global.image.tag=test >"$out"

pool=$(yq -N 'select(.kind=="NodePool" and .metadata.name=="rio-store")' "$out")
test -n "$pool" || {
  echo "FAIL: rio-store NodePool did not render — the store fleet still squats rio-general (D1)" >&2
  exit 1
}
policy=$(yq -N 'select(.kind=="NodePool" and .metadata.name=="rio-store") | .spec.disruption.consolidationPolicy' "$out")
test "$policy" = "WhenEmptyOrUnderutilized" || {
  echo "FAIL: rio-store consolidationPolicy is '$policy', want WhenEmptyOrUnderutilized — store reclaim must be STORE-plane policy, not the builder default" >&2
  exit 1
}
taint=$(yq -N 'select(.kind=="NodePool" and .metadata.name=="rio-store") | .spec.template.spec.taints[0].key' "$out")
test "$taint" = "rio.build/store" || {
  echo "FAIL: rio-store pool missing the rio.build/store taint (got: $taint)" >&2
  exit 1
}
cap=$(yq -N 'select(.kind=="NodePool" and .metadata.name=="rio-store") | .spec.template.spec.requirements[] | select(.key=="karpenter.sh/capacity-type") | .values[0]' "$out")
test "$cap" = "on-demand" || {
  echo "FAIL: rio-store pool capacity-type is '$cap', want on-demand (the signed doctrine's store exception: od-only-by-nodepool)" >&2
  exit 1
}

gen_policy=$(yq -N 'select(.kind=="NodePool" and .metadata.name=="rio-general") | .spec.disruption.consolidationPolicy' "$out")
test "$gen_policy" = "WhenEmpty" || {
  echo "FAIL: rio-general consolidationPolicy drifted to '$gen_policy' — the per-pool override must not flip the WhenEmpty default" >&2
  exit 1
}

sel=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store") | .spec.template.spec.nodeSelector."rio.build/node-role"' "$out")
test "$sel" = "store" || {
  echo "FAIL: store Deployment does not target the store pool (nodeSelector rio.build/node-role: '$sel')" >&2
  exit 1
}
tol=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store") | .spec.template.spec.tolerations[0].key' "$out")
test "$tol" = "rio.build/store" || {
  echo "FAIL: store Deployment missing the rio.build/store toleration (got: $tol)" >&2
  exit 1
}

# The k3s face: karpenter disabled (default) => NO nodeSelector.
out2=$TMPDIR/store-nodepool-default.yaml
helm template rio . --set global.image.tag=test >"$out2"
sel2=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store") | .spec.template.spec.nodeSelector' "$out2")
test "$sel2" = "null" || {
  echo "FAIL: with karpenter disabled the store Deployment must carry NO nodeSelector (k3s schedulability), got: $sel2" >&2
  exit 1
}

echo "OK: rio-store pool (WhenEmptyOrUnderutilized, tainted, od) + targeted Deployment; rio-general keeps WhenEmpty; k3s face clean"
