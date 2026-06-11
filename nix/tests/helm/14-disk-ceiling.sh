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

# live_057-c (W10-CP): the defaultDisk density derivation — the
# committed table values.yaml narrates at sla.defaultDisk, asserted
# here against the live values with the same content-mirrored
# constants (the member-4 census[gen:] precedent at
# jobs.rs::pod_ephemeral_request), so the helm value and the rust
# accounting cannot drift silently. Inputs: defaultDisk,
# poolDefaults.fuseCacheBytes, LOG_BUDGET_BYTES, the no-estimate
# headroom arm, and the density-input row (allocatable =
# karpenter.dataVolumeSize × 0.9). Touch ANY input ⇒ re-derive the
# whole table (narration rows + these asserts together).
# The recovery path for a too-small default is the worker-classified
# disk ladder (25→50→100, floor persisted once per pname) — the
# cross-crate end-to-end witness lives beside the classifier
# (W10-CM: quota-exhausted build ⇒ DISK_FULL report ⇒ the scheduler's
# disk floor doubles); the §1.6.4-15 order pin makes this fragment
# valid only in trees where that ladder is live.
DEFAULT_HEADROOM_PCT=150   # OVERLAY_HEADROOM_FALLBACK (no-estimate arm)
default_disk=$(yq '.scheduler.sla.defaultDisk' values.yaml)
test "$default_disk" = "26843545600" || {
  echo "FAIL: sla.defaultDisk=$default_disk B != 25 GiB (26843545600 B) —" >&2
  echo "  the W10-CP density table (values.yaml narration + this fragment)" >&2
  echo "  derives from 25 GiB; re-derive BOTH before moving the default" >&2
  exit 1
}
default_req=$(( default_disk * DEFAULT_HEADROOM_PCT / 100 + fuse + LOG_BUDGET_BYTES ))
test "$default_req" = "95026151424" || {
  echo "FAIL: default-pod ephemeral request $default_req B != the committed" >&2
  echo "  88.5 GiB row (95026151424 B = 25×1.5 + 50 fuse + 1 log GiB)" >&2
  exit 1
}
# Density-input row: allocatable = dataVolumeSize × 0.9 (the same
# kubelet-reserve fraction max_node_disk renders from), pods/node =
# floor(allocatable / request). 500Gi × 0.9 = 450 GiB.
data_vol=$(yq '.karpenter.dataVolumeSize' values.yaml)
test "$data_vol" = "500Gi" || {
  echo "FAIL: karpenter.dataVolumeSize=$data_vol != 500Gi — the W10-CP" >&2
  echo "  pods/node rows assume 450 GiB allocatable; re-derive the table" >&2
  exit 1
}
alloc=$(( 500 * 1024 * 1024 * 1024 * 9 / 10 ))
pods=$(( alloc / default_req ))
test "$pods" -eq 5 || {
  echo "FAIL: derived pods/node $pods != the committed 5 (the 2→5 density row)" >&2
  exit 1
}
# The historical 100 GiB row, kept as the comparison anchor (the B8
# live specimen: a 201 GiB ask): 2 pods/node on the same allocatable.
# (12xlarge nvme: measured 12 at 201 GiB ⇒ allocatable ∈ [2412, 2613)
# GiB ⇒ 27–29 pods/node at 88.5 GiB — a BAND over instance-store
# capacities, narration-only: no nvme allocatable renders here.)
old_req=$(( 107374182400 * DEFAULT_HEADROOM_PCT / 100 + fuse + LOG_BUDGET_BYTES ))
test $(( alloc / old_req )) -eq 2 || {
  echo "FAIL: the historical 100 GiB row no longer derives 2 pods/node —" >&2
  echo "  a density INPUT (fuse/log/allocatable) moved; re-derive the table" >&2
  exit 1
}

# live_057-d (W10-CQ): the fuse-cache MEASURED-RULED record's
# narration binds. The values.yaml comment at fuseCacheBytes must
# name the measured trigger's occupancy gauge, carry the warm-pod
# dominance rows consistent with the LIVE values, and the value
# itself stands at the ruled 50 GiB until the 7-day p99 trigger
# fires (the gauge is the cross-plane instrument, landed beside the
# builder quota sample; quoted here, never re-run).
test "$fuse" = "53687091200" || {
  echo "FAIL: poolDefaults.fuseCacheBytes=$fuse B != the RULED 50 GiB" >&2
  echo "  (53687091200 B). The measured ruling stands until the 7-day p99" >&2
  echo "  rio_builder_fuse_cache_bytes_used trigger; re-derive the RULED" >&2
  echo "  record + the W10-CQ/W10-CP rows together before moving it" >&2
  exit 1
}
grep -q 'rio_builder_fuse_cache_bytes_used' values.yaml || {
  echo "FAIL: the fuse RULED record lost its trigger gauge name" >&2
  echo "  (rio_builder_fuse_cache_bytes_used — the H9″ instrument)" >&2
  exit 1
}
warm_req=$(( 1073741824 * DEFAULT_HEADROOM_PCT / 100 + fuse + LOG_BUDGET_BYTES ))
test "$warm_req" = "56371445760" || {
  echo "FAIL: warm-pod (1 GiB solve) request $warm_req B != the committed" >&2
  echo "  52.5 GiB row (56371445760 B) — re-derive the dominance rows" >&2
  exit 1
}
grep -q '52.5 GiB' values.yaml || {
  echo "FAIL: the warm-pod 52.5 GiB dominance row drifted from values.yaml" >&2
  exit 1
}
fuse_share=$(( fuse * 100 / default_req ))
test "$fuse_share" -eq 56 || {
  echo "FAIL: fuse share of the default-pod request is ${fuse_share}%, the" >&2
  echo "  committed dominance row says 56% — re-derive both" >&2
  exit 1
}
