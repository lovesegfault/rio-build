# P0564: karpenter-enabled (= NixOS-AMI EKS) renders must carry the
# operator's declaration that the node AMI kernel provides
# FUSE_PASSTHROUGH (kernel >= 6.9), or the chart refuses to render.
#
# This is a DECLARATION guard, not a kernel probe: the AMI build itself
# is asserted by the node-kernel-config check (nix/misc-checks.nix
# builds the AMI kernel's .config and greps CONFIG_FUSE_PASSTHROUGH=y);
# the chart can only check what the operator declared about the AMI it
# points karpenter at. Without the feature every castore open() on
# every builder node silently degrades to userspace read round-trips.

# (i) values.yaml ships the declaration populated — it describes the
# only in-tree AMI path (`xtask ami push` → nix/nixos-node/kernel.nix).
# Someone emptying the default turns every EKS deploy into a render
# failure, someone dropping the key breaks the guard itself; both land
# here.
feats=$(yq '.karpenter.amiKernelFeatures[]' values.yaml)
grep -qx FUSE_PASSTHROUGH <<<"$feats" || {
  echo "FAIL: values.yaml karpenter.amiKernelFeatures no longer declares FUSE_PASSTHROUGH" >&2
  exit 1
}

# (ii) the default declaration renders with karpenter enabled.
helm template rio . \
  --set global.image.tag=test \
  --set karpenter.enabled=true \
  --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role \
  --set karpenter.amiTag=test >/dev/null

# (iii) declaring an AMI without FUSE_PASSTHROUGH fails the render, and
# the failure names the fix (kernel.nix / xtask ami).
err=$TMPDIR/ami-kernel-features-err.txt
if helm template rio . \
  --set global.image.tag=test \
  --set karpenter.enabled=true \
  --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role \
  --set karpenter.amiTag=test \
  --set karpenter.amiKernelFeatures=null >/dev/null 2>"$err"; then
  echo "FAIL: karpenter.enabled=true without FUSE_PASSTHROUGH in amiKernelFeatures should fail render" >&2
  exit 1
fi
for want in FUSE_PASSTHROUGH kernel.nix; do
  grep -q "$want" "$err" || {
    echo "FAIL: amiKernelFeatures render failure does not mention '$want':" >&2
    cat "$err" >&2
    exit 1
  }
done

# (iv) the guard is karpenter-scoped: k3s/dev profiles (karpenter off)
# keep rendering even with the declaration cleared.
helm template rio . \
  --set global.image.tag=test \
  --set karpenter.amiKernelFeatures=null >/dev/null
