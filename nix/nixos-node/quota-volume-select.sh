# Select the dedicated kubelet quota volume from a device-glob
# population (rio-kubelet-mount's EBS-branch enumeration, extracted so
# the selection logic is unit-testable against fixture by-id
# namespaces — merged_bug_024: the in-unit copy was only ever
# exercised against a bare /dev/vdb, never against the per-partition
# by-id links the production glob actually matches).
#
# usage: quota-volume-select GLOB...
#   stdout: the single selected bare whole-disk device (resolved path)
#   exit 0: selected; exit 1: typed refusal (stderr names the rejected
#           classes — the operator trail)
#
# Device-class law (merged_bug_024, R31'(iv)): candidacy is decided by
# TYPED predicates over the device taxonomy, never inferred from
# children/mountpoint side effects alone:
#
#   1. partition LINKS are rejected by NAME CLASS (`*-part[0-9]*`):
#      udev mints one by-id link per partition beside every whole-disk
#      link (`vol<id>` AND `vol<id>-partN`), so any whole-volume glob
#      matches both classes. A partition is never the quota volume.
#      On the ami-bios (legacy+gpt) AMI, partition 1 is the never-
#      mounted, filesystem-free bios_grub partition — it PASSES the
#      children/mountpoint filters, which is exactly how every
#      x86-metal boot counted n_bare=2 (kubelet Requires= hard-fail,
#      NotReady/reap churn), and how the quota-volume-late corner
#      selected bios_grub for mkfs.xfs -f.
#   2. every surviving candidate must read `lsblk TYPE == disk` (the
#      kernel's own taxonomy): partitions reached through suffix-free
#      aliases, loop devices, and md members are rejected by class
#      even when their link names carry no -part suffix.
#   3. of the whole disks, the one with partition children is the
#      root/boot disk (excluded); a mounted disk is somebody's
#      filesystem (excluded); exactly one bare disk must remain.
#
# Diagnostics go to stderr; stdout carries ONLY the selected device.

declare -A seen
cands=()
n_partlink=0
n_nondisk=0
n_children=0
n_mounted=0

for g in "$@"; do
  # Unquoted on purpose: each configured entry is itself a glob.
  for l in $g; do
    [ -e "$l" ] || continue
    case "$l" in
      *-part[0-9]*)
        # Class 1: a per-partition by-id link. Rejected by name —
        # no probing, no side-effect inference.
        n_partlink=$((n_partlink + 1))
        continue
        ;;
    esac
    # udev mints two by-id links per NVMe namespace (`…_<serial>` and
    # `…_<serial>_<nsid>`); resolve and dedup so one volume cannot
    # count twice.
    d=$(readlink -f "$l")
    if [ -z "${seen[$d]:-}" ]; then
      seen[$d]=1
      cands+=("$d")
    fi
  done
done

quota_dev=""
n_bare=0
for d in "${cands[@]:-}"; do
  [ -n "$d" ] || continue
  # Class 2: the kernel's device taxonomy. Only whole disks compete.
  t=$(lsblk -ndo TYPE "$d" 2>/dev/null || true)
  if [ "$t" != "disk" ]; then
    n_nondisk=$((n_nondisk + 1))
    continue
  fi
  # Class 3a: a disk carrying partitions is the root/boot disk.
  if [ -n "$(lsblk -nro NAME "$d" | tail -n +2)" ]; then
    n_children=$((n_children + 1))
    continue
  fi
  # Class 3b: a mounted disk is somebody's filesystem.
  if [ -n "$(lsblk -nro MOUNTPOINTS "$d" | tr -d '[:space:]')" ]; then
    n_mounted=$((n_mounted + 1))
    continue
  fi
  quota_dev="$d"
  n_bare=$((n_bare + 1))
done

trail() {
  echo "quota-volume-select: rejected by class: $n_partlink partition-link," \
    "$n_nondisk non-disk, $n_children partitioned (root/boot)," \
    "$n_mounted mounted" >&2
}

if [ "$n_bare" -eq 0 ]; then
  echo "quota-volume-select: NO bare quota volume found — the EC2NodeClass" >&2
  echo "must attach the second EBS mapping (live_060: an EBS-only builder" >&2
  echo "node without prjquota has a dead disk producer; refusing to let" >&2
  echo "kubelet start on the root fs)" >&2
  trail
  exit 1
fi
if [ "$n_bare" -gt 1 ]; then
  echo "quota-volume-select: $n_bare bare candidate volumes — ambiguous; refusing" >&2
  trail
  exit 1
fi
# Success also discloses what was rejected: every ami-bios boot logs
# the partition links dying BY CLASS (the I-205 churn precondition,
# visibly dead in the journal), and an operator diffing two node
# classes sees the namespace difference without reproducing a refusal.
if [ $((n_partlink + n_nondisk + n_children + n_mounted)) -gt 0 ]; then
  trail
fi
printf '%s\n' "$quota_dev"
