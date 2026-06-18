# max_node_disk (controller.toml — `cover::sizing`'s per-claim
# ephemeral-storage cap) MUST cover the largest single-pod request:
#   disk_bytes × OVERLAY_HEADROOM + LOG_BUDGET_BYTES
# clamped at sla.maxDisk (jobs.rs::pod_ephemeral_request — the per-pod
# FUSE cache is gone since P0560; the node-level castore caches are
# mountd-owned hostPaths outside per-pod ephemeral-storage accounting).
# kube-scheduler's NodeResourcesFit SUMS ephemeral-storage across bound
# pods (same as cpu/mem); this check guards only that ONE max-disk pod
# fits (the runtime 3-axis chunking in `claim_count` handles the
# multi-pod case structurally).
#
# Single-source: assert against the helm-RENDERED `max_node_disk`
# (= dataVolumeSize × 0.9 in templates/controller.yaml) instead of
# re-deriving the kubelet-reserve fraction here — two open-coded
# constants (×0.9 vs ×1/1.1) aren't inverses and the gap between them
# was a lint pass-gap (r26 bug_028).
#
# nvme hw-classes are exempt (instance-store, not this EBS volume) — this
# only guards the rio-default EC2NodeClass.

. "$(dirname "$0")/_lib.sh"

# Mirror jobs.rs constants. headroom(n_eff) is bounded above by
# headroom(1.0) = 1.25 + 0.7 = 1.95.
OVERLAY_HEADROOM_PCT=195   # worst-case headroom(n_eff=1)
LOG_BUDGET_BYTES=$((1 << 30))

max_disk=$(yq '.scheduler.sla.maxDisk' values.yaml)

# Render controller.toml — the max_node_disk value cover::sizing
# actually reads.
toml=$TMPDIR/ctrl.toml
render_controller_toml >"$toml"

max_node_disk=$(toml_int_key max_node_disk <"$toml")
need=$(( max_disk * OVERLAY_HEADROOM_PCT / 100 + LOG_BUDGET_BYTES ))

test "$max_node_disk" -ge "$need" || {
  echo "FAIL: controller.toml max_node_disk=$max_node_disk B < required $need B" >&2
  echo "  = sla.maxDisk × 1.95 + 1Gi" >&2
  echo "  = $max_disk × 1.95 + $LOG_BUDGET_BYTES" >&2
  echo "  raise karpenter.dataVolumeSize (max_node_disk = dataVolumeSize × 0.9)" >&2
  exit 1
}

# live_057-c (W10-CP): the defaultDisk density derivation — the
# committed table values.yaml narrates at sla.defaultDisk, asserted
# here against the live values with the same content-mirrored
# constants (the member-4 census[gen:] precedent at
# jobs.rs::pod_ephemeral_request), so the helm value and the rust
# accounting cannot drift silently. Inputs: defaultDisk,
# LOG_BUDGET_BYTES, the no-estimate headroom arm, and the
# density-input row (allocatable = karpenter.dataVolumeSize × 0.9).
# Touch ANY input ⇒ re-derive the whole table (narration rows + these
# asserts together).
# The per-pod fuse-cache addend is GONE since P0560/ADR-022 (castore
# caches are node-level mountd-owned hostPaths outside per-pod
# ephemeral-storage accounting); the W10-CQ fuseCacheBytes
# MEASURED-RULED record is dropped accordingly.
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
default_req=$(( default_disk * DEFAULT_HEADROOM_PCT / 100 + LOG_BUDGET_BYTES ))
test "$default_req" = "41339060224" || {
  echo "FAIL: default-pod ephemeral request $default_req B != the committed" >&2
  echo "  38.5 GiB row (41339060224 B = 25×1.5 + 1 log GiB)" >&2
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
test "$pods" -eq 11 || {
  echo "FAIL: derived pods/node $pods != the committed 11 (the 5→11 density row, post-P0560 fuse-addend drop)" >&2
  exit 1
}
# The historical 100 GiB row, kept as the comparison anchor (the B8
# live specimen: a 201 GiB ask): 2 pods/node on the same allocatable.
# (12xlarge nvme: measured 12 at 201 GiB ⇒ allocatable ∈ [2412, 2613)
# GiB — a BAND over instance-store capacities, narration-only: no nvme
# allocatable renders here.)
old_req=$(( 107374182400 * DEFAULT_HEADROOM_PCT / 100 + LOG_BUDGET_BYTES ))
test $(( alloc / old_req )) -eq 2 || {
  echo "FAIL: the historical 100 GiB row no longer derives 2 pods/node —" >&2
  echo "  a density INPUT (log/allocatable) moved; re-derive the table" >&2
  exit 1
}

# live_057-d (W10-CQ) DROPPED: the fuseCacheBytes MEASURED-RULED record
# is obsolete under ADR-022 — there is no per-pod fuse cache addend, no
# poolDefaults.fuseCacheBytes key, and the warm-pod fuse-dominance rows
# (52.5 GiB / 56%) no longer derive. The
# rio_builder_fuse_cache_bytes_used gauge survives as a node-level
# castore-cache occupancy instrument (metric-help.json), no longer a
# per-pod sizing input. TODO(adr-022-rebase): re-derive the values.yaml
# W10-CP narration rows (sla.defaultDisk comment) for 38.5 GiB / 11
# pods/node in a follow-up; the asserts above are the load-bearing
# half.
