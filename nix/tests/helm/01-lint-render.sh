# Base lint + render every value profile. Catches Go-template syntax
# errors, missing required values, bad YAML in rendered output.

helm lint .

# Default (prod) profile: tag must be set (empty → bad image ref).
helm template rio . --set global.image.tag=test >/dev/null
helm template rio . -f values/dev.yaml >/dev/null
helm template rio . -f values/vmtest-full.yaml >/dev/null

# ADR-021: karpenter.enabled requires amiTag (NixOS AMI is the only
# EC2NodeClass — no Bottlerocket fallback). The `required` template
# func should fail without it.
if helm template rio . --set global.image.tag=test \
  --set karpenter.enabled=true \
  --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci 2>/dev/null; then
  echo "FAIL: karpenter.enabled=true without amiTag should fail render" >&2
  exit 1
fi

# Values-gated RIO_SUBSTITUTE_STALL_SECS contract (both halves): set →
# renders with the exact env name (a typo'd name would ship green and
# the knob would silently never reach the binary); unset → absent, so
# the binary default (180 s) governs.
helm template rio . --set global.image.tag=test \
  --set store.substituteStallSecs=240 \
  | grep -A1 'name: RIO_SUBSTITUTE_STALL_SECS' | grep -q '"240"'
if helm template rio . --set global.image.tag=test \
  | grep -q 'RIO_SUBSTITUTE_STALL_SECS'; then
  echo "FAIL: RIO_SUBSTITUTE_STALL_SECS must not render when unset" >&2
  exit 1
fi
