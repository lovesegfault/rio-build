# I-205 metal/virtualized partition is structural: every NodePool that
# resolves to a UEFI NodeClass (rio-default / rio-nvme) MUST carry an
# `instance-size NotIn metalSizes` requirement, and rio-builder-metal's
# `instance-size In` MUST be the SAME list. Single source is
# `.Values.karpenter.metalSizes`; both sides are template-injected.

karp=$TMPDIR/karp-metal.yaml
helm template rio . \
  --set karpenter.enabled=true \
  --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role \
  --set karpenter.amiTag=test \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$karp"

want='["metal","metal-16xl","metal-24xl","metal-32xl","metal-48xl"]'

# instance-size NotIn values for one pool, as a sorted JSON array (or []).
notin_of() {
  yq -N "select(.kind==\"NodePool\" and .metadata.name==\"$1\")
         | .spec.template.spec.requirements[]
         | select(.key==\"karpenter.k8s.aws/instance-size\" and .operator==\"NotIn\")
         | .values" -o=json "$karp" | jq -sc 'add // [] | sort'
}

# Every UEFI-NodeClass pool carries NotIn ⊇ want.
fail=0
while read -r pool; do
  got=$(notin_of "$pool")
  for sz in metal metal-16xl metal-24xl metal-32xl metal-48xl; do
    jq -e --arg s "$sz" 'index($s) != null' >/dev/null <<<"$got" || {
      echo "FAIL: NodePool $pool instance-size NotIn missing '$sz' (got: $got)" >&2
      fail=1
    }
  done
done < <(yq -N 'select(.kind=="NodePool"
                       and (.spec.template.spec.nodeClassRef.name=="rio-default"
                            or .spec.template.spec.nodeClassRef.name=="rio-nvme"))
                | .metadata.name' "$karp")
[ "$fail" = 0 ] || exit 1

# rio-builder-metal's In == any uefi pool's NotIn (same list, partition).
metal_in=$(yq -N 'select(.kind=="NodePool" and .metadata.name=="rio-builder-metal")
                  | .spec.template.spec.requirements[]
                  | select(.key=="karpenter.k8s.aws/instance-size" and .operator=="In")
                  | .values' -o=json "$karp" | jq -sc 'add // [] | sort')
uefi_notin=$(notin_of rio-fetcher)
test "$metal_in" = "$uefi_notin" || {
  echo "FAIL: rio-builder-metal In ($metal_in) != rio-fetcher NotIn ($uefi_notin) — partition not single-sourced" >&2
  exit 1
}
test "$metal_in" = "$want" || {
  echo "FAIL: rio-builder-metal In ($metal_in) != $want" >&2
  exit 1
}
