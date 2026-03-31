# EKS cluster for rio-build.
#
# Managed control plane + system nodegroup + IRSA for rio-store S3 access.
# Worker nodes are Karpenter-provisioned (karpenter.tf) — no managed
# worker nodegroup. IMDSv2 hop limit 1 on Karpenter nodes (metadata
# protection) via EC2NodeClass in the chart.
#
# Usage:
#   AWS_PROFILE=beme_sandbox terraform init
#   AWS_PROFILE=beme_sandbox terraform plan
#   AWS_PROFILE=beme_sandbox terraform apply
#
# After apply: `terraform output kubeconfig_command` prints the
# aws eks update-kubeconfig invocation.

terraform {
  required_version = ">= 1.5"
  # Version constraints match what nixpkgs ships (opentofu.withPlugins
  # in flake.nix). Nix is the real lock; these are for human readers
  # and for the EKS/IAM modules' own >=-constraints. Bumping nixpkgs
  # may require bumping these.
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 6.0"
    }
    helm = {
      source  = "hashicorp/helm"
      version = "~> 3.0"
    }
    kubernetes = {
      source  = "hashicorp/kubernetes"
      version = "~> 3.0"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.0"
    }
  }
}

provider "aws" {
  region = var.region
}

# K8s/Helm providers use `exec` auth (not a cached token) so
# `tofu plan` on a fresh clone doesn't try to contact a cluster
# that doesn't exist yet. The exec block defers token fetch to
# apply time, and the token is always fresh (15min EKS token
# lifetime means a cached token would expire mid-apply anyway).
#
# `depends_on` at the resource level (in addons.tf) makes the
# helm_releases wait for module.eks — the provider block itself
# can't have depends_on.
provider "helm" {
  # helm provider v3 migrated to the plugin framework: `kubernetes` and
  # `exec` are now nested object ATTRIBUTES (`= { ... }`), not blocks.
  # See hashicorp/terraform-provider-helm docs/guides/v3-upgrade-guide.md.
  kubernetes = {
    host                   = module.eks.cluster_endpoint
    cluster_ca_certificate = base64decode(module.eks.cluster_certificate_authority_data)
    exec = {
      api_version = "client.authentication.k8s.io/v1beta1"
      command     = "aws"
      args        = ["eks", "get-token", "--cluster-name", module.eks.cluster_name, "--region", var.region]
    }
  }
}

provider "kubernetes" {
  host                   = module.eks.cluster_endpoint
  cluster_ca_certificate = base64decode(module.eks.cluster_certificate_authority_data)
  exec {
    api_version = "client.authentication.k8s.io/v1beta1"
    command     = "aws"
    args        = ["eks", "get-token", "--cluster-name", module.eks.cluster_name, "--region", var.region]
  }
}

data "aws_availability_zones" "available" {
  state = "available"
}

# VPC: 3 AZs, public + private subnets. EKS control plane in
# public (managed), nodes in private (egress via NAT).
module "vpc" {
  source = "terraform-aws-modules/vpc/aws"
  # v6 is a provider-constraint-only bump (requires aws >= 6.0); no
  # variable/output renames for our usage.
  version = "~> 6.0"

  name = "${var.cluster_name}-vpc"
  cidr = "10.42.0.0/16"

  azs             = slice(data.aws_availability_zones.available.names, 0, 3)
  private_subnets = ["10.42.1.0/24", "10.42.2.0/24", "10.42.3.0/24"]
  public_subnets  = ["10.42.101.0/24", "10.42.102.0/24", "10.42.103.0/24"]

  enable_nat_gateway   = true
  single_nat_gateway   = true # dev: one NAT is fine; prod: HA with per-AZ
  enable_dns_hostnames = true

  # EKS requires these tags for subnet auto-discovery by the
  # load-balancer-controller and cluster-autoscaler.
  public_subnet_tags = {
    "kubernetes.io/role/elb" = "1"
  }
  private_subnet_tags = {
    "kubernetes.io/role/internal-elb" = "1"
    # Karpenter EC2NodeClass subnetSelectorTerms matches on this.
    "karpenter.sh/discovery" = var.cluster_name
  }
}

module "eks" {
  source  = "terraform-aws-modules/eks/aws"
  version = "~> 21.0"

  # v21 stripped the `cluster_` prefix from input variables to match the
  # underlying API (cluster_name → name, cluster_version →
  # kubernetes_version, cluster_endpoint_public_access →
  # endpoint_public_access). OUTPUTS kept the prefix —
  # `module.eks.cluster_name` etc. are unchanged. See
  # terraform-aws-eks/docs/UPGRADE-21.0.md.
  name               = var.cluster_name
  kubernetes_version = var.kubernetes_version

  vpc_id     = module.vpc.vpc_id
  subnet_ids = module.vpc.private_subnets

  # IRSA: OIDC provider for pod-to-IAM role assumption. Required
  # for rio-store's S3 access (no static AWS keys in pods).
  enable_irsa = true

  # Public endpoint for kubectl from outside the VPC. Private-only would
  # require running kubectl from inside the VPC (bastion) or via an
  # SSM tunnel to the API server, which is more friction than the
  # cluster being a dev/test deployment warrants. Lock down with
  # endpoint_public_access_cidrs if this bothers you.
  endpoint_public_access = true

