# r[infra.node.nixos-ami] full cutover.
#
# karpenter.enabled=true MUST render the rio-default EC2NodeClass with
# amiFamily: AL2023, the rio.build/ami tag selector, and NO userData
# (sysctls/seccomp/device-plugin are baked into the AMI —
# nix/nixos-node/).

karp=$TMPDIR/karp-on.yaml
helm template rio . \
  --set karpenter.enabled=true \
  --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role \
  --set karpenter.amiTag=test \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$karp"

test "$(yq 'select(.kind=="EC2NodeClass" and .metadata.name=="rio-default")
            | .spec.amiFamily' "$karp")" = AL2023 || {
  echo "FAIL: rio-default EC2NodeClass amiFamily != AL2023" >&2
  exit 1
}
test "$(yq 'select(.kind=="EC2NodeClass" and .metadata.name=="rio-default")
            | .spec.amiSelectorTerms[0].tags."rio.build/ami"' "$karp")" = test || {
  echo "FAIL: rio-default amiSelectorTerms[0] missing rio.build/ami=test tag" >&2
  exit 1
}
test "$(yq 'select(.kind=="EC2NodeClass" and .metadata.name=="rio-default")
            | .spec.userData' "$karp")" = null || {
  echo "FAIL: rio-default EC2NodeClass renders userData (should be baked into AMI)" >&2
  exit 1
}

# No Bottlerocket functionality in the karpenter render — the cutover
# deletes the fallback entirely. Check for functional markers (amiAlias
# key, Bottlerocket TOML settings sections, the canary NodeClass), not
# the bare word — comments mention it in past tense.
for pat in 'amiAlias:' '\[settings\.' 'name: rio-nixos$' \
  'rio-builder-nixos-canary' 'amiFamily: Bottlerocket'; do
  if grep -Eq "$pat" "$karp"; then
    echo "FAIL: Bottlerocket-era pattern '$pat' present in karpenter render" >&2
    grep -En "$pat" "$karp" >&2
    exit 1
  fi
done

# device-plugin.yaml is GONE — both EKS and k3s now inject
# /dev/{fuse,kvm} via containerd base_runtime_spec
# (nix/base-runtime-spec.nix). Guard against it returning.
if [ -e templates/device-plugin.yaml ]; then
  echo "FAIL: templates/device-plugin.yaml resurrected — both paths use base_runtime_spec now" >&2
  exit 1
fi
default=$TMPDIR/karp-default.yaml
helm template rio . --set global.image.tag=test >"$default"
! yq 'select(.kind=="DaemonSet") | .metadata.name' "$karp" "$default" |
  grep -x rio-device-plugin >/dev/null || {
  echo "FAIL: rio-device-plugin DaemonSet rendered (both paths use base_runtime_spec)" >&2
  exit 1
}

# NodeOverlay is GONE — its capacity is simulation-only (Karpenter docs
# explicit) so nothing wrote Node.status.capacity and kube-scheduler
# couldn't bind pods requesting it. Guard against it returning.
! yq 'select(.kind=="NodeOverlay") | .metadata.name' "$karp" | grep -q . || {
  echo "FAIL: NodeOverlay rendered — rio.build/{fuse,kvm} extended resource was dropped" >&2
  exit 1
}
