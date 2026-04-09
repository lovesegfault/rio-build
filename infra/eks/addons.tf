# Cluster addons installed via Helm: cilium + aws-load-balancer-
# controller. Both are prerequisites for the rio chart — cilium is the
# CNI (nodes are NotReady until it lands; v21 bootstrap_self_managed_
# addons=false means NO vpc-cni fallback), and aws-lbc provisions the
# gateway NLB.
#
# helm_release here (not a separate `helm install` step) keeps
# everything in one `tofu apply`. The tradeoff: terraform now owns
# the Helm release lifecycle — `helm upgrade` outside terraform
# drifts state. For cluster-foundational addons that rarely change,
# this is fine.

# ============================================================
# Gateway API CRDs (C1: must exist BEFORE helm_release.cilium)
# ============================================================
#
# Cilium chart's `gatewayAPI.enabled=true` is gated on
# `.Capabilities.APIVersions.Has "gateway.networking.k8s.io/v1"` —
# if the CRDs aren't installed first, the feature silently no-ops
# (no Gateway controller, no GatewayClass). The chart does NOT ship
# these CRDs itself.
#
# Standard-channel install (GatewayClass, Gateway, HTTPRoute,
# GRPCRoute, ReferenceGrant). Pinned via nix/pins.nix → tfvars.

data "http" "gateway_api_crds" {
  url = "https://github.com/kubernetes-sigs/gateway-api/releases/download/${var.gateway_api_version}/standard-install.yaml"
}

data "kubectl_file_documents" "gateway_api_crds" {
  content = data.http.gateway_api_crds.response_body
}

resource "kubectl_manifest" "gateway_api_crds" {
  for_each  = data.kubectl_file_documents.gateway_api_crds.manifests
  yaml_body = each.value

  # server_side_apply: CRDs are large; client-side apply hits the
  # last-applied-configuration annotation size limit.
  server_side_apply = true

  depends_on = [module.eks]
}

# ============================================================
# cilium
# ============================================================
#
# Root of the helm_release depends_on chain. Until cilium's DaemonSet
# is Ready, system nodes are NotReady → every other helm_release's
# pods are Pending → their wait=true times out. So this MUST install
# first. The DaemonSet tolerates node.kubernetes.io/not-ready (it's
# the thing that makes nodes Ready).

resource "helm_release" "cilium" {
  name       = "cilium"
  namespace  = "kube-system"
  repository = "https://helm.cilium.io"
  chart      = "cilium"
  # Pinned via nix/pins.nix → generated.auto.tfvars.json. Same pin as
  # nix/cilium-render.nix (k3s VM tests). 1.19+ required: IPv6 tunnel
  # underlay first-class only since PR #40324.
  version = var.cilium_version

  values = [
    yamlencode({
      # Cluster-pool IPAM: Cilium-managed ULA range, NOT VPC IPs. ENI
      # mode (ipam.mode=eni) is IPv4-ONLY per upstream docs; this
      # cluster is ip_family=ipv6 (immutable), so ENI mode is not an
      # option. Overlay encap is kernel-level Geneve — not a userspace
      # proxy hop. MTU auto-derived from node ENI (9001 → ~8871 pod).
      ipam = {
        mode = "cluster-pool"
        operator = {
          clusterPoolIPv6PodCIDRList = ["fd42::/104"]
          clusterPoolIPv6MaskSize    = 120
        }
      }
      ipv6                 = { enabled = true }
      ipv4                 = { enabled = false }
      enableIPv6Masquerade = true

      routingMode    = "tunnel"
      tunnelProtocol = "geneve"

      kubeProxyReplacement = true
      k8sServiceHost       = replace(module.eks.cluster_endpoint, "https://", "")
      k8sServicePort       = 443

      encryption = {
        enabled        = true
        type           = "wireguard"
        nodeEncryption = true
      }

      # C3: dsrDispatch=geneve is the ONLY DSR mode compatible with
      # tunnel+WireGuard; default `opt` fails. mode=hybrid: SNAT for
      # ETP:Cluster, DSR for ETP:Local (rio-gateway uses Local for
      # source-IP preservation).
      loadBalancer = {
        mode        = "hybrid"
        dsrDispatch = "geneve"
      }

      gatewayAPI = { enabled = true }
      hubble = {
        enabled = true
        relay   = { enabled = true }
      }

      # Embedded envoy (NOT separate cilium-envoy DaemonSet). Cilium
      # 1.19's separate-DS cecProcessor→EnvoyResource pipeline never
      # calls xds.upsertEndpoint — EDS stays at empty v1, Gateway
      # backends have zero endpoints. NOT IPv6-specific; reproduced
      # in k3s VM. See ~/tmp/rio-cilium/eds-rootcause.md. Embedded
      # mode (in-process xDS) works.
      envoy = { enabled = false }

      # Pin to system nodes — cilium-operator can't schedule on
      # Karpenter nodes (Karpenter itself depends on CNI being up).
      operator = {
        replicas     = 1
        nodeSelector = { "rio.build/node-role" = "system" }
      }
    })
  ]

  # C1: Gateway API CRDs must exist or gatewayAPI.enabled silently
  # no-ops. for_each set → depend on the whole map.
  depends_on = [module.eks, kubectl_manifest.gateway_api_crds]
}

