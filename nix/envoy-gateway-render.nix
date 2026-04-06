# Render the Envoy Gateway operator chart for airgapped k3s VM tests.
#
# Same pattern as helm-render.nix (template → split → k3s manifests)
# but for the upstream gateway-helm chart. Output is a directory with
# numbered YAML files that k3s applies in filename order:
#
#   00-envoy-gateway-crds.yaml  Gateway API + gateway.envoyproxy.io CRDs
#   01-envoy-gateway-ns.yaml    envoy-gateway-system Namespace
#   02-envoy-gateway-rbac.yaml  ServiceAccount / ClusterRole / Bindings
#   03-envoy-gateway.yaml       Deployment / Service / ConfigMap / certgen
#
# Installed BEFORE the rio chart so the GatewayClass/Gateway/GRPCRoute
# CRDs exist when 02-rio-workloads.yaml is applied. k3s manifest
# ordering is filename-alphabetical, so the `00-envoy-gateway-` prefix
# sorts before `00-rio-crds` (e < r).
#
# Airgap: the chart's certgen Job and Deployment both pull
# docker.io/envoyproxy/gateway:v1.7.1. The envoy data-plane image
# (envoy:distroless-v1.37.1) is NOT in the chart — it's compiled into
# the operator binary as a default and injected into the envoy
# Deployment at reconcile time. The rio chart's dashboard-gateway-
# tls.yaml pins it via EnvoyProxy.spec.provider.kubernetes.
# envoyDeployment.container.image so preloading can target a known
# version.
{
  pkgs,
  nixhelm,
  system,
}:
let
  subcharts = import ./helm-charts.nix { inherit pkgs nixhelm system; };
  chart = subcharts.gateway-helm;
in
pkgs.runCommand "envoy-gateway-rendered"
  {
    nativeBuildInputs = [
      pkgs.kubernetes-helm
      pkgs.yq-go
    ];
  }
  ''
    mkdir -p $out

    # ── CRDs ───────────────────────────────────────────────────────────
    # gateway-helm's crds/ has both Gateway API (gatewayapi-crds.yaml,
    # ~20K lines) and Envoy Gateway extension CRDs
    # (crds/generated/gateway.envoyproxy.io_*.yaml). Concat with explicit
    # separators — some generated files don't end with ---.
    for f in ${chart}/crds/gatewayapi-crds.yaml ${chart}/crds/generated/*.yaml; do
      cat "$f"
      echo "---"
    done > $out/00-envoy-gateway-crds.yaml

    # ── Operator ───────────────────────────────────────────────────────
    # --namespace envoy-gateway-system (chart's default, also its
    # hardcoded lease/configmap namespace references). createNamespace=
    # true so the chart renders the Namespace object (k3s manifests
    # don't have a --create-namespace flag; we need the object in the
    # YAML). The GatewayClass controller watches all namespaces by
    # default.
    helm template envoy-gateway ${chart} \
      --namespace envoy-gateway-system \
      --set createNamespace=true > all.yaml

    # Namespace MUST come first (separate file, sorts before rbac).
    yq 'select(.kind == "Namespace")' all.yaml > $out/01-envoy-gateway-ns.yaml

    yq 'select(.kind == "ServiceAccount" or
               .kind == "ClusterRole" or
               .kind == "ClusterRoleBinding" or
               .kind == "Role" or
               .kind == "RoleBinding")' all.yaml > $out/02-envoy-gateway-rbac.yaml

    yq 'select(.kind != "Namespace" and
               .kind != "ServiceAccount" and
               .kind != "ClusterRole" and
               .kind != "ClusterRoleBinding" and
               .kind != "Role" and
               .kind != "RoleBinding")' all.yaml > $out/03-envoy-gateway.yaml

    # Sanity: non-empty
    for f in $out/*.yaml; do
      test -s "$f" || { echo "rendered file $f is empty" >&2; exit 1; }
    done
  ''
