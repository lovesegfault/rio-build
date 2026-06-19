# Shared helpers for helm-lint fragments. Source via:
#   . "$(dirname "$0")/_lib.sh"
# (the driver runs `bash -euo pipefail "$f"` with $f an absolute store
#  path, so dirname resolves to the fragments fileset).
#
# sh-043-r3: the `helm template … | yq select(ConfigMap) | grep KEY`
# pipeline had FIVE per-file copies (11/14/23/39/52). A future fix to
# the pipefail guard or the ConfigMap name fanned out across all five.
# shellcheck shell=bash

# `helm template` with the karpenter-enabled profile (the controller
# ConfigMap renders only when karpenter.enabled=true). Extra args
# pass through.
render_karpenter() {
  helm template rio . \
    --set karpenter.enabled=true \
    --set karpenter.clusterName=ci \
    --set karpenter.nodeRoleName=ci-role \
    --set karpenter.amiTag=test \
    --set global.image.tag=test \
    --set postgresql.enabled=false \
    "$@"
}

# Extract a ConfigMap's TOML body from rendered manifests.
# $1=configmap name, $2=.data key, $3=file (defaults to stdin).
toml_body() {
  yq -N "select(.kind==\"ConfigMap\" and .metadata.name==\"$1\") | .data.\"$2\"" "${3:--}"
}

# render_karpenter "$@" | controller.toml body — the dominant shape.
render_controller_toml() {
  render_karpenter "$@" | toml_body rio-controller-config controller.toml
}

# Read an integer-valued top-level TOML key from stdin. `|| true`
# inside the pipelines: grep's no-match exit must reach the CALLER's
# dedicated failure message, not die silently in a `set -e` command
# substitution (the stdenv-pipefail trap).
toml_int_key() {
  { grep -E "^$1 = " || true; } | grep -oE '[0-9]+' || true
}
