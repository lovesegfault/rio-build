# max_node_disk (controller.toml — `cover::sizing`'s per-claim
# ephemeral-storage cap) MUST cover the largest single-pod request:
#   disk_bytes × OVERLAY_HEADROOM + fuse_cache_bytes + LOG_BUDGET_BYTES
# clamped at sla.maxDisk. kube-scheduler's NodeResourcesFit SUMS
# ephemeral-storage across bound pods (same as cpu/mem); this check
# guards only that ONE max-disk pod fits (the runtime 3-axis chunking
# in `claim_count` handles the multi-pod case structurally).
#
# Single-source: assert against the helm-RENDERED `max_node_disk`
# (= dataVolumeSize × 0.9 in templates/controller.yaml) instead of
# re-deriving the kubelet-reserve fraction here — two open-coded
# constants (×0.9 vs ×1/1.1) aren't inverses and the gap between them
# was a lint pass-gap (r26 bug_028).
#
# nvme hw-classes are exempt (instance-store, not this EBS volume) — this
# only guards the rio-default EC2NodeClass.

# Mirror jobs.rs constants. headroom(n_eff) is bounded above by
# headroom(1.0) = 1.25 + 0.7 = 1.95.
OVERLAY_HEADROOM_PCT=195   # worst-case headroom(n_eff=1)
LOG_BUDGET_BYTES=$((1 << 30))

max_disk=$(yq '.scheduler.sla.maxDisk' values.yaml)
fuse=$(yq '.poolDefaults.fuseCacheBytes' values.yaml)

# Render controller.toml — single source for both fuse_cache_bytes and
# max_node_disk (the values cover::sizing actually reads).
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

got_fuse=$(grep -E '^fuse_cache_bytes = ' "$toml" | grep -oE '[0-9]+')
test "$got_fuse" = "$fuse" || {
  echo "FAIL: controller.toml fuse_cache_bytes=$got_fuse != poolDefaults.fuseCacheBytes=$fuse" >&2
  exit 1
}

max_node_disk=$(grep -E '^max_node_disk = ' "$toml" | grep -oE '[0-9]+')
need=$(( max_disk * OVERLAY_HEADROOM_PCT / 100 + fuse + LOG_BUDGET_BYTES ))

test "$max_node_disk" -ge "$need" || {
  echo "FAIL: controller.toml max_node_disk=$max_node_disk B < required $need B" >&2
  echo "  = sla.maxDisk × 1.95 + poolDefaults.fuseCacheBytes + 1Gi" >&2
  echo "  = $max_disk × 1.95 + $fuse + $LOG_BUDGET_BYTES" >&2
  echo "  raise karpenter.dataVolumeSize (max_node_disk = dataVolumeSize × 0.9)" >&2
  exit 1
}
