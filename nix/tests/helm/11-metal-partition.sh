# I-205 metal/virtualized partition is structural: every static NodePool
# (UEFI NodeClass: rio-default / rio-nvme) MUST carry an
# `instance-size NotIn metalSizes` requirement. §13c: there is no static
# metal NodePool — `metal-*` hwClasses get the `In` side from §13b
# `cover::build_nodeclaim`, which reads `controller.toml [nodeclaim_pool]
# metal_sizes`. Single source is `.Values.karpenter.metalSizes`; both the
# template-side NotIn AND the controller-side metal_sizes are injected
# from it. This check asserts both stay in lockstep.

karp=$TMPDIR/karp-metal.yaml
helm template rio . \
  --set karpenter.enabled=true \
  --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role \
  --set karpenter.amiTag=test \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$karp"

# live_050(d): metal-96xl joined the partition list — its omission let
# 96xl metal variants leak through the band classes' NotIn partition
# into their ceiling argmax. This literal mirrors karpenter.metalSizes
# (sorted); the scheduler-side census
# (config.rs::shipped_metal_sizes_cover_the_catalog_enumeration) pins
# the values list against the catalog's own metal-size enumeration.
want='["metal","metal-16xl","metal-24xl","metal-32xl","metal-48xl","metal-96xl"]'

# instance-size NotIn values for one pool, as a sorted JSON array (or []).
notin_of() {
  yq -N "select(.kind==\"NodePool\" and .metadata.name==\"$1\")
         | .spec.template.spec.requirements[]
         | select(.key==\"karpenter.k8s.aws/instance-size\" and .operator==\"NotIn\")
         | .values" -o=json "$karp" | jq -sc 'add // [] | sort'
}

# Every static NodePool carries NotIn ⊇ want (UEFI partition guard).
# §13c: there should be NO NodePool referencing rio-metal anymore.
fail=0
metal_pools=$(yq -N 'select(.kind=="NodePool"
                            and .spec.template.spec.nodeClassRef.name=="rio-metal")
                     | .metadata.name' "$karp")
if [ -n "$metal_pools" ]; then
  echo "FAIL: NodePool(s) still reference rio-metal NodeClass (should be §13b-only): $metal_pools" >&2
  fail=1
fi
while read -r pool; do
  got=$(notin_of "$pool")
  for sz in metal metal-16xl metal-24xl metal-32xl metal-48xl; do
    jq -e --arg s "$sz" 'index($s) != null' >/dev/null <<<"$got" || {
      echo "FAIL: NodePool $pool instance-size NotIn missing '$sz' (got: $got)" >&2
      fail=1
    }
  done
done < <(yq -N 'select(.kind=="NodePool") | .metadata.name' "$karp")
[ "$fail" = 0 ] || exit 1

# §13c In-side: controller.toml [nodeclaim_pool] metal_sizes ==
# .Values.karpenter.metalSizes. The §13b `cover::build_nodeclaim` reads
# this for the `instance-size In/NotIn` partition; if it drifts from the
# NodePool NotIn list, a metal NodeClaim could resolve to a UEFI-pool
# instance size or vice versa.
ctrl_toml=$(yq -N 'select(.kind=="ConfigMap" and .metadata.name=="rio-controller-config")
                   | .data."controller.toml"' "$karp")
ctrl_metal=$(printf '%s\n' "$ctrl_toml" \
  | grep '^metal_sizes' | sed 's/^metal_sizes = //' | jq -sc 'add // [] | sort')
test "$ctrl_metal" = "$want" || {
  echo "FAIL: controller.toml metal_sizes ($ctrl_metal) != karpenter.metalSizes ($want)" >&2
  exit 1
}
# §13e: rio-fetcher NodePool deleted; rio-general is the only static
# NodePool left to spot-check the lockstep between
# `.Values.karpenter.metalSizes` (template-side NotIn) and
# `controller.toml metal_sizes` (controller-side In/NotIn).
uefi_notin=$(notin_of rio-general)
test "$ctrl_metal" = "$uefi_notin" || {
  echo "FAIL: controller.toml metal_sizes ($ctrl_metal) != rio-general NotIn ($uefi_notin) — partition not single-sourced" >&2
  exit 1
}
