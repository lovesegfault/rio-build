# KEDA: event-driven autoscaler. Installs the operator + the
# external.metrics.k8s.io aggregated API server + the ScaledObject/
# ScaledJob CRDs. The rio chart's gateway ScaledObject (templates/
# gateway-scaledobject.yaml, gated on gateway.autoscaling.enabled) is
# inert until this lands — same relationship as kube-prometheus-stack
# and the ServiceMonitor/PrometheusRule templates.
#
# Unconditional, like every addon here except external-dns (which needs
# zone config that may not exist). Not an idle addon either: the chart
# defaults gateway.autoscaling.enabled=true (only the dev/k3s VM-test
# overlays, which run no KEDA, switch it off), so every EKS deploy
# ships the gateway ScaledObject and its replica count is owned by
# KEDA — a broken KEDA means the gateway never scales past its floor.
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
    # hostNetwork changes DNS resolution too: under the default
    # ClusterFirst policy a hostNetwork pod resolves via the node's
    # resolv.conf (VPC resolver), which can't resolve cluster-DNS
    # names — and the adapter forwards every external-metrics query to
    # the operator's gRPC service by name (--metrics-service-address=
    # keda-operator.keda.svc.cluster.local:9666). Unresolvable → every
    # HPA external metric is <unknown> and nothing ever scales, same
    # blast radius as an unreachable adapter. ClusterFirstWithHostNet
    # keeps kube-dns resolution while on the host network. The webhooks
    # pod needs no equivalent (the chart exposes none): it only serves
    # the API server and reaches kube-api via the env-injected service
    # IP, no cluster-DNS lookups.
    {
      name  = "metricsServer.dnsPolicy"
      value = "ClusterFirstWithHostNet"
    },
    # On a hostNetwork pod every listener is a host port, not just the
    # serving port. Besides 6443 the adapter always runs a plain-HTTP
    # metrics listener (prometheus.metricServer.port — declared as a
    # containerPort, so the scheduler reserves it even with scraping
    # disabled). Its 8080 default is taken on the system nodes by the
    # aws-lbc metrics endpoint, so move it into the 1027x block holding
    # KEDA's auxiliary listeners (just above kubelet's 10250 and
    # prometheus-operator's 10260).
    # Sitting outside the 9443-10260 control-plane→node rule is
    # fine here: only kubelet probes and in-cluster scrapes hit these,
    # never the API server. Health probes need no extra port — the
    # adapter serves /healthz on 6443 itself.
    {
      name  = "prometheus.metricServer.port"
      value = "10271"
    },
    # The admission webhook only validates ScaledObjects
    # (failurePolicy=Ignore — unreachable just skips validation), but
    # leave it functional rather than silently dead. The serving port
    # must sit inside the 9443-10260 control-plane→node SG rule
    # (main.tf webhooks_from_control_plane) — outside it the API server
    # can't connect and every ScaledObject write (each helm upgrade,
    # now that gateway autoscaling defaults on) waits out the webhook
    # timeout and is admitted unvalidated. Port 9444: 9443 (chart
    # default) collides with aws-lbc's hostNetwork webhook one port
    # down, and 10260 is taken by prometheus-operator (ESO holds
    # 9445-9447 — see the main.tf allocation table).
    {
      name  = "webhooks.useHostNetwork"
      value = "true"
    },
    {
      name  = "webhooks.port"
      value = "9444"
    },
    # Same port-namespace problem for the webhook pod's auxiliary
    # listeners: controller-runtime always binds its metrics endpoint
    # (prometheus.webhooks.port, default 8080) and health probe
    # (webhooks.healthProbePort, default 8081) regardless of whether
    # anything scrapes them; 8080 is taken on the system nodes by
    # aws-lbc's hostNetwork metrics endpoint, and staying in the 1027x
    # block keeps both clear of such common defaults. Distinct from the
    # apiserver's 10271 so the two KEDA pods can share a node.
    {
      name  = "prometheus.webhooks.port"
      value = "10272"
    },
    {
      name  = "webhooks.healthProbePort"
      value = "10273"
    },
  ]

  # aws_lbc dep: webhook-ordering only — see addons.tf aws_lbc.
  # cilium dep: CNI must be up or pods Pending → wait=true times out.
  # No dep on kube_prometheus_stack: the prometheus trigger is a
  # runtime query that retries, not an install-time CRD requirement.
  depends_on = [helm_release.aws_lbc, helm_release.cilium]
}