# ============================================================
# aws-load-balancer-controller
# ============================================================
#
# Needs IRSA: the controller calls EC2/ELB APIs to create NLBs,
# register targets, manage security groups. The IAM policy is
# broad (~200 permissions) because it has to handle every LB
# feature. The policy JSON is vendored here — it's the standard
# one AWS publishes, pinned so a `tofu apply` doesn't fetch from
# GitHub at apply time.

# Service account for aws-lbc. Created here (not by the helm chart)
# so IRSA can annotate it before the pods start. The chart's
# `serviceAccount.create=false` + `serviceAccount.name` wire to this.
resource "kubernetes_service_account_v1" "aws_lbc" {
  metadata {
    name      = "aws-load-balancer-controller"
    namespace = "kube-system"
    annotations = {
      # iam module v6: output iam_role_arn → arn.
      "eks.amazonaws.com/role-arn" = module.aws_lbc_irsa.arn
    }
  }
  depends_on = [module.eks]
}

module "aws_lbc_irsa" {
  source  = "terraform-aws-modules/iam/aws//modules/iam-role-for-service-accounts"
  version = "~> 6.0"

  name = "${var.cluster_name}-aws-lbc"

  # The module has a built-in for this exact policy. Setting
  # attach_load_balancer_controller_policy = true generates and
  # attaches the standard ~200-permission policy. No vendored JSON
  # needed. Still present in v6 (survived the variable cull).
  attach_load_balancer_controller_policy = true

  oidc_providers = {
    eks = {
      provider_arn               = module.eks.oidc_provider_arn
      namespace_service_accounts = ["kube-system:aws-load-balancer-controller"]
    }
  }
}

resource "helm_release" "aws_lbc" {
  name       = "aws-load-balancer-controller"
  namespace  = "kube-system"
  repository = "https://aws.github.io/eks-charts"
  chart      = "aws-load-balancer-controller"
  # Pinned via nix/pins.nix → generated.auto.tfvars.json. v3.0+ aligns
  # chart with app version. v3 adds CRDs (ALBTargetControlConfig,
  # GlobalAccelerator) that helm upgrade does NOT apply from crds/ —
  # apply manually before bumping:
  #   kubectl apply -k github.com/aws/eks-charts/stable/aws-load-balancer-controller/crds?ref=master
  # GlobalAccelerator needs IAM perms not in terraform-aws-modules iam v6's
  # attach_load_balancer_controller_policy (NLB-only here, not a blocker).
  version = var.aws_lbc_version

  set = [
    {
      name  = "clusterName"
      value = module.eks.cluster_name
    },
    {
      name  = "serviceAccount.create"
      value = "false"
    },
    {
      name  = "serviceAccount.name"
      value = kubernetes_service_account_v1.aws_lbc.metadata[0].name
    },
    # Region + VPC explicitly: the controller can auto-discover via
    # IMDS, but we set IMDSv2 hop limit 1 on worker nodes (defense-
    # in-depth, see main.tf). The controller runs on system nodes
    # (no hop limit override there — though eks v21 now defaults
    # hop limit to 1 everywhere) so explicit is one less thing to
    # debug.
    {
      name  = "region"
      value = var.region
    },
    {
      name  = "vpcId"
      value = module.vpc.vpc_id
    },
  ]

  depends_on = [
    module.eks,
    kubernetes_service_account_v1.aws_lbc,
    # CNI must be up or aws-lbc pods are Pending → wait=true times out.
    helm_release.cilium,
  ]
}
