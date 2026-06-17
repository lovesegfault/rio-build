# rio-mountd DaemonSet (ADR-022 P0567). mountd.enabled defaults to
# false, so 01-lint-render's profile renders never validate this
# template — render it explicitly here and pin the invariants that are
# load-bearing for the privilege boundary rather than cosmetic.

out=$TMPDIR/mountd.yaml
helm template rio . \
  --set mountd.enabled=true \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$out"

ds() {
  yq -N "select(.kind==\"DaemonSet\" and .metadata.name==\"rio-mountd\") | $1" "$out"
}

test "$(ds .kind)" = "DaemonSet" || {
  echo "FAIL: mountd.enabled=true did not render a rio-mountd DaemonSet" >&2
  exit 1
}

# PSA: the pod is privileged; rio-system is enforce=restricted and
# would reject it at admission. Must land in the builders namespace.
test "$(ds .metadata.namespace)" = "rio-builders" || {
  echo "FAIL: rio-mountd namespace=$(ds .metadata.namespace) (want rio-builders — rio-system is PSA-restricted)" >&2
  exit 1
}

# The FUSE mounts mountd creates under /var/rio/castore/ must propagate
# to builder pods. Bidirectional mountPropagation is the mechanism, and
# the apiserver rejects it on non-privileged containers — these two
# fields are a pair. Losing either silently breaks every build (the
# castore lower is an empty dir from the builder's point of view).
prop=$(ds '.spec.template.spec.containers[0].volumeMounts[] | select(.name=="var-rio") | .mountPropagation')
test "$prop" = "Bidirectional" || {
  echo "FAIL: /var/rio mountPropagation=$prop (want Bidirectional) — FUSE mounts stay invisible to builder pods" >&2
  exit 1
}
test "$(ds '.spec.template.spec.containers[0].securityContext.privileged')" = "true" || {
  echo "FAIL: rio-mountd not privileged — apiserver rejects Bidirectional mountPropagation on non-privileged containers" >&2
  exit 1
}

# Builder and fetcher nodes are both tainted; missing a toleration
# means that node class silently gets no mountd and every build on it
# fails at UDS connect.
for key in rio.build/builder rio.build/fetcher rio.build/kvm; do
  got=$(ds ".spec.template.spec.tolerations[] | select(.key==\"$key\") | .key")
  test "$got" = "$key" || {
    echo "FAIL: rio-mountd missing toleration for $key — those nodes get no mountd" >&2
    exit 1
  }
done

# The rio-builder image ships no shell and no coreutils, so an exec
# probe has nothing to execve — it fails forever and the pod never
# becomes Ready (which also wedges a RollingUpdate after one node).
# Any probe on this container must be tcpSocket/httpGet/grpc.
execprobes=$(ds '.spec.template.spec.containers[0] | (.livenessProbe.exec, .readinessProbe.exec, .startupProbe.exec) | select(. != null)')
test -z "$execprobes" || {
  echo "FAIL: rio-mountd has an exec probe — the image has no shell/coreutils to exec, the probe can never succeed: $execprobes" >&2
  exit 1
}

# A privileged pod must not also carry an API token it never uses —
# that turns a mountd compromise into a k8s API foothold for free.
test "$(ds '.spec.template.spec.automountServiceAccountToken')" = "false" || {
  echo "FAIL: rio-mountd automountServiceAccountToken != false — privileged pod gets an unused API token" >&2
  exit 1
}

# The most privileged pod in the cluster must not be the only one
# without a network policy. Cilium default-allows unpoliced endpoints,
# so a missing CNP means unrestricted egress (IMDS, apiserver, world)
# from a privileged root pod. mountd needs zero egress.
np=$TMPDIR/mountd-np.yaml
helm template rio . \
  --set mountd.enabled=true \
  --set networkPolicy.enabled=true \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$np"
denied=$(yq -N 'select(.kind=="CiliumNetworkPolicy" and .metadata.name=="rio-mountd") | .spec.egressDeny[0].toEntities[0]' "$np")
test "$denied" = "all" || {
  echo "FAIL: no CiliumNetworkPolicy denying all egress for rio-mountd — privileged pod with unrestricted network" >&2
  exit 1
}

# Default profile must NOT render the DS (nothing dials the socket
# until P0559/P0560; an idle privileged DaemonSet is pure surface).
off=$TMPDIR/mountd-off.yaml
helm template rio . --set global.image.tag=test --set postgresql.enabled=false >"$off"
if yq -N -e 'select(.kind=="DaemonSet" and .metadata.name=="rio-mountd")' "$off" >/dev/null 2>&1; then
  echo "FAIL: rio-mountd DaemonSet renders with mountd.enabled=false" >&2
  exit 1
fi

echo "OK"
