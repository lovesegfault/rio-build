# dashboard.enabled=true MUST render exactly one each of GatewayClass/
# Gateway/GRPCRoute (standard Gateway API only — Cilium controller, no
# vendor CRDs). Any Go-template syntax error, bad nindent, or Values
# typo surfaces here before the VM test spends 5min on k3s bring-up.

out=$TMPDIR/gateway-dash-on.yaml
helm template rio . \
  --set dashboard.enabled=true \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$out"

for k in GatewayClass Gateway GRPCRoute; do
  grep -qx "kind: $k" "$out" || {
    echo "FAIL: dashboard.enabled=true did not render kind: $k" >&2
    exit 1
  }
done

# Vendor CRDs MUST NOT render — D3 cascade dissolved EnvoyProxy/
# SecurityPolicy/ClientTrafficPolicy/BackendTLSPolicy into in-process
# code (tonic-web + tower-http/cors + h2 keepalive + connect-web retry).
for k in EnvoyProxy SecurityPolicy ClientTrafficPolicy BackendTLSPolicy BackendTrafficPolicy; do
  ! grep -qx "kind: $k" "$out" || {
    echo "FAIL: vendor CRD kind: $k rendered (should be gone post-D3)" >&2
    exit 1
  }
done

# Controller name must be Cilium's.
yq 'select(.kind=="GatewayClass") | .spec.controllerName' "$out" |
  grep -qx 'io.cilium/gateway-controller' || {
  echo "FAIL: GatewayClass.spec.controllerName is not io.cilium/gateway-controller" >&2
  exit 1
}
