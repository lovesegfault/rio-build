# Cluster addons installed via Helm: cert-manager + aws-load-balancer-
# controller. Both are prerequisites for the rio chart — cert-manager
# issues the mTLS certs (the chart's cert-manager.yaml template has
# Certificate CRs when tls.enabled=true), and aws-lbc provisions the
# gateway NLB (service.beta.kubernetes.io/aws-load-balancer-type:
# external annotation, set via xtask deploy --set-json).
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
  # Pinned via nix/pins.nix → generated.auto.tfvars.json. cert-manager's
  # chart versioning equals its app version. v1.20 requires k8s ≥1.31
  # (selectable-field CRDs); UID/GID changed 1000/0 → 65532/65532 —
  # harmless, no PVs.
  version = var.cert_manager_version

  create_namespace = true

  # CRDs via the chart (not a separate `kubectl apply -f crds.yaml`
  # step). cert-manager v1.15+ uses `crds.enabled`; the deprecated
  # `installCRDs` is an ERROR (not ignored) when set alongside it.
  # helm provider v3: `set` is now a list-of-objects attribute, not
  # repeated blocks. See hashicorp/terraform-provider-helm
  # docs/guides/v3-upgrade-guide.md.
  set = [
    {
      name  = "crds.enabled"
      value = "true"
    }
  ]

  # `depends_on` at the resource level makes this wait for the EKS
  # module. The helm provider itself can't have depends_on, so
  # without this, terraform might try to install cert-manager
  # before the cluster exists (rare — usually the plan graph gets
  # it right via the provider's `host = module.eks.cluster_endpoint`
  # reference — but explicit is safer).
  #
  # aws_lbc dep: not semantic — aws-lbc's mservice.elbv2.k8s.aws
  # mutating webhook intercepts ALL Service creates cluster-wide
  # with failurePolicy=Fail (it's the only one of its six webhooks
  # with an empty namespaceSelector). On a fresh apply, the chart
  # lands the MutatingWebhookConfiguration before its pod is Ready
  # → cert-manager's Service create gets "no endpoints available
  # for service aws-load-balancer-webhook-service". Serializing
  # behind aws_lbc (wait=true default → Deployment Ready → endpoints
  # populated) closes the race. Same dep on karpenter_crd and
  # external_secrets for the same reason.
  depends_on = [module.eks, helm_release.aws_lbc]
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
  ]
}
