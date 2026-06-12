# The defaultDisk ORDER-PIN's values-surface machine check (live_060-d,
# WO-S8-16 / W12-LI2): the disk ladder's quantifier is the DEPLOYED
# FLEET PRECONDITION — every EBS-only builder-hosting EC2NodeClass
# must declare the dedicated kubelet quota volume (second EBS mapping,
# /dev/xvdb), because a builder pool without prjquota has a dead
# peak_disk_bytes producer and BOTH disk-sizing ladders silently die
# (live_060: 2022/2022 completions None while every tree-state
# witness stayed green). rio-nvme is exempt: instance-store RAID0
# owns its kubelet root via rio-nvme-mount.
#
# Planted red: a render with the quota volume nulled out must be
# CAUGHT — the pin's oracle demonstrably fires on the exact
# no-quota-builder-pool shape the live fleet had.
#
# (documentary — .sh is not tracey-scanned.)

T=$TMPDIR/quota-volume-pin
rm -rf "$T"; mkdir -p "$T"

render() {
  helm template rio . \
    --set karpenter.enabled=true \
    --set karpenter.amiTag=test \
    --set karpenter.nodeRoleName=test \
    --set karpenter.clusterName=test \
    "$@" -s templates/karpenter.yaml
}

# ── the live surface: every EBS-only class carries the quota volume ──
render > "$T/render.yaml"

for nc in rio-default rio-metal; do
  awk -v nc="$nc" '
    $0 ~ "^  name: " nc "$" { in_nc=1 }
    in_nc && /^kind:/ && !/EC2NodeClass/ { in_nc=0 }
    in_nc { print }
    in_nc && /^---/ { in_nc=0 }
  ' "$T/render.yaml" > "$T/$nc.yaml"
  grep -q 'deviceName: /dev/xvdb' "$T/$nc.yaml" || {
    echo "FAIL: EC2NodeClass $nc has NO kubelet quota volume (/dev/xvdb) —" >&2
    echo "      an EBS-only builder pool without prjquota is the live_060" >&2
    echo "      dead-producer fleet shape; the defaultDisk order-pin is void" >&2
    exit 1
  }
done

# rio-nvme stays exempt (instance-store owns its kubelet root).
awk '
  /^  name: rio-nvme$/ { in_nc=1 }
  in_nc && /^---/ { in_nc=0 }
  in_nc { print }
' "$T/render.yaml" > "$T/rio-nvme.yaml"
if grep -q 'deviceName: /dev/xvdb' "$T/rio-nvme.yaml"; then
  echo "FAIL: rio-nvme carries the EBS quota volume — instance-store classes" >&2
  echo "      mount their own prjquota root (rio-nvme-mount); a second volume" >&2
  echo "      there is dead cost and an ambiguity hazard for the mount unit" >&2
  exit 1
fi

# ── the quota volume must cover the pod_ephemeral_request inequality ──
# (kubelet's ephemeral-storage allocatable derives from the fs hosting
# /var/lib/kubelet — on EBS classes that is THIS volume; the binding
# inequality is documented at karpenter.dataVolumeSize.)
qsize=$(yq -r '.karpenter.quotaVolumeSize' values.yaml)
dsize=$(yq -r '.karpenter.dataVolumeSize' values.yaml)
[ "$qsize" = "$dsize" ] || {
  echo "FAIL: karpenter.quotaVolumeSize ($qsize) != dataVolumeSize ($dsize) —" >&2
  echo "      the pod_ephemeral_request derivation moved to the quota volume" >&2
  echo "      on EBS classes; re-derive BOTH or update this pin with the math" >&2
  exit 1
}

# ── planted RED: the no-quota builder pool shape is CAUGHT ──
if render --set karpenter.quotaVolumeSize= 2>/dev/null \
    | awk '/^  name: rio-default$/{f=1} f&&/^---/{f=0} f' \
    | grep -A2 'deviceName: /dev/xvdb' | grep -qE 'volumeSize: *[0-9]'; then
  echo "FAIL: the nulled quotaVolumeSize still rendered a sized quota volume —" >&2
  echo "      the planted no-quota shape was not representable; oracle broken" >&2
  exit 1
fi

echo "quota-volume pin: rio-default+rio-metal carry /dev/xvdb (${qsize}); rio-nvme exempt; nulled-size red caught"
