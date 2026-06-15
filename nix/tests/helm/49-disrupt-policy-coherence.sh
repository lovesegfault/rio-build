# bug_015 (the §SCC(2)/(3) cross-tier sweep miss): commit 2329652
# stamped every store pod `karpenter.sh/do-not-disrupt: "true"` while
# the rio-store NodePool kept `WhenEmptyOrUnderutilized` — Underutilized
# was structurally dead (one do-not-disrupt pod per node, tainted
# pool, required anti-affinity). The chokepoint quantifies the
# RELATIONSHIP instead of pinning a value: every NodePool whose
# consolidationPolicy is NOT WhenEmpty must host at least one targeting
# Deployment/StatefulSet whose pod template is disruptable (no
# `do-not-disrupt: "true"` annotation), or the non-WhenEmpty half of
# the policy cannot fire and the values.yaml/prose claims it makes
# are false on render. The join key is `rio.build/node-role` (the one
# label every static pool stamps and every workload nodeSelector
# names).
#
# Pre-fix RED: rio-store declares WhenEmptyOrUnderutilized; its only
# targeting Deployment carries do-not-disrupt: "true".

out=$TMPDIR/disrupt-coherence.yaml
helm template rio . \
  --set karpenter.enabled=true \
  --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role \
  --set karpenter.amiTag=test \
  --set global.image.tag=test >"$out"

fail=0
while IFS=$'\t' read -r pool role; do
  # Guard: a pool without a node-role label has no workload join key —
  # the relationship is undefined for it (the dynamic builder/fetcher
  # claims live outside this static-pool census).
  [ -n "$role" ] || continue
  disruptable=$(yq -N '
    select(.kind=="Deployment" or .kind=="StatefulSet")
    | select(.spec.template.spec.nodeSelector."rio.build/node-role" == "'"$role"'")
    | select((.spec.template.metadata.annotations."karpenter.sh/do-not-disrupt" // "false") != "true")
    | .metadata.name' "$out")
  if [ -z "$disruptable" ]; then
    echo "FAIL: NodePool '$pool' (rio.build/node-role=$role) declares a non-WhenEmpty" >&2
    echo "      consolidationPolicy, but every targeting workload carries" >&2
    echo "      karpenter.sh/do-not-disrupt: \"true\" — Underutilized can never fire" >&2
    echo "      (bug_015). Either set consolidationPolicy: WhenEmpty for the pool," >&2
    echo "      or drop the do-not-disrupt annotation from a workload that targets it." >&2
    fail=1
  fi
done < <(yq -N '
  select(.kind=="NodePool")
  | select(.spec.disruption.consolidationPolicy != "WhenEmpty")
  | [.metadata.name, (.spec.template.metadata.labels."rio.build/node-role" // "")]
  | @tsv' "$out")

[ "$fail" -eq 0 ] || exit 1
echo "OK: every non-WhenEmpty NodePool hosts ≥1 disruptable workload (or quantifies vacuously)"
