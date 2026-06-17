# sh-028: rio-controller's gateway-cost annotator (reconcilers/
# gateway_cost.rs) stamps `pod-deletion-cost` on each gateway pod with
# its scraped `rio_gateway_connections_active`, so KEDA scale-down
# evicts the least-loaded replica. The annotator lives in the
# CONTROLLER (not the gateway) so the gateway keeps
# `automountServiceAccountToken: false` and gains no kube client / RBAC
# — that posture is the security-review invariant this fragment pins.
#
# r[verify ctrl.gateway.deletion-cost] (documentary — .sh is not in
# tracey test_include; the scannable verify lives on the unit test in
# rio-controller/src/reconcilers/gateway_cost.rs).

out=$TMPDIR/gw-dc.yaml
helm template rio . --set global.image.tag=test >"$out"

# Premise guard: both Deployments render in the default profile.
for d in rio-controller rio-gateway; do
  test -n "$(yq -N "select(.kind==\"Deployment\" and .metadata.name==\"$d\")" "$out")" || {
    echo "FAIL: $d Deployment did not render — gateway-deletion-cost assertions vacuous" >&2
    exit 1
  }
done

# Security invariant: the gateway SA stays automount:false (the chart
# only emits the field when false; absent ⇒ k8s default true ⇒ a kube
# client would init). A future "just give the gateway a kube client"
# regression flips this to true/absent — refuse it here.
auto=$(yq -N 'select(.kind=="ServiceAccount" and .metadata.name=="rio-gateway") | .automountServiceAccountToken' "$out")
test "$auto" = "false" || {
  echo "FAIL: rio-gateway SA lost automountServiceAccountToken: false — sh-028 placed the deletion-cost annotator in the controller PRECISELY so the gateway gains no kube client; do not flip this" >&2
  exit 1
}

# Enable gate: RIO_GATEWAY_NAMESPACE in the controller env from
# downward-API metadata.namespace (the single non-empty gate that
# spawns the annotator). Capture-then-grep — same SIGPIPE shape as
# 21-control-plane-readiness.
env_names=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-controller") | .spec.template.spec.containers[].env[]? | select(.valueFrom.fieldRef.fieldPath=="metadata.namespace") | .name' "$out")
grep -qx RIO_GATEWAY_NAMESPACE <<<"$env_names" || {
  echo "FAIL: rio-controller env missing RIO_GATEWAY_NAMESPACE from downward-API metadata.namespace — annotator not spawned (gateway_namespace empty)" >&2
  exit 1
}

# CNP egress: controller → gateway:9090. Without this rule the scrape
# is default-denied and the annotator silently degrades to no-op (it
# never crashes — that is by design, so CNP drift would be invisible
# at runtime).
egress=$(yq -N 'select(.kind=="CiliumNetworkPolicy" and .metadata.name=="rio-controller-egress") | .spec.egress[] | select(.toEndpoints[]?.matchLabels."app.kubernetes.io/name"=="rio-gateway") | .toPorts[].ports[].port' "$out")
grep -qx '9090' <<<"$egress" || {
  echo "FAIL: rio-controller-egress CNP has no rule for gateway:9090 — /metrics scrape default-denied; deletion-cost annotator silently no-ops" >&2
  exit 1
}

echo "OK"
