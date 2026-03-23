#!/usr/bin/env bash
# Flake-check: inject impurity into rust source, rebuild full gate 3×, report.
# Exit 0 = all 3 passed, 1 = at least one failure (flaky), 2 = all 3 failed (broken).
#
# Usage: ./flake-check.sh [label]
#   label: optional tag for log filenames (defaults to short SHA)

set -uo pipefail
cd "$(dirname "$(realpath "$0")")"

label="${1:-$(git rev-parse --short=8 HEAD)}"
impurity_file="rio-common/src/lib.rs"
logdir="/tmp/sprint-save"
mkdir -p "$logdir"

# Self-log driver output (script status, not build logs)
driver_log="${logdir}/driver-${label}.log"
exec > >(tee -a "$driver_log") 2>&1

# Sanity: must be on sprint-save with clean tree (aside from impurity marker)
branch=$(git rev-parse --abbrev-ref HEAD)
if [ "$branch" != "sprint-save" ]; then
  echo "ERROR: not on sprint-save (on: $branch)" >&2
  exit 125
fi

# Clean any prior impurity markers
git checkout -- "$impurity_file" 2>/dev/null || true

# Ensure impurity is reverted even if we're killed mid-run — otherwise
# the nonce gets swept into the next `git add -A` commit.
trap 'git checkout -- "$impurity_file" 2>/dev/null || true' EXIT INT TERM

pass=0
fail=0
results=()

for i in 1 2 3; do
  nonce="flake-check-${label}-run${i}-$(date +%s%N)"
  log="${logdir}/gate-${label}-run${i}.log"

  # Inject impurity: trailing comment in rio-common/src/lib.rs
  # crate2nix filesets hash file content → comment changes derivation hash → full rebuild cascade
  printf '\n// impurity: %s\n' "$nonce" >> "$impurity_file"

  echo "=== [$label] run $i/3: nonce=$nonce ==="

  # Remote-store mode: local eval, remote build+substitute (faster than --builders dispatch)
  nix build --no-link --eval-store auto --store ssh-ng://nxb-dev -L \
    '.#ci' '.#coverage-full' \
    > "$log" 2>&1
  rc=$?

  # Revert impurity for next run
  git checkout -- "$impurity_file"

  # KVM verification: scan log for KVM-denied / TCG fallback markers
  # Previous session's flakiness was masked by TCG fallback — build "passed" but under TCG,
  # which has different timing and exposes races that KVM doesn't.
  kvm_denied=$(grep -cE "failed to initialize kvm|KVM.*[Pp]ermission denied|Could not access KVM" "$log" 2>/dev/null); kvm_denied=${kvm_denied:-0}
  tcg_fallback=$(grep -cE "falling back to.*[Tt][Cc][Gg]|accel.*tcg|using TCG" "$log" 2>/dev/null); tcg_fallback=${tcg_fallback:-0}
  kvm_used=$(grep -cE "accel.*kvm|KVM.*acceleration|kvm version" "$log" 2>/dev/null); kvm_used=${kvm_used:-0}
  kvm_detected=$(grep -cE "Hypervisor detected: KVM" "$log" 2>/dev/null); kvm_detected=${kvm_detected:-0}

  kvm_note=""
  if [ "$kvm_denied" -gt 0 ]; then
    kvm_note="KVM-DENIED×${kvm_denied}"
  elif [ "$tcg_fallback" -gt 0 ]; then
    kvm_note="TCG-FALLBACK×${tcg_fallback}"
  elif [ "$kvm_detected" -gt 0 ]; then
    kvm_note="kvm-detected×${kvm_detected}"
  fi

  # KVM hard-fail detection: the kvm-check preamble raises
  # "KVM NOT AVAILABLE — HARD FAIL" if /dev/kvm isn't usable.
  # Surface that in the run summary for visibility.
  kvm_hardfail=$(grep -c "KVM NOT AVAILABLE" "$log" 2>/dev/null); kvm_hardfail=${kvm_hardfail:-0}

  if [ $rc -eq 0 ]; then
    pass=$((pass+1))
    results+=("run${i}:PASS${kvm_note:+[$kvm_note]}")
    echo "    PASS (log: $log) ${kvm_note:+[$kvm_note]}"
    # PASS with KVM-DENIED or TCG-FALLBACK is suspicious — build "worked" but under degraded accel
    if [ "$kvm_denied" -gt 0 ] || [ "$tcg_fallback" -gt 0 ]; then
      echo "    WARNING: PASS under degraded acceleration — timing may differ from KVM"
    fi
  else
    fail=$((fail+1))
    results+=("run${i}:FAIL(rc=$rc)${kvm_note:+[$kvm_note]}")
    echo "    FAIL rc=$rc (log: $log) ${kvm_note:+[$kvm_note]}"
    if [ "$kvm_hardfail" -gt 0 ]; then
      echo "    --- KVM HARD-FAIL (${kvm_hardfail} test(s)) ---"
      grep -B1 "KVM NOT AVAILABLE" "$log" | grep -oE "vm-test-run-[a-z-]+" | sort -u
      echo "    Check builder /dev/kvm permissions (udev rule, group membership, or MODE)"
    fi
    echo "    --- last errors ---"
    grep -E "error:|FAILED|AssertionError|Cannot build|build_failed|panicked" "$log" | tail -10 || true
    echo "    --- last 20 lines ---"
    tail -20 "$log"
    if [ "$kvm_denied" -gt 0 ]; then
      echo "    --- KVM-denied context ---"
      grep -B2 -A5 -E "failed to initialize kvm|KVM.*[Pp]ermission denied" "$log" | head -30
    fi
  fi
done

echo ""
echo "=== [$label] SUMMARY: ${results[*]} ==="
echo "=== pass=$pass fail=$fail ==="

# Write JSON summary for programmatic consumption
jq -n \
  --arg label "$label" \
  --arg sha "$(git rev-parse --short=8 HEAD)" \
  --argjson pass "$pass" \
  --argjson fail "$fail" \
  --arg results "${results[*]}" \
  --arg ts "$(date -Iseconds)" \
  '{label:$label, sha:$sha, pass:$pass, fail:$fail, results:$results, ts:$ts}' \
  > "${logdir}/summary-${label}.json"

if [ $fail -eq 0 ]; then
  echo "STABLE: 3/3 passed"
  exit 0
elif [ $pass -eq 0 ]; then
  echo "BROKEN: 0/3 passed"
  exit 2
else
  echo "FLAKY: $pass/3 passed"
  exit 1
fi
