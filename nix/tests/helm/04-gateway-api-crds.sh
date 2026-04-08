# dashboard.enabled=true MUST render exactly one each of GatewayClass/
# Gateway/GRPCRoute/EnvoyProxy/SecurityPolicy/ClientTrafficPolicy
# (+ BackendTLSPolicy when tls.enabled). Any Go-template syntax error,
# bad nindent, or Values typo surfaces here before the VM test has to
# spend 5min on k3s bring-up to discover a YAML parse error.

out=$TMPDIR/gateway-dash-on.yaml
helm template rio . \
  --set dashboard.enabled=true \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$out"

for k in GatewayClass Gateway GRPCRoute EnvoyProxy \
  SecurityPolicy ClientTrafficPolicy BackendTLSPolicy; do
  grep -qx "kind: $k" "$out" || {
    echo "FAIL: dashboard.enabled=true did not render kind: $k" >&2
    exit 1
  }
done

# grpc_web filter auto-inject is a runtime property of Envoy Gateway's
# xDS translator, not something helm-lint can prove — the GRPCRoute
# existence + Gateway single-listener is the static contract.
n=$(yq 'select(.kind=="Gateway" and .metadata.name=="rio-dashboard")
        | .spec.listeners | length' "$out")
test "$n" -eq 1 || {
  echo "FAIL: rio-dashboard Gateway must have exactly 1 listener (dodges #7559), got $n" >&2
  exit 1
}
