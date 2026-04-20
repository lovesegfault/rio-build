# Karpenter node autoscaling — replaces the static `workers` managed
# nodegroup. The terraform-aws-eks karpenter submodule creates the
# controller IAM role + Pod Identity association, SQS interruption
# queue + EventBridge rules, node IAM role, and EKS access entry.
# NodePool/EC2NodeClass CRs live in the rio-build chart (gated by
# karpenter.enabled — xtask sets it from tofu outputs).

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
  #
  # Cilium cluster-pool IPAM does NOT call the EC2 API from nodes —
  # pod IPs come from a Cilium-managed ULA pool, no per-ENI
  # allocation. (vpc-cni's AmazonEKS_CNI_IPv6_Policy was for ipamd's
  # AssignIpv6Addresses call; ENI mode would need operator-side IRSA
  # not node IAM, but ENI mode is IPv4-only so moot here.)
  node_iam_role_additional_policies = {
    AmazonSSMManagedInstanceCore = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
    PrimaryIpv6                  = aws_iam_policy.karpenter_node_primary_ipv6.arn
  }

  # Pod Identity — requires the eks-pod-identity-agent addon (main.tf
  # addons block). Karpenter is the first Pod Identity user in this
  # cluster; everything else stays IRSA.
  create_pod_identity_association = true
}

# Allow nodes to set primary-IPv6 on their own ENI at boot
# (primary-ipv6-init systemd oneshot baked into the NixOS AMI,
# nix/nixos-node/eks-node.nix). NLB target-type=instance + dualstack
# requires it; neither EC2NodeClass nor managed-nodegroup LTs can set
# it declaratively.
resource "aws_iam_policy" "karpenter_node_primary_ipv6" {
  name = "${var.cluster_name}-karpenter-node-primary-ipv6"
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = ["ec2:ModifyNetworkInterfaceAttribute"]
      Resource = "*"
    }]
  })
}

# Karpenter NodePools (rio-build chart, templates/karpenter.yaml) allow
# capacity-type=[spot, on-demand]. The first spot request in an account
# makes EC2 auto-create AWSServiceRoleForEC2Spot via the CALLER's
# iam:CreateServiceLinkedRole — the controller role above doesn't have
# that (and shouldn't; keep runtime IAM minimal). Symptom: karpenter
# logs AuthFailure.ServiceLinkedRoleCreationNotPermitted and silently
# falls through to on-demand. Pre-create under apply-time credentials.
#
# SLRs are account-global singletons; aws_iam_service_linked_role fails
# create if one already exists (prior manual spot request, another
# stack). aws_iam_roles (plural) returns an empty set rather than
# erroring on miss, so count=0 when present.
data "aws_iam_roles" "spot_slr" {
  name_regex  = "^AWSServiceRoleForEC2Spot$"
  path_prefix = "/aws-service-role/spot.amazonaws.com/"
}

resource "aws_iam_service_linked_role" "spot" {
  count            = length(data.aws_iam_roles.spot_slr.names) == 0 ? 1 : 0
  aws_service_name = "spot.amazonaws.com"
}

# Separate CRD chart: Helm NEVER upgrades CRDs in a chart's crds/
# directory on `helm upgrade` — only on first install. A version bump
# on helm_release.karpenter below would leave stale CRDs. The
# karpenter-crd chart ships CRDs as templates/, which helm DOES upgrade.
resource "helm_release" "karpenter_crd" {
  name       = "karpenter-crd"
  namespace  = "kube-system"
  repository = "oci://public.ecr.aws/karpenter"
  chart      = "karpenter-crd"
  version    = var.karpenter_version

  # aws_lbc dep: webhook-ordering only — see addons.tf aws_lbc.
  # cilium dep: CNI must be up or pods Pending → wait=true times out.
  depends_on = [helm_release.aws_lbc, helm_release.cilium]
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
  version    = var.karpenter_version

  # CRDs come from helm_release.karpenter_crd — skip the baked-in
  # crds/ dir to avoid dual ownership.
  skip_crds = true

  # Karpenter's post-install webhook validation hook can flake on
  # first install. NodePool CRs (applied later by xtask deploy) retry
  # on transient webhook unavailability, so we don't block here.
  wait = false

  values = [
    yamlencode({
      settings = {
        clusterName       = module.eks.cluster_name
        clusterEndpoint   = module.eks.cluster_endpoint
        interruptionQueue = module.karpenter.queue_name
        # NodeRepair (alpha): terminate+replace nodes that stay
        # NotReady past the toleration threshold. Live QA hit a
        # cgwb_release kernel panic (cgroup-writeback teardown under
        # rio's one-cgroup-per-build churn) where the panic handler
        # itself hung — kubelet dead, Node NotReady for 4h+, 8 pods
        # stuck Terminating. WhenEmpty consolidation ignores
        # "non-empty" NotReady; this is the only repair path.
        #
        # Per-instance CloudWatch StatusCheckFailed_Instance →
        # automate:ec2:terminate alarms are not declaratively
        # feasible for ephemeral Karpenter nodes (alarm dimension
        # binds a specific InstanceId; instances are short-lived).
        # The submodule already routes aws.health events to the
        # interruption queue (covers AWS-side scheduled/system
        # issues). OS-level failures (kernel panic, kubelet wedge)
        # surface as Node NotReady, which NodeRepair handles.
        featureGates = {
          nodeRepair = true
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

  depends_on = [module.karpenter, helm_release.karpenter_crd]
}
