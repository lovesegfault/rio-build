# Karpenter node autoscaling — replaces the static `workers` managed
# nodegroup. The terraform-aws-eks karpenter submodule creates the
# controller IAM role + Pod Identity association, SQS interruption
# queue + EventBridge rules, node IAM role, and EKS access entry.
# NodePool/EC2NodeClass CRs live in the rio-build chart (gated by
# karpenter.enabled — `just eks deploy` sets it from tofu outputs).

# ECR Public token. The Karpenter chart is OCI at public.ecr.aws; the
# ECR Public API is us-east-1 only regardless of cluster region — this
# is the data source's own region override, not a provider alias.
data "aws_ecrpublic_authorization_token" "karpenter" {
  region = "us-east-1"
}

# Creates:
#   - Controller IAM role + Pod Identity association (v21 of the
#     submodule supports ONLY Pod Identity — no IRSA fallback)
#   - SQS interruption queue + EventBridge rules (spot interrupt,
#     rebalance, instance-state-change) so Karpenter drains nodes
#     gracefully before AWS terminates them
#   - Node IAM role (referenced by EC2NodeClass.spec.role)
#   - EKS access entry for the node role (so kubelets can join)
module "karpenter" {
  source  = "terraform-aws-modules/eks/aws//modules/karpenter"
  version = "~> 21.0"

  cluster_name = module.eks.cluster_name

  # Stable name (no prefix/suffix) so the chart can reference it via
  # tofu output without churn.
  node_iam_role_use_name_prefix = false
  node_iam_role_name            = "${var.cluster_name}-karpenter-node"

  # SSM access so Karpenter-provisioned nodes show up in Session
  # Manager (same as the bastion). Not required, useful for debugging.
  node_iam_role_additional_policies = {
    AmazonSSMManagedInstanceCore = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
  }

  # Pod Identity — requires the eks-pod-identity-agent addon (main.tf
  # addons block). Karpenter is the first Pod Identity user in this
  # cluster; everything else stays IRSA.
  create_pod_identity_association = true
}

resource "helm_release" "karpenter" {
  name                = "karpenter"
  namespace           = "kube-system"
  repository          = "oci://public.ecr.aws/karpenter"
  repository_username = data.aws_ecrpublic_authorization_token.karpenter.user_name
  repository_password = data.aws_ecrpublic_authorization_token.karpenter.password
  chart               = "karpenter"
  version             = "1.6.0"

  # Karpenter's post-install webhook validation hook can flake on
  # first install. NodePool CRs (applied later by `just eks deploy`)
  # retry on transient webhook unavailability, so we don't block here.
  wait = false

  values = [
    yamlencode({
      settings = {
        clusterName       = module.eks.cluster_name
        clusterEndpoint   = module.eks.cluster_endpoint
        interruptionQueue = module.karpenter.queue_name
      }
      # Pin controller to system nodes — can't run Karpenter on
      # Karpenter-provisioned nodes (chicken-and-egg on first boot).
      # System nodegroup has no taint → no toleration needed.
      # Targets our own label (main.tf) — the eks.amazonaws.com/
      # nodegroup label is name-prefixed and unpredictable.
      nodeSelector = {
        "rio.build/node-role" = "system"
      }
    })
  ]

  depends_on = [module.eks, module.karpenter]
}
