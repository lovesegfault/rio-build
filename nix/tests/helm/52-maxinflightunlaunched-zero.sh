# sh-043: Sprig `default` treats integer 0 as empty — an operator
# setting `karpenter.nodeclaimPool.maxInflightUnlaunched: 0` as an
# emergency mint-halt would have rendered `= 50`. The chart-level
# default in values.yaml covers nil; the template MUST NOT shadow it
# (the 47-template-default-ban single-default convention). Explicit 0
# is the meaningful kill-switch the law's own doc treats as halt.

render_toml() {
  helm template rio . \
    --set karpenter.enabled=true \
    --set karpenter.clusterName=ci \
    --set karpenter.nodeRoleName=ci-role \
    --set karpenter.amiTag=test \
    --set global.image.tag=test \
    --set postgresql.enabled=false \
    "$@" \
    | yq -N 'select(.kind=="ConfigMap" and .metadata.name=="rio-controller-config")
             | .data."controller.toml"'
}

# `|| true` inside the pipelines: grep's no-match exit must reach the
# DEDICATED failure message below, not die silently in a `set -e`
# command substitution (the stdenv-pipefail trap; same guard as 39).
extract() { { grep -E '^max_inflight_unlaunched = ' || true; } | grep -oE '[0-9]+' || true; }

# Explicit 0 renders 0 (NOT the Sprig-swallowed 50).
got=$(render_toml --set karpenter.nodeclaimPool.maxInflightUnlaunched=0 | extract)
test "$got" = "0" || {
  echo "FAIL: maxInflightUnlaunched=0 rendered max_inflight_unlaunched=$got, want 0" >&2
  echo "  (Sprig 'default N' treats integer 0 as empty — the operator's mint-halt" >&2
  echo "   kill-switch was silently swallowed; sh-043-r1)" >&2
  exit 1
}

# Unset renders the values.yaml default (50).
got=$(render_toml | extract)
test "$got" = "50" || {
  echo "FAIL: unset maxInflightUnlaunched rendered $got, want values.yaml default 50" >&2
  exit 1
}
