# P0567 rio-mountd DaemonSet security posture + placement.
#
# The mountd pod is the trust boundary between unprivileged builders
# and the node's shared cache: it runs as root with CAP_SYS_ADMIN on
# every builder/fetcher node. The exact securityContext shape is
# load-bearing —
#   - `privileged: false` + `capabilities.add: [SYS_ADMIN]`: privileged
#     would disable seccomp and expose every host device; anything less
#     than SYS_ADMIN cannot mount(2)/FUSE_DEV_IOC_BACKING_OPEN/quotactl.
#   - seccomp RuntimeDefault at the pod level.
#   - node affinity restricted to builder/fetcher node-roles: a mountd
#     pod on a general (control-plane) node is attack surface with no
#     consumer.
# A "simplification" of any of these is a security regression that
# would otherwise only surface in a live-cluster audit.

out=$TMPDIR/mountd.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$out"

ds='select(.kind=="DaemonSet" and .metadata.name=="rio-mountd")'

test "$(yq "$ds | .spec.template.spec.containers[0].securityContext.privileged" "$out")" = "false" || {
  echo "FAIL: rio-mountd container privileged != false" >&2
  exit 1
}
test "$(yq "$ds | .spec.template.spec.containers[0].securityContext.capabilities.add[0]" "$out")" = "SYS_ADMIN" || {
  echo "FAIL: rio-mountd container capabilities.add[0] != SYS_ADMIN" >&2
  exit 1
}
test "$(yq "$ds | .spec.template.spec.containers[0].securityContext.capabilities.drop[0]" "$out")" = "ALL" || {
  echo "FAIL: rio-mountd container capabilities.drop[0] != ALL" >&2
  exit 1
}
test "$(yq "$ds | .spec.template.spec.securityContext.seccompProfile.type" "$out")" = "RuntimeDefault" || {
  echo "FAIL: rio-mountd pod seccompProfile.type != RuntimeDefault" >&2
  exit 1
}
test "$(yq "$ds | .spec.template.spec.affinity.nodeAffinity.requiredDuringSchedulingIgnoredDuringExecution.nodeSelectorTerms[0].matchExpressions[0].key" "$out")" = "rio.build/node-role" || {
  echo "FAIL: rio-mountd node affinity does not key on rio.build/node-role" >&2
  exit 1
}

# The --allowed-gid arg must agree with users.groups.rio-builder.gid in
# nix/nixos-node/eks-node.nix (990). The two sides of the SO_PEERCRED
# gate are declared in different languages in different trees — this is
# the only place they meet.
yq "$ds | .spec.template.spec.containers[0].args[]" "$out" | grep -qx -- "--allowed-gid=990" || {
  echo "FAIL: rio-mountd --allowed-gid != 990 (must match users.groups.rio-builder.gid in nix/nixos-node/eks-node.nix)" >&2
  exit 1
}

# Renders into the PSA-privileged builders namespace — restricted/
# baseline namespaces reject CAP_SYS_ADMIN + hostPath at admission.
test "$(yq "$ds | .metadata.namespace" "$out")" = "rio-builders" || {
  echo "FAIL: rio-mountd DaemonSet not in the rio-builders (PSA privileged) namespace" >&2
  exit 1
}

# mountd.enabled=false must drop the DaemonSet entirely.
off=$TMPDIR/mountd-off.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  --set mountd.enabled=false \
  >"$off"
test "$(yq "$ds | .metadata.name" "$off")" = "" || {
  echo "FAIL: mountd.enabled=false still renders the DaemonSet" >&2
  exit 1
}

echo "OK"
