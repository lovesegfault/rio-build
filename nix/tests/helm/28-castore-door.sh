# External castore door (ADR-024 P2) render assertions.
#
# The door is routing-only — auth lives in the rio-store services
# (rio-store/tests/grpc/external_door.rs proves anonymous rejection +
# tenant-JWT round-trip on every routed RPC family). What the chart
# must guarantee is the ROUTED SURFACE: castore services whole,
# StoreService restricted to exactly PutPathChunked. A bare
# StoreService match (no method) would fail-open the whole-NAR /
# query surface to the internet.

# Fail-closed: default values render none of the door resources.
def=$TMPDIR/door-off.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$def"
! yq -N 'select(.kind=="GRPCRoute") | .metadata.name' "$def" |
  grep -x rio-castore >/dev/null || {
  echo "FAIL: rio-castore GRPCRoute rendered with default values (externalDoor should default off)" >&2
  exit 1
}

out=$TMPDIR/door-on.yaml
helm template rio . \
  --set store.externalDoor.enabled=true \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$out"

# All four resources render (GatewayClass/Gateway/GRPCRoute/ReferenceGrant).
for k in GatewayClass Gateway GRPCRoute ReferenceGrant; do
  yq -N "select(.kind==\"$k\") | .metadata.name" "$out" |
    grep -e '^rio-castore' >/dev/null || {
    echo "FAIL: externalDoor.enabled=true did not render a rio-castore $k" >&2
    exit 1
  }
done

# Cilium controller, like every other Gateway in the chart.
yq -N 'select(.kind=="GatewayClass" and .metadata.name=="rio-castore") | .spec.controllerName' "$out" |
  grep -x 'io.cilium/gateway-controller' >/dev/null || {
  echo "FAIL: rio-castore GatewayClass controllerName is not io.cilium/gateway-controller" >&2
  exit 1
}

matches() {
  yq -N 'select(.kind=="GRPCRoute" and .metadata.name=="rio-castore") | .spec.rules[].matches[].method | (.service + "/" + (.method // "*"))' "$out"
}

# The castore services route whole (negotiation + retrieval surface).
for svc in rio.store.DirectoryService rio.store.ChunkService rio.store.DrvBlobService; do
  matches | grep -x "$svc/\*" >/dev/null || {
    echo "FAIL: GRPCRoute missing whole-service match for $svc" >&2
    exit 1
  }
done

# StoreService routes EXACTLY PutPathChunked — a method-less match or
# any extra method fail-opens cluster-internal RPCs to the internet.
matches | grep -x 'rio.store.StoreService/PutPathChunked' >/dev/null || {
  echo "FAIL: GRPCRoute missing StoreService/PutPathChunked match (upload path unrouted)" >&2
  exit 1
}
extra=$(matches | grep '^rio.store.StoreService/' | grep -vx 'rio.store.StoreService/PutPathChunked' || true)
test -z "$extra" || {
  echo "FAIL: GRPCRoute routes StoreService beyond PutPathChunked: $extra" >&2
  exit 1
}

# Backend is the store ClusterIP service on its gRPC port, cross-ns.
yq -N 'select(.kind=="GRPCRoute" and .metadata.name=="rio-castore") | .spec.rules[].backendRefs[] | (.name + ":" + (.port|tostring) + "@" + .namespace)' "$out" |
  grep -x 'rio-store:9002@rio-store' >/dev/null || {
  echo "FAIL: GRPCRoute backendRef is not rio-store:9002 in the store namespace" >&2
  exit 1
}

# The cross-ns backendRef needs the ReferenceGrant in the STORE
# namespace, granting GRPCRoute-from-system → Service rio-store;
# without it the route silently goes ResolvedRefs=False.
yq -N 'select(.kind=="ReferenceGrant") | .metadata.namespace + " " + .spec.from[0].kind + " " + .spec.to[0].name' "$out" |
  grep -x 'rio-store GRPCRoute rio-store' >/dev/null || {
  echo "FAIL: ReferenceGrant not in store namespace granting GRPCRoute → Service rio-store" >&2
  exit 1
}

echo "OK"
