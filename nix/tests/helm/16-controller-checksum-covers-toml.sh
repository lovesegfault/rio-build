# checksum/controller-toml MUST change when ANY input that lands in the
# rendered controller.toml changes — not just `.scheduler.sla` /
# `.karpenter.nodeclaimPool`. bug_022: `poolDefaults.fuseCacheBytes` and
# `karpenter.metalSizes` are read into the TOML body but were not in the
# hashed key-list, so a fuse-cache or metal-partition bump updated the
# ConfigMap without rolling the (subPath-mounted) pod. Fix: hash the
# rendered body via a named template, like kube-build-scheduler.yaml does.

render_checksum() {
  helm template rio . \
    --set karpenter.enabled=true \
    --set karpenter.clusterName=ci \
    --set karpenter.nodeRoleName=ci-role \
    --set karpenter.amiTag=test \
    --set global.image.tag=test \
    --set postgresql.enabled=false \
    "$@" \
    | yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-controller")
             | .spec.template.metadata.annotations."checksum/controller-toml"'
}

base=$(render_checksum)
test -n "$base" || { echo "FAIL: checksum/controller-toml absent" >&2; exit 1; }

# Each axis below maps to one TOML key; changing it MUST change the hash.
for axis in \
  "poolDefaults.fuseCacheBytes=99999999999" \
  'karpenter.metalSizes={metal,metal-zz}' \
  "scheduler.sla.maxFleetCores=12345" \
  "karpenter.nodeclaimPool.leaseName=other-lease"
do
  got=$(render_checksum --set "$axis")
  test "$got" != "$base" || {
    echo "FAIL: checksum/controller-toml unchanged after --set $axis" >&2
    echo "  (rendered controller.toml input changed but checksum did not roll the pod)" >&2
    exit 1
  }
done
