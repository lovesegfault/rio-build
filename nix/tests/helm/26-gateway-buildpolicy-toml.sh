# gateway.buildPolicy must render a ConfigMap-backed /etc/rio/gateway.toml,
# the checksum/gateway-toml annotation must roll when a policy entry
# changes (the gateway reads the file once at boot and subPath mounts
# don't live-update — without the checksum a policy edit updates the
# ConfigMap but never reaches the running pod), and the subPath mount
# must exist whenever the map is non-empty. Empty map → no ConfigMap,
# no mount (config stays env-only).
#
# Same render flag set as 16-controller-checksum-covers-toml.sh:
# karpenter.enabled=true demands clusterName/nodeRoleName/amiTag — keep
# the sets identical so both fragments exercise the same rendered chart.

render() {
  helm template rio . \
    --set karpenter.enabled=true \
    --set karpenter.clusterName=ci \
    --set karpenter.nodeRoleName=ci-role \
    --set karpenter.amiTag=test \
    --set global.image.tag=test \
    --set postgresql.enabled=false \
    "$@"
}

base=$TMPDIR/gateway-buildpolicy-base.yaml
render >"$base"

# 1. Default values carry the inert qa-keep-going entry → the ConfigMap
#    renders a quoted [build_policy."qa-keep-going"] table with
#    keep_going = true (lowercase TOML boolean).
toml=$(yq -N 'select(.kind=="ConfigMap" and .metadata.name=="rio-gateway-config")
              | .data."gateway.toml"' "$base")
echo "$toml" | grep -F '[build_policy."qa-keep-going"]' >/dev/null || {
  echo 'FAIL: gateway.toml missing the [build_policy."qa-keep-going"] table' >&2
  exit 1
}
echo "$toml" | grep -x 'keep_going = true' >/dev/null || {
  echo "FAIL: qa-keep-going entry did not render keep_going = true" >&2
  exit 1
}

# 2. Checksum annotation hashes the rendered TOML body and rolls the pod
#    when a policy entry changes.
csum() {
  yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-gateway")
         | .spec.template.metadata.annotations."checksum/gateway-toml"' "$1"
}
base_sum=$(csum "$base")
echo "$base_sum" | grep -E '^[0-9a-f]{64}$' >/dev/null || {
  echo "FAIL: checksum/gateway-toml absent or not a sha256 (got: '$base_sum')" >&2
  exit 1
}
flipped=$TMPDIR/gateway-buildpolicy-flipped.yaml
render --set gateway.buildPolicy.qa-keep-going.keepGoing=false >"$flipped"
test "$(csum "$flipped")" != "$base_sum" || {
  echo "FAIL: checksum/gateway-toml unchanged after flipping qa-keep-going.keepGoing" >&2
  echo "  (a policy edit would update the ConfigMap without rolling the subPath-mounted pod)" >&2
  exit 1
}

# Inverse (mirrors helm/19): an input that does NOT land in the TOML must
# NOT roll the pod — the named template must render only gateway.toml.
# gateway.sessionDrainSecs feeds the Deployment (terminationGracePeriodSeconds
# and the RIO_SESSION_DRAIN_SECS env) but not the TOML. gateway.replicas
# would NOT work here: under the chart default autoscaling=on it is rendered
# nowhere (gateway.yaml emits replicas only with autoscaling off, and the
# maxReplicas ternaries discard it), so perturbing it leaves the render
# byte-identical and the equality below holds for ANY checksum definition.
drain=$TMPDIR/gateway-buildpolicy-drain.yaml
render --set gateway.sessionDrainSecs=123 >"$drain"
# Control first: the perturbation must actually change the render — a
# render no-op perturbation would make this inverse vacuous again.
if cmp -s "$base" "$drain"; then
  echo "FAIL: gateway.sessionDrainSecs=123 left the render byte-identical — the inverse" >&2
  echo "  check below is vacuous; pick a perturbation that lands in the Deployment" >&2
  exit 1
fi
test "$(csum "$drain")" = "$base_sum" || {
  echo "FAIL: checksum/gateway-toml changed on gateway.sessionDrainSecs (non-TOML input)" >&2
  echo "  (over-coverage rolls the only SSH build-submission ingress — and its live" >&2
  echo "   sessions — on every unrelated gateway.* edit)" >&2
  exit 1
}

# 3. subPath mount + configMap volume present when the map is non-empty.
yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-gateway")
       | .spec.template.spec.containers[0].volumeMounts[]
       | select(.mountPath=="/etc/rio/gateway.toml")
       | .subPath' "$base" | grep -x gateway.toml >/dev/null || {
  echo "FAIL: gateway.toml subPath mount missing from the gateway container" >&2
  exit 1
}
yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-gateway")
       | .spec.template.spec.volumes[]
       | select(.name=="gateway-config")
       | .configMap.name' "$base" | grep -x rio-gateway-config >/dev/null || {
  echo "FAIL: gateway-config volume missing or not backed by the rio-gateway-config ConfigMap" >&2
  exit 1
}

# 4. Empty map → no ConfigMap, no mount, no volume. `--set-json
#    gateway.buildPolicy={}` would NOT exercise this: helm deep-merges
#    user values into defaults, so an empty map can't clear the default
#    qa-keep-going entry — only nulling the key deletes it.
off=$TMPDIR/gateway-buildpolicy-off.yaml
render --set gateway.buildPolicy=null >"$off"
cm_off=$(yq -N 'select(.kind=="ConfigMap" and .metadata.name=="rio-gateway-config")' "$off")
test -z "$cm_off" || {
  echo "FAIL: empty gateway.buildPolicy must not render the rio-gateway-config ConfigMap" >&2
  exit 1
}
mount_off=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-gateway")
                   | .spec.template.spec.containers[0].volumeMounts[]
                   | select(.mountPath=="/etc/rio/gateway.toml")' "$off")
test -z "$mount_off" || {
  echo "FAIL: empty gateway.buildPolicy must not render the gateway.toml mount" >&2
  exit 1
}
volume_off=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-gateway")
                    | .spec.template.spec.volumes[]
                    | select(.name=="gateway-config")' "$off")
test -z "$volume_off" || {
  echo "FAIL: empty gateway.buildPolicy must not render the gateway-config volume" >&2
  exit 1
}
