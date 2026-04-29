# karpenter.dataVolumeSize MUST cover the largest pod ephemeral-storage
# request a §13b NodeClaim can place — kube-scheduler's NodeResourcesFit
# SUMS ephemeral-storage across bound pods (same as cpu/mem), so
# `cover::sizing` chunks claims at `max_node_disk` ≈ 90% of
# dataVolumeSize. This check guards only that ONE max-disk pod fits
# (the runtime n-axis chunking in `claim_count` handles the multi-pod
# case structurally). Per-pod formula (pool/jobs.rs pod_ephemeral_request):
#   disk_bytes × OVERLAY_HEADROOM + fuse_cache_bytes + LOG_BUDGET_BYTES
# clamped at sla.ceilings.maxDisk. kubelet reserves ~10% before allocatable.
#
# nvme hw-classes are exempt (instance-store, not this EBS volume) — this
# only guards the rio-default EC2NodeClass.

# Mirror jobs.rs constants. headroom(n_eff) is bounded above by
# headroom(1.0) = 1.25 + 0.7 = 1.95.
OVERLAY_HEADROOM_PCT=195   # worst-case headroom(n_eff=1)
LOG_BUDGET_BYTES=$((1 << 30))
RESERVE_PCT=110            # ~10% kubelet reserve

max_disk=$(yq '.scheduler.sla.maxDisk' values.yaml)
fuse=$(yq '.poolDefaults.fuseCacheBytes' values.yaml)
vol=$(yq '.karpenter.dataVolumeSize' values.yaml)

# dataVolumeSize is a k8s Quantity string ("500Gi"); normalize to bytes.
case "$vol" in
  *Gi) vol_b=$(( ${vol%Gi} * (1 << 30) )) ;;
  *Ti) vol_b=$(( ${vol%Ti} * (1 << 40) )) ;;
  *)   echo "FAIL: dataVolumeSize '$vol' not Gi/Ti-suffixed" >&2; exit 1 ;;
esac

need=$(( (max_disk * OVERLAY_HEADROOM_PCT / 100 + fuse + LOG_BUDGET_BYTES) \
         * RESERVE_PCT / 100 ))

test "$vol_b" -ge "$need" || {
  echo "FAIL: karpenter.dataVolumeSize=$vol ($vol_b B) < required $need B" >&2
  echo "  = (sla.maxDisk × 1.95 + poolDefaults.fuseCacheBytes + 1Gi) × 1.1" >&2
  echo "  = ($max_disk × 1.95 + $fuse + $LOG_BUDGET_BYTES) × 1.1" >&2
  exit 1
}

# controller.toml must pass poolDefaults.fuseCacheBytes through so the
# NodeClaim path computes the same ephemeral request the pod will make.
toml=$TMPDIR/ctrl.toml
helm template rio . \
  --set karpenter.enabled=true \
  --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role \
  --set karpenter.amiTag=test \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  | yq -N 'select(.kind=="ConfigMap" and .metadata.name=="rio-controller-config")
           | .data."controller.toml"' >"$toml"

got=$(grep -E '^fuse_cache_bytes = ' "$toml" | grep -oE '[0-9]+')
test "$got" = "$fuse" || {
  echo "FAIL: controller.toml fuse_cache_bytes=$got != poolDefaults.fuseCacheBytes=$fuse" >&2
  exit 1
}
