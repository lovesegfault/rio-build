# metrics-server: the v1beta1.metrics.k8s.io aggregated API server —
# the resource-metrics backend the HPA path (and therefore KEDA's
# `type: cpu` trigger on the store ScaledObject) reads. live_044: the
# cluster never had one — k3s embeds its own metrics-server, which is
# exactly why this gap only surfaced live on EKS: the cpu trigger
# reported FailedGetResourceMetric on pods.metrics.k8s.io and the dead
# axis impeded HPA scale-DOWN (cpu never reports, so the HPA cannot
# conclude low utilization).
#
# helm_release for the same reason as every non-bootstrap addon here
# (keda/kube-prometheus-stack/external-secrets): one `tofu apply`,
# terraform owns the lifecycle. Version hardcoded (not nix/pins.toml)
# — same as keda.tf: not exercised by VM tests (k3s brings its own),
# so no nix↔tofu pin to keep in sync. Re-check the chart's supported
# k8s window on every pins.toml [cluster].kubernetes_version bump,
# same as the chart-version comments in keda.tf/monitoring.tf.
#
# REACHABILITY (the load-bearing values — main.tf:332 law): the EKS
# managed apiserver dials the aggregated API's endpoints directly and
# CANNOT reach Cilium cluster-pool overlay IPs — a stock (pod-network)
# install lands DEAD with the exact live FailedGetResourceMetric
# symptom persisting. Like every apiserver-dialed backend here
# (webhooks in keda.tf/secrets.tf/monitoring.tf), metrics-server runs
# hostNetwork=true; containerPort moves 10250 → 9448 (10250 collides
# with the kubelet under hostNetwork; 9448 = the first free port
# >= 9448 per the canonical host-port table's own instruction, inside
# the 9443..10260 webhooks_from_control_plane window the
# apiserver-dialed backend must sit in). Row added to the main.tf
# table.
#
# Ordering: depends_on cilium only (CNI up or wait=true times out —
# the edge every addon carries). No edge to KEDA in either direction:
# the cpu scaler degrades gracefully until metrics flow (a runtime
# query that retries — NOT the merged_bug_086 shape: this chart
# renders no monitoring.coreos.com objects; if it ever gains a
# ServiceMonitor value, it gains the kube_prometheus_stack edge with
# it, per the keda.tf ordering law).

resource "helm_release" "metrics_server" {
  name             = "metrics-server"
  namespace        = "kube-system"
  create_namespace = false
  repository       = "https://kubernetes-sigs.github.io/metrics-server"
  chart            = "metrics-server"
  version          = "3.13.0"

  set = [
    {
      name  = "hostNetwork.enabled"
      value = "true"
    },
    {
      # --secure-port follows containerPort in this chart; the
      # APIService (v1beta1.metrics.k8s.io) is chart-managed.
      name  = "containerPort"
      value = "9448"
    },
  ]

  depends_on = [helm_release.cilium]
}
