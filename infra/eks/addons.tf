# Cluster addons installed via Helm: cert-manager + aws-load-balancer-
# controller. Both are prerequisites for `kubectl apply -k infra/k8s/
# overlays/eks` — cert-manager issues the mTLS certs (cert-manager.yaml
# in the prod overlay has Certificate CRs), and aws-lbc provisions the
# gateway NLB (service.beta.kubernetes.io/aws-load-balancer-type:
# external annotation).
#
# helm_release here (not a separate `helm install` step) keeps
# everything in one `tofu apply`. The tradeoff: terraform now owns
# the Helm release lifecycle — `helm upgrade` outside terraform
# drifts state. For cluster-foundational addons that rarely change,
# this is fine.

# ============================================================
# cert-manager
# ============================================================

resource "helm_release" "cert_manager" {
  name       = "cert-manager"
  namespace  = "cert-manager"
  repository = "https://charts.jetstack.io"
  chart      = "cert-manager"
  # Pin: cert-manager's chart versioning equals its app version.
  # v1.x is stable; bump when there's a reason to.
  version = "v1.16.2"

  create_namespace = true

  # CRDs via the chart (not a separate `kubectl apply -f crds.yaml`
  # step). cert-manager v1.15+ uses `crds.enabled`; the deprecated
  # `installCRDs` is an ERROR (not ignored) when set alongside it.
  set {
    name  = "crds.enabled"
    value = "true"
  }

  # `depends_on` at the resource level makes this wait for the EKS
  # module. The helm provider itself can't have depends_on, so
  # without this, terraform might try to install cert-manager
  # before the cluster exists (rare — usually the plan graph gets
  # it right via the provider's `host = module.eks.cluster_endpoint`
  # reference — but explicit is safer).
  depends_on = [module.eks]
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
resource "kubernetes_service_account" "aws_lbc" {
  metadata {
    name      = "aws-load-balancer-controller"
    namespace = "kube-system"
    annotations = {
      "eks.amazonaws.com/role-arn" = module.aws_lbc_irsa.iam_role_arn
    }
  }
  depends_on = [module.eks]
}

module "aws_lbc_irsa" {
  source  = "terraform-aws-modules/iam/aws//modules/iam-role-for-service-accounts-eks"
  version = "~> 5.0"

  role_name = "${var.cluster_name}-aws-lbc"

  # The module has a built-in for this exact policy. Setting
  # attach_load_balancer_controller_policy = true generates and
  # attaches the standard ~200-permission policy. No vendored JSON
  # needed after all.
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
  version    = "1.10.0"

  set {
    name  = "clusterName"
    value = module.eks.cluster_name
  }
  set {
    name  = "serviceAccount.create"
    value = "false"
  }
  set {
    name  = "serviceAccount.name"
    value = kubernetes_service_account.aws_lbc.metadata[0].name
  }
  # Region + VPC explicitly: the controller can auto-discover via
  # IMDS, but we set IMDSv2 hop limit 1 on worker nodes (defense-
  # in-depth, see main.tf). The controller runs on system nodes
  # (no hop limit set there) so IMDS would work, but explicit is
  # one less thing to debug.
  set {
    name  = "region"
    value = var.region
  }
  set {
    name  = "vpcId"
    value = module.vpc.vpc_id
  }

  depends_on = [
    module.eks,
    kubernetes_service_account.aws_lbc,
  ]
}
