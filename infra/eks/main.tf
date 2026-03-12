# EKS cluster for rio-build.
#
# Managed control plane + 2 nodegroups (system + workers) + IRSA for
# rio-store S3 access. IMDSv2 hop limit 1 on worker nodes (metadata
# protection).
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
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
    # helm + kubernetes providers: for cert-manager and
    # aws-load-balancer-controller helm_release (addons.tf).
    helm = {
      source  = "hashicorp/helm"
      version = "~> 2.0"
    }
    kubernetes = {
      source  = "hashicorp/kubernetes"
      version = "~> 2.0"
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
  kubernetes {
    host                   = module.eks.cluster_endpoint
    cluster_ca_certificate = base64decode(module.eks.cluster_certificate_authority_data)
    exec {
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
  source  = "terraform-aws-modules/vpc/aws"
  version = "~> 5.0"

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
  }
}

module "eks" {
  source  = "terraform-aws-modules/eks/aws"
  version = "~> 20.0"

  cluster_name    = var.cluster_name
  cluster_version = var.kubernetes_version

  vpc_id     = module.vpc.vpc_id
  subnet_ids = module.vpc.private_subnets

  # IRSA: OIDC provider for pod-to-IAM role assumption. Required
  # for rio-store's S3 access (no static AWS keys in pods).
  enable_irsa = true

  # Public endpoint for kubectl from outside the VPC. eks module v20
  # default is unclear from docs — set explicitly. Private-only would
  # require running kubectl from inside the VPC (bastion) or via an
  # SSM tunnel to the API server, which is more friction than the
  # cluster being a dev/test deployment warrants. Lock down with
  # cluster_endpoint_public_access_cidrs if this bothers you.
  cluster_endpoint_public_access = true

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

  # Managed node groups. Two groups:
  # - system: scheduler/store/gateway/controller. m5.large, 3 nodes.
  # - workers: rio-worker pods. c6a.xlarge (compute-optimized),
  #   2-10 nodes, tainted so ONLY worker pods land there.
  eks_managed_node_groups = {
    system = {
      instance_types = [var.system_instance_type]
      min_size       = 3
      max_size       = 5
      desired_size   = 3

      # No taint: system components (plus kube-system addons like
      # CoreDNS) schedule here freely.
    }

    workers = {
      instance_types = [var.worker_instance_type]
      min_size       = var.worker_min_size
      max_size       = var.worker_max_size
      desired_size   = var.worker_min_size

      # Taint so only pods with the matching toleration (set by
      # the WorkerPool reconciler when spec.tolerations is
      # configured) land here. Keeps kube-system pods off the
      # worker nodes — they're for builds only.
      taints = [{
        key    = "rio.build/worker"
        value  = "true"
        effect = "NO_SCHEDULE"
      }]

      labels = {
        "rio.build/node-role" = "worker"
      }

      # IMDSv2 hop limit 1: pod can't reach instance metadata.
      # Container network adds a hop, so limit 1 means the
      # instance itself can (kubelet needs it) but pods can't.
      # Defense-in-depth: NetworkPolicy also blocks 169.254.
      # 169.254, but this is belt-and-suspenders (and catches
      # hostNetwork pods which bypass NetworkPolicy).
      metadata_options = {
        http_endpoint               = "enabled"
        http_tokens                 = "required" # IMDSv2 only
        http_put_response_hop_limit = 1
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
  source  = "terraform-aws-modules/iam/aws//modules/iam-role-for-service-accounts-eks"
  version = "~> 5.0"

  role_name = "${var.cluster_name}-rio-store"

  oidc_providers = {
    eks = {
      provider_arn               = module.eks.oidc_provider_arn
      namespace_service_accounts = ["rio-system:rio-store"]
    }
  }

  role_policy_arns = {
    s3 = aws_iam_policy.rio_store_s3.arn
  }
}

resource "aws_iam_policy" "rio_store_s3" {
  name   = "${var.cluster_name}-rio-store-s3"
  policy = data.aws_iam_policy_document.rio_store_s3.json
}