  # API authentication mode. CONFIG_MAP = legacy aws-auth only.
  # API_AND_CONFIG_MAP = both, for migration. API = access entries
  # only (the new way, EKS 1.30+). API gives us `aws eks update-
  # kubeconfig` working out of the box for the terraform caller
  # without touching aws-auth.
  authentication_mode = "API_AND_CONFIG_MAP"

  # Grant the terraform caller cluster-admin. Without this, the
  # helm_release in addons.tf (which uses the same AWS creds via
  # the exec provider) would get Forbidden on every API call.
  enable_cluster_creator_admin_permissions = true

  # eks-pod-identity-agent: required for module.karpenter's Pod
  # Identity association (karpenter.tf). before_compute=true so the
  # agent DaemonSet is running before any nodegroup pods start —
  # otherwise the association doesn't take effect until agent restart.
  addons = {
    eks-pod-identity-agent = {
      before_compute = true
    }
  }

  # Discovery tag on the node security group. Karpenter's
  # EC2NodeClass securityGroupSelectorTerms matches on this.
  node_security_group_tags = {
    "karpenter.sh/discovery" = var.cluster_name
  }

  # Managed node groups. System only — worker nodes are
  # Karpenter-provisioned (karpenter.tf). rio-controller scales the
  # WorkerPool STS by queue depth; Karpenter provisions nodes for
  # Pending pods. Scale-to-zero: rio-controller's 10-min
  # SCALE_DOWN_WINDOW → pods to 0 → Karpenter consolidates.
  eks_managed_node_groups = {
    system = {
      instance_types = [var.system_instance_type]
      min_size       = 3
      max_size       = 5
      desired_size   = 3

      # No taint: system components (plus kube-system addons like
      # CoreDNS, Karpenter controller) schedule here freely.
      #
      # Label: Karpenter's nodeSelector (karpenter.tf) targets this.
      # The eks.amazonaws.com/nodegroup label is name-prefixed by
      # default (e.g. system-2026031219...) so we can't match it
      # statically. EKS applies label updates in-place — no node churn.
      labels = {
        "rio.build/node-role" = "system"
      }
    }
  }
}

# IRSA for rio-store: S3 GetObject/PutObject/DeleteObject/ListBucket
# on the chunk bucket. The pod assumes this role via the projected
# service account token; no static keys.
data "aws_iam_policy_document" "rio_store_s3" {
  statement {
    effect = "Allow"
    actions = [
      "s3:GetObject",
      "s3:PutObject",
      "s3:DeleteObject",
    ]
    resources = ["${aws_s3_bucket.chunks.arn}/*"]
  }
  statement {
    effect    = "Allow"
    actions   = ["s3:ListBucket"]
    resources = [aws_s3_bucket.chunks.arn]
  }
}

module "rio_store_irsa" {
  # v6 renamed the submodule (dropped `-eks` suffix) and stripped the
  # `role_` prefix from inputs/outputs: role_name → name,
  # role_policy_arns → policies, output iam_role_arn → arn.
  # See terraform-aws-iam/docs/UPGRADE-6.0.md.
  source  = "terraform-aws-modules/iam/aws//modules/iam-role-for-service-accounts"
  version = "~> 6.0"

  name = "${var.cluster_name}-rio-store"

  oidc_providers = {
    eks = {
      provider_arn = module.eks.oidc_provider_arn
      # ADR-019: store moved to its own namespace (NS_STORE in
      # xtask/src/k8s/mod.rs). The trust-policy sub must match the
      # pod's OIDC token: system:serviceaccount:rio-store:rio-store.
      # Drift here → sts:AssumeRoleWithWebIdentity AccessDenied →
      # S3 PutObject fails → chunked PutPath (NAR ≥ 256 KiB) fails.
      namespace_service_accounts = ["rio-store:rio-store"]
    }
  }

  policies = {
    s3 = aws_iam_policy.rio_store_s3.arn
  }
}

resource "aws_iam_policy" "rio_store_s3" {
  name   = "${var.cluster_name}-rio-store-s3"
  policy = data.aws_iam_policy_document.rio_store_s3.json
}

# IRSA for rio-scheduler: S3 PutObject/ListBucket on the chunks bucket
# (build-log flush writes under logs/ prefix in the same bucket). The
# scheduler aggregates worker logs and flushes to S3 — it does NOT read
# or delete, so narrower than the store policy.
data "aws_iam_policy_document" "rio_scheduler_s3" {
  statement {
    effect = "Allow"
    actions = [
      "s3:PutObject",
    ]
    resources = ["${aws_s3_bucket.chunks.arn}/*"]
  }
  statement {
    effect    = "Allow"
    actions   = ["s3:ListBucket"]
    resources = [aws_s3_bucket.chunks.arn]
  }
}

module "rio_scheduler_irsa" {
  source  = "terraform-aws-modules/iam/aws//modules/iam-role-for-service-accounts"
  version = "~> 6.0"

  name = "${var.cluster_name}-rio-scheduler"

  oidc_providers = {
    eks = {
      provider_arn               = module.eks.oidc_provider_arn
      namespace_service_accounts = ["rio-system:rio-scheduler"]
    }
  }

  policies = {
    s3 = aws_iam_policy.rio_scheduler_s3.arn
  }
}

resource "aws_iam_policy" "rio_scheduler_s3" {
  name   = "${var.cluster_name}-rio-scheduler-s3"
  policy = data.aws_iam_policy_document.rio_scheduler_s3.json
}
