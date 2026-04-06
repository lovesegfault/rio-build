#!/usr/bin/env bash
# Gate check: build .#ci + .#coverage-full once, report PASS/FAIL.
# Exit 0 = pass, 1 = fail.
#
# Usage: ./flake-check.sh [label]
#   label: optional tag for log filenames (defaults to short SHA)

set -uo pipefail
cd "$(dirname "$(realpath "$0")")"

label="${1:-$(git rev-parse --short=8 HEAD)}"
logdir="/tmp/sprint-save"
mkdir -p "$logdir"

driver_log="${logdir}/driver-${label}.log"
exec > >(tee -a "$driver_log") 2>&1

branch=$(git rev-parse --abbrev-ref HEAD)
if [ "$branch" != "sprint-save" ]; then
  echo "ERROR: not on sprint-save (on: $branch)" >&2
  exit 125
fi

log="${logdir}/gate-${label}.log"
echo "=== [$label] building .#ci .#coverage-full ==="

nix build --no-link --eval-store auto --store ssh-ng://nxb-dev -L \
  '.#ci' '.#coverage-full' \
  > "$log" 2>&1
rc=$?

# KVM verification
kvm_denied=$(grep -cE "failed to initialize kvm|KVM.*[Pp]ermission denied|Could not access KVM" "$log" 2>/dev/null); kvm_denied=${kvm_denied:-0}
kvm_detected=$(grep -cE "Hypervisor detected: KVM" "$log" 2>/dev/null); kvm_detected=${kvm_detected:-0}
kvm_hardfail=$(grep -c "KVM NOT AVAILABLE" "$log" 2>/dev/null); kvm_hardfail=${kvm_hardfail:-0}

kvm_note=""
[ "$kvm_denied" -gt 0 ] && kvm_note="KVM-DENIED×${kvm_denied}"
[ -z "$kvm_note" ] && [ "$kvm_detected" -gt 0 ] && kvm_note="kvm-detected×${kvm_detected}"

if [ $rc -eq 0 ]; then
  echo "=== [$label] PASS ${kvm_note:+[$kvm_note]} ==="
  exit 0
else
  echo "=== [$label] FAIL rc=$rc ${kvm_note:+[$kvm_note]} ==="
  if [ "$kvm_hardfail" -gt 0 ]; then
    echo "--- KVM HARD-FAIL (${kvm_hardfail} test(s)) ---"
    grep -B1 "KVM NOT AVAILABLE" "$log" | grep -oE "vm-test-run-[a-z-]+" | sort -u
    echo "Check builder /dev/kvm permissions"
  fi
  echo "--- errors ---"
  grep -E "error:|FAILED|AssertionError|Cannot build|panicked" "$log" | tail -10
  exit 1
fi
