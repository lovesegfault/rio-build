# KEDA: event-driven autoscaler. Installs the operator + the
# external.metrics.k8s.io aggregated API server + the ScaledObject/
# ScaledJob CRDs. The rio chart's gateway ScaledObject (templates/
# gateway-scaledobject.yaml, gated on gateway.autoscaling.enabled) is
# inert until this lands — same relationship as kube-prometheus-stack
# and the ServiceMonitor/PrometheusRule templates.
#
# Unconditional, like every addon here except external-dns (which needs
# zone config that may not exist). An installed-but-unused KEDA is two
# idle pods; the thing that actually changes scaling behavior is the
# ScaledObject CR, and that is gated off by default in the chart.
#
# helm_release here for the same reason as external-secrets: one
# `tofu apply`, terraform owns the lifecycle. Version hardcoded (not
# nix/pins.toml) — same as kube-prometheus-stack/external-secrets: not
# exercised by VM tests, so no nix↔tofu pin to keep in sync.

resource "helm_release" "keda" {
  name             = "keda"
  namespace        = "keda"
  create_namespace = true
  repository       = "https://kedacore.github.io/charts"
  chart            = "keda"
  version          = "2.19.0"

  set = [
    # hostNetwork: EKS managed API server can't route to overlay pod IPs
    # (Cilium cluster-pool fd42::) — see same comment in secrets.tf
    # external-secrets. For KEDA this hits TWO components:
    #
    # metricsServer is the aggregated v1beta1.external.metrics.k8s.io
    # API server the HPA controller reads metric values through (via
    # kube-apiserver proxying to the Service). Unreachable → every HPA
    # target shows <unknown> and nothing ever scales. This one is
    # load-bearing. Container port 6443 — nothing else binds it on an
    # EKS node (the kube-apiserver is off-node).
    {
      name  = "metricsServer.useHostNetwork"
      value = "true"
    },
    # The admission webhook only validates ScaledObjects
    # (failurePolicy=Ignore — unreachable just skips validation), but
    # leave it functional rather than silently dead. Port 10270: 9443
    # (chart default) collides with aws-lbc's hostNetwork webhook and
    # 10260 is taken by external-secrets/prometheus-operator on the
    # same system nodes.
    {
      name  = "webhooks.useHostNetwork"
      value = "true"
    },
    {
      name  = "webhooks.port"
      value = "10270"
    },
  ]

  # aws_lbc dep: webhook-ordering only — see addons.tf aws_lbc.
  # cilium dep: CNI must be up or pods Pending → wait=true times out.
  # No dep on kube_prometheus_stack: the prometheus trigger is a
  # runtime query that retries, not an install-time CRD requirement.
  depends_on = [helm_release.aws_lbc, helm_release.cilium]
}
