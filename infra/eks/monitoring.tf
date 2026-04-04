# kube-prometheus-stack: Prometheus + Alertmanager + Grafana + the
# operator. The rio chart's ServiceMonitor/PodMonitor/PrometheusRule
# templates (gated on monitoring.enabled, set by xtask deploy) are
# inert until this lands — the operator's CRDs define those types.
#
# helm_release here for the same reason as cert-manager/external-secrets:
# one `tofu apply`, terraform owns the lifecycle. The chart bundles its
# own CRDs (prometheus-operator's) via the crds/ dir — helm installs
# those on first `helm install` and never touches them on upgrade. A
# major bump that changes CRDs needs a manual `kubectl apply
# --server-side -f https://.../stripped-down-crds.yaml` first; see the
# chart's UPGRADING.md.

resource "helm_release" "kube_prometheus_stack" {
  name             = "kube-prometheus-stack"
  namespace        = "monitoring"
  create_namespace = true
  repository       = "https://prometheus-community.github.io/helm-charts"
  chart            = "kube-prometheus-stack"
  # Hardcoded (not nix/pins.nix) — same as external-secrets: not exercised
  # by VM tests, so no nix↔tofu pin to keep in sync. Bump alongside
  # kubernetes_version; check chart's kubeVersion constraint.
  version = "75.6.0"

  set = [
    # P0539b ships dashboards as ConfigMaps in rio-system labelled
    # `grafana_dashboard=1`. The sidecar watches for that label and
    # mounts the JSON into Grafana. searchNamespace=ALL because the
    # dashboards live in the rio chart's namespaces, not `monitoring`.
    {
      name  = "grafana.sidecar.dashboards.enabled"
      value = "true"
    },
    {
      name  = "grafana.sidecar.dashboards.searchNamespace"
      value = "ALL"
    },
    # By default the operator only picks up ServiceMonitors/PodMonitors/
    # PrometheusRules carrying `release: kube-prometheus-stack` (the
    # chart's own). Nil-uses-helm-values=false drops that filter so the
    # rio chart's monitors (which carry rio.labels, not the release
    # label) are scraped. Namespace discovery is already cluster-wide
    # (the chart sets serviceMonitorNamespaceSelector: {} by default).
    {
      name  = "prometheus.prometheusSpec.serviceMonitorSelectorNilUsesHelmValues"
      value = "false"
    },
    {
      name  = "prometheus.prometheusSpec.podMonitorSelectorNilUsesHelmValues"
      value = "false"
    },
    {
      name  = "prometheus.prometheusSpec.ruleSelectorNilUsesHelmValues"
      value = "false"
    },
  ]

  # aws_lbc dep: webhook-ordering only — see addons.tf cert_manager. The
  # mservice.elbv2.k8s.aws mutating webhook intercepts ALL Service
  # creates cluster-wide with failurePolicy=Fail; without serializing,
  # the chart's grafana/alertmanager/prometheus Services race the
  # webhook's pod-Ready and get "no endpoints available".
  depends_on = [module.eks, helm_release.aws_lbc]
}
