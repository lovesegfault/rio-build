# Karpenter node autoscaling — replaces the static `workers` managed
# nodegroup. The terraform-aws-eks karpenter submodule creates the
# controller IAM role + Pod Identity association, SQS interruption
# queue + EventBridge rules, node IAM role, and EKS access entry.
# NodePool/EC2NodeClass CRs live in the rio-build chart (gated by
# karpenter.enabled — `just eks deploy` sets it from tofu outputs).

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

locals {
  # Sourced from nix/pins.nix via generated.auto.tfvars.json. Kept as a
  # local (not inlined) because both helm_release.karpenter_crd and
  # helm_release.karpenter reference it — single point of truth within TF.
  karpenter_version = var.karpenter_version
}

# Separate CRD chart: Helm NEVER upgrades CRDs in a chart's crds/
# directory on `helm upgrade` — only on first install. A version bump
# on helm_release.karpenter below would leave stale CRDs (and skip new
# ones like NodeOverlay, added in v1.7). The karpenter-crd chart ships
# CRDs as templates/, which helm DOES upgrade.
resource "helm_release" "karpenter_crd" {
  name       = "karpenter-crd"
  namespace  = "kube-system"
  repository = "oci://public.ecr.aws/karpenter"
  chart      = "karpenter-crd"
  version    = local.karpenter_version

  depends_on = [module.eks]
}

resource "helm_release" "karpenter" {
  name      = "karpenter"
  namespace = "kube-system"
  # ECR Public supports anonymous pulls for public charts — no auth
  # needed. Passing aws_ecrpublic_authorization_token credentials here
  # causes intermittent 403s when the helm provider's OCI client picks
  # up a stale token from ~/.config/helm/registry/config.json (populated
  # by a prior run) instead of the fresh one terraform fetches.
  repository = "oci://public.ecr.aws/karpenter"
  chart      = "karpenter"
  version    = local.karpenter_version

  # CRDs come from helm_release.karpenter_crd — skip the baked-in
  # crds/ dir to avoid dual ownership.
  skip_crds = true

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
        # NodeOverlay (alpha, v1.7+): declare extended-resource capacity
        # on NodePools so Karpenter can bin-pack for smarter-devices/fuse
        # BEFORE a node exists. Without this, worker pods requesting the
        # extended resource deadlock cold-start (resource only advertised
        # after the device-plugin DaemonSet registers on a running node).
        # The NodeOverlay CR lives in the rio-build chart alongside
        # NodePools.
        featureGates = {
          nodeOverlay = true
        }
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

  depends_on = [module.eks, module.karpenter, helm_release.karpenter_crd]
}
