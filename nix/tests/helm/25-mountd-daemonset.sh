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
# Exact capability sets, not just first-element: the whole point of
# this fragment is locking the privilege surface, and `add[0] ==
# SYS_ADMIN` is satisfied by `add: [SYS_ADMIN, SYS_PTRACE]`.
test "$(yq "$ds | .spec.template.spec.containers[0].securityContext.capabilities.add | join(\",\")" "$out")" = "SYS_ADMIN" || {
  echo "FAIL: rio-mountd container capabilities.add != [SYS_ADMIN] exactly" >&2
  exit 1
}
test "$(yq "$ds | .spec.template.spec.containers[0].securityContext.capabilities.drop | join(\",\")" "$out")" = "ALL" || {
  echo "FAIL: rio-mountd container capabilities.drop != [ALL] exactly" >&2
  exit 1
}
# Explicit runAsUser 0: the rio-builder image has no config.User, so
# omitting this would still run as root today — but a future image
# User field (or a copy-paste of rio.podSecurityContext's 65532) would
# silently break the chown/0444-publication paths.
test "$(yq "$ds | .spec.template.spec.securityContext.runAsUser" "$out")" = "0" || {
  echo "FAIL: rio-mountd pod runAsUser != 0" >&2
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
# Capture-then-grep (not `yq | grep -q`): under the driver's pipefail,
# grep -q exiting at first match can SIGPIPE yq and turn a MATCH into a
# 141 pipeline failure — the inverse of the r39 pass-gap, a false FAIL.
args=$(yq "$ds | .spec.template.spec.containers[0].args[]" "$out")
grep -qx -- "--allowed-gid=990" <<<"$args" || {
  echo "FAIL: rio-mountd --allowed-gid != 990 (must match users.groups.rio-builder.gid in nix/nixos-node/eks-node.nix)" >&2
  exit 1
}

# Renders into the PSA-privileged builders namespace — restricted/
# baseline namespaces reject CAP_SYS_ADMIN + hostPath at admission.
test "$(yq "$ds | .metadata.namespace" "$out")" = "rio-builders" || {
  echo "FAIL: rio-mountd DaemonSet not in the rio-builders (PSA privileged) namespace" >&2
  exit 1
}

# The socket volume is the dedicated /run/rio-mountd directory, never
# the host's entire /run — that would hand a CAP_SYS_ADMIN pod the
# containerd/systemd/dbus control sockets. Both directions: the named
# volume points at the right path, AND no volume (whatever its name)
# mounts /run itself.
test "$(yq "$ds | .spec.template.spec.volumes[] | select(.name==\"run-rio-mountd\") | .hostPath.path" "$out")" = "/run/rio-mountd" || {
  echo "FAIL: rio-mountd socket volume hostPath.path != /run/rio-mountd" >&2
  exit 1
}
# Capture-then-grep (not `yq | grep -q`): in the `if pipeline; then
# FAIL` direction a producer SIGPIPE under pipefail silently skips the
# FAIL — the r39 pass-gap shape from 02-monitoring-kinds.sh.
paths=$(yq "$ds | .spec.template.spec.volumes[].hostPath.path" "$out")
if grep -qx "/run" <<<"$paths"; then
  echo "FAIL: rio-mountd mounts the host's entire /run — narrow it to /run/rio-mountd" >&2
  exit 1
fi

# Builder nodes carry rio.build/builder=true:NoSchedule, fetcher nodes
# rio.build/fetcher=true:NoSchedule, metal nodes rio.build/kvm. Some
# rendered toleration must cover each of them (today the blanket
# `operator: Exists` does — this guards against someone narrowing it);
# a node class without a matching toleration gets no mountd and every
# build scheduled onto it fails at UDS connect. Capture-then-test, not
# a `[...] | length` collect: yq's collect emits a per-document line
# even for documents the select filtered out.
for key in rio.build/builder rio.build/fetcher rio.build/kvm; do
  covered=$(yq "$ds | .spec.template.spec.tolerations[] | select((.key == \"$key\") or (.operator == \"Exists\" and (.key == null or .key == \"$key\")))" "$out")
  test -n "$covered" || {
    echo "FAIL: no rio-mountd toleration covers the $key taint — those nodes get no mountd and every build on them fails at UDS connect" >&2
    exit 1
  }
done

# A root + CAP_SYS_ADMIN pod must not also carry an API token it never
# uses — that would turn a mountd compromise into a k8s API foothold.
test "$(yq "$ds | .spec.template.spec.automountServiceAccountToken" "$out")" = "false" || {
  echo "FAIL: rio-mountd automountServiceAccountToken != false — privileged pod gets an unused API token" >&2
  exit 1
}

# No readiness probe exists, so minReadySeconds is what stops a
# crash-looping image from rolling across the whole fleet one node at a
# time (liveness restarts do not pause a rollout).
test "$(yq "$ds | .spec.minReadySeconds" "$out")" = "30" || {
  echo "FAIL: rio-mountd DaemonSet minReadySeconds != 30 — a crash-looping image rolls across the fleet unimpeded" >&2
  exit 1
}

# The rio-builder image ships no shell or coreutils, so an exec probe
# has nothing to execve — it would fail forever and (with
# minReadySeconds) wedge the rollout after one node. Probes on this
# container must be httpGet/tcpSocket/grpc.
execprobes=$(yq "$ds | .spec.template.spec.containers[0] | (.livenessProbe.exec, .readinessProbe.exec, .startupProbe.exec) | select(. != null)" "$out")
test -z "$execprobes" || {
  echo "FAIL: rio-mountd has an exec probe — the image has no shell/coreutils to exec, the probe can never succeed: $execprobes" >&2
  exit 1
}

# The most privileged pod in the cluster must not be the only one
# without a network policy: Cilium default-allows unpoliced endpoints,
# so losing the CNP means unrestricted egress (IMDS, apiserver, world)
# from a root + CAP_SYS_ADMIN pod whose only legitimate network surface
# is the :9095 metrics exporter. This is currently the only CI guard on
# any CiliumNetworkPolicy in the chart — keep it until a dedicated CNP
# fragment exists.
cnp='select(.kind=="CiliumNetworkPolicy" and .metadata.name=="rio-mountd")'
test "$(yq "$cnp | .spec.egressDeny[0].toEntities[0]" "$out")" = "all" || {
  echo "FAIL: rio-mountd CiliumNetworkPolicy does not deny all egress — privileged pod with unrestricted network" >&2
  exit 1
}
# Every ingress port across every rule, one per line: anything other
# than the single 9095 metrics port is a widened ingress surface.
test "$(yq "$cnp | .spec.ingress[].toPorts[].ports[].port" "$out")" = "9095" || {
  echo "FAIL: rio-mountd CiliumNetworkPolicy ingress is not exactly the :9095 metrics port" >&2
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
