# nixpkgs-parity enablement (parity.enabled): the campaign engine's CNP
# admissions must render when enabled, the three campaign tenants must get
# default [build_policy."…"] entries in the gateway.toml ConfigMap (the
# gateway build-policy vehicle: rio-gateway-config, subPath-mounted at
# /etc/rio/gateway.toml — see 25-gateway-buildpolicy-toml.sh), and NONE of
# the parity bits may leak into the default render.

on=$TMPDIR/parity-on.yaml
helm template rio . \
  --set global.image.tag=test \
  --set parity.enabled=true \
  >"$on"

# scheduler-ingress and store-ingress each admit the engine pods
# (namespace rio-parity, app.kubernetes.io/name=rio-parity).
for pol in scheduler-ingress store-ingress; do
  yq "select(.kind==\"CiliumNetworkPolicy\" and .metadata.name==\"$pol\")
      | .spec.ingress[0].fromEndpoints[]
      | select(.matchLabels.\"k8s:app.kubernetes.io/name\" == \"rio-parity\")
      | .matchLabels.\"k8s:io.kubernetes.pod.namespace\"" "$on" |
    grep -x rio-parity >/dev/null || {
    echo "FAIL: $pol missing rio-parity fromEndpoints entry" >&2
    exit 1
  }
done

# gateway.toml ConfigMap carries the three campaign tenants with the
# campaign default policy (snake_case TOML keys are what the gateway
# config loader consumes).
toml=$(yq 'select(.kind=="ConfigMap" and .metadata.name=="rio-gateway-config") | .data."gateway.toml"' "$on")
test -n "$toml" || { echo "FAIL: rio-gateway-config ConfigMap missing with parity.enabled=true" >&2; exit 1; }
for tenant in parity-leaf parity-selfhosted parity-warm; do
  echo "$toml" | grep -F "[build_policy.\"$tenant\"]" >/dev/null || {
    echo "FAIL: gateway.toml missing [build_policy.\"$tenant\"] default entry: $toml" >&2
    exit 1
  }
done
# parity-leaf forces roots; the other two must not. keep_going and
# force_build_roots are the two lines immediately after each table header.
echo "$toml" | grep -A2 -F '[build_policy."parity-leaf"]' | grep 'force_build_roots = true' >/dev/null || {
  echo "FAIL: parity-leaf default must have force_build_roots = true: $toml" >&2
  exit 1
}
for tenant in parity-selfhosted parity-warm; do
  echo "$toml" | grep -A2 -F "[build_policy.\"$tenant\"]" | grep 'force_build_roots = false' >/dev/null || {
    echo "FAIL: $tenant default must have force_build_roots = false: $toml" >&2
    exit 1
  }
  echo "$toml" | grep -A2 -F "[build_policy.\"$tenant\"]" | grep 'keep_going = true' >/dev/null || {
    echo "FAIL: $tenant default must have keep_going = true: $toml" >&2
    exit 1
  }
done

# Gateway mounts gateway.toml whenever parity.enabled is set, even with an
# empty explicit gateway.buildPolicy map (the gate is `or`).
yq 'select(.kind=="Deployment" and .metadata.name=="rio-gateway")
    | .spec.template.spec.containers[0].volumeMounts[]
    | select(.mountPath=="/etc/rio/gateway.toml") | .subPath' "$on" |
  grep -x gateway.toml >/dev/null || {
  echo "FAIL: gateway missing the /etc/rio/gateway.toml subPath mount" >&2
  exit 1
}

# An operator-supplied entry must override the parity default (merge
# semantics: explicit values win).
ovr=$TMPDIR/parity-override.yaml
helm template rio . \
  --set global.image.tag=test \
  --set parity.enabled=true \
  --set gateway.buildPolicy.parity-leaf.keepGoing=false \
  --set gateway.buildPolicy.parity-leaf.forceBuildRoots=true \
  >"$ovr"
yq 'select(.kind=="ConfigMap" and .metadata.name=="rio-gateway-config") | .data."gateway.toml"' "$ovr" |
  grep -A2 -F '[build_policy."parity-leaf"]' | grep 'keep_going = false' >/dev/null || {
  echo "FAIL: explicit gateway.buildPolicy entry did not override the parity default" >&2
  exit 1
}

# Negative: the default render carries no parity bits. (The gateway.toml
# ConfigMap itself MAY exist by default for non-parity entries, e.g. the
# qa-* ones — only the parity strings must be absent.)
off=$TMPDIR/parity-off.yaml
helm template rio . --set global.image.tag=test >"$off"
! grep -q 'rio-parity\|parity-leaf\|parity-selfhosted\|parity-warm' "$off" || {
  echo "FAIL: parity enablement rendered with parity.enabled=false (default)" >&2
  grep -n 'rio-parity\|parity-leaf\|parity-selfhosted\|parity-warm' "$off" >&2
  exit 1
}
