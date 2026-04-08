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
