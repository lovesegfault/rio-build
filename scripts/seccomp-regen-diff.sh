#!/usr/bin/env bash
# Regenerate seccomp-rio-worker.json from moby default.json and diff
# against the checked-in version. Run manually when bumping moby tag.
#
# Derivation: moby's default.json has conditional blocks keyed on caps
# (CAP_SYS_ADMIN gets mount/umount2/setns, CAP_SYS_CHROOT gets chroot).
# Flatten for the caps the worker HAS, then remove the 5 we explicitly
# deny (ptrace, bpf, setns, process_vm_readv, process_vm_writev per
# security.md r[worker.seccomp.localhost-profile]).
#
# NOT in CI — network-dependent, and moby bumps are rare. Run manually
# on moby tag bumps (dependabot or periodic check).
#
# The jq flattening is approximate — moby's format is complex
# (architecture-specific blocks, minKernel conditionals). The script
# produces a diff for HUMAN REVIEW, not a mechanical overwrite. If the
# diff is non-trivial, a human reads both files.
set -euo pipefail
MOBY_TAG="${1:-v27.5.1}"
OURS=infra/helm/rio-build/files/seccomp-rio-worker.json
DENIED=(ptrace bpf setns process_vm_readv process_vm_writev)

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

curl -sfL "https://raw.githubusercontent.com/moby/moby/${MOBY_TAG}/profiles/seccomp/default.json" |
  jq --argjson caps '["CAP_SYS_ADMIN","CAP_SYS_CHROOT"]' '
    # Flatten: default block + blocks whose .includes.caps ⊆ our caps.
    # Moby format: .syscalls is an array of {names:[], action:, includes:{caps:[]}}.
    .syscalls |= map(select(
      (.includes.caps // []) | all(. as $c | $caps | index($c))
    ))
  ' |
  jq --argjson denied "$(printf '%s\n' "${DENIED[@]}" | jq -R . | jq -s .)" '
    # Remove denied syscalls from every .names array.
    .syscalls |= map(.names -= $denied)
  ' > "$tmp"

diff -u "$OURS" "$tmp" || {
  echo "DRIFT: moby ${MOBY_TAG} default.json differs from checked-in profile"
  echo "Review the diff. If moby added safe syscalls, update $OURS."
  echo "If moby removed syscalls, check whether worker builds need them."
  exit 1
}
echo "No drift vs moby ${MOBY_TAG}"
