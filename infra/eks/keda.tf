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
# exercised by VM tests, so no nix↔tofu pin to keep in sync. The pin
# IS coupled to pins.toml [cluster].kubernetes_version, just not
# mechanically: KEDA tests against a 3-minor k8s window per release
# (https://keda.sh/docs/<ver>/operate/cluster/ compat matrix —
# re-check on every kubernetes_version bump, same as monitoring.tf).
# 2.20.x = 1.33-1.35 tested; the 1.36 control plane is one minor past
# that window. 2.20.1 is the latest release (2.21 = TBD upstream as of
# 2026-06) and the chart's kubeVersion floor is `>=1.23`, no ceiling —
# KEDA's surface here (external.metrics.k8s.io v1beta1, ScaledObject
# CRD, prometheus scaler) has no 1.36 removal that touches it.
# TODO: bump to 2.21.x (expected 1.34-1.36 tested) once kedacore cuts
# the release.

resource "helm_release" "keda" {
  name             = "keda"
  namespace        = "keda"
  create_namespace = true
  repository       = "https://kedacore.github.io/charts"
  chart            = "keda"
  version          = "2.20.1"

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
    # Operator self-metrics: the scaler-health surface for our two
    # ScaledObjects — keda_scaler_detail_errors_total (prometheus
    # unreachable → the gateway SO's spec.fallback engages),
    # keda_scaler_empty_upstream_responses_total (sum() over an absent
    # series scales toward the floor with no error anywhere else), the
    # 2.20 keda_scaler_http_* query outcome/latency metrics, and
    # keda_scaler_active (correct for cpu triggers since 2.20). The
    # store dashboard's scaling row reads these
    # (infra/helm/rio-build/dashboards/store.json). serviceMonitor
    # because kube-prometheus-stack watches ServiceMonitors
    # cluster-wide (monitoring.tf sets
    # serviceMonitorSelectorNilUsesHelmValues=false and namespace
    # discovery is unrestricted). The operator pod is NOT hostNetwork
    # (only the metrics-apiserver and webhooks above are), so its
    # metrics port stays a pod-network containerPort (chart default
    # 8080) — no row in the main.tf host-port allocation table.
    {
      name  = "prometheus.operator.enabled"
      value = "true"
    },
    {
      name  = "prometheus.operator.serviceMonitor.enabled"
      value = "true"
    },
  ]

  # aws_lbc dep: webhook-ordering only — see addons.tf aws_lbc.
  # cilium dep: CNI must be up or pods Pending → wait=true times out.
  # kube_prometheus_stack dep (merged_bug_086): enabling ANY
  # monitoring.coreos.com object in a release's values IS an
  # install-time CRD requirement on kube_prometheus_stack — the sole
  # installer of the prometheus-operator CRDs. The two operator
  # ServiceMonitor values above render a ServiceMonitor object
  # unconditionally (KEDA 2.20.1 gates only on the values, no
  # Capabilities guard), and without this edge terraform schedules
  # the two releases concurrently on a fresh apply (identical parent
  # sets): keda winning the race fails with "resource mapping not
  # found ... kind ServiceMonitor". Hidden in steady-state upgrades
  # (the CRD already exists); bites greenfield bring-up and xtask
  # destroy/recreate. The PROMETHEUS TRIGGER half of the old comment
  # stays true — that is a runtime query that retries — but rendered
  # monitoring objects are not triggers. Next values-add: any new
  # monitoring.coreos.com enable in ANY release carries this same
  # edge (this comment is the in-file contract).
  depends_on = [
    helm_release.aws_lbc,
    helm_release.cilium,
    helm_release.kube_prometheus_stack,
  ]
}
