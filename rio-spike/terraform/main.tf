provider "aws" {
  region = var.region
}

data "aws_availability_zones" "available" {
  state = "available"
}

data "aws_caller_identity" "current" {}

locals {
  azs = slice(data.aws_availability_zones.available.names, 0, 2)
}

# --- VPC ---

module "vpc" {
  source  = "terraform-aws-modules/vpc/aws"
  version = "~> 6.6"

  name = "${var.cluster_name}-vpc"
  cidr = "10.0.0.0/16"

  azs             = local.azs
  private_subnets = ["10.0.1.0/24", "10.0.2.0/24"]
  public_subnets  = ["10.0.101.0/24", "10.0.102.0/24"]

  enable_nat_gateway   = true
  single_nat_gateway   = true # cost savings for a spike
  enable_dns_hostnames = true

  public_subnet_tags = {
    "kubernetes.io/role/elb" = 1
  }

  private_subnet_tags = {
    "kubernetes.io/role/internal-elb" = 1
  }

  tags = {
    Project = "rio-spike"
  }
}

# --- ECR ---

resource "aws_ecr_repository" "spike" {
  name                 = "rio-spike"
  image_tag_mutability = "MUTABLE"
  force_delete         = true # spike repo, easy cleanup

  image_scanning_configuration {
    scan_on_push = false
  }

  tags = {
    Project = "rio-spike"
  }
}

# --- EKS ---

module "eks" {
  source  = "terraform-aws-modules/eks/aws"
  version = "~> 21.15"

  name               = var.cluster_name
  kubernetes_version = var.kubernetes_version

  vpc_id     = module.vpc.vpc_id
  subnet_ids = module.vpc.private_subnets

  endpoint_public_access = true

  # Let the caller's IAM identity administer the cluster
  enable_cluster_creator_admin_permissions = true

  # Daemonset addons — safe to install before nodes exist
  addons = {
    vpc-cni = {
      most_recent = true
    }
    kube-proxy = {
      most_recent = true
    }
    # coredns is managed separately below — it needs schedulable nodes first
  }

  # Node groups are created separately below to ensure addons are ready first

  tags = {
    Project = "rio-spike"
  }
}

# --- EKS Node Group (separate to enforce addon-before-nodes ordering) ---

module "spike_node_group" {
  source  = "terraform-aws-modules/eks/aws//modules/eks-managed-node-group"
  version = "~> 21.15"

  name            = "spike-workers"
  cluster_name         = module.eks.cluster_name
  cluster_service_cidr = module.eks.cluster_service_cidr

  subnet_ids = module.vpc.private_subnets

  cluster_primary_security_group_id = module.eks.cluster_primary_security_group_id
  vpc_security_group_ids            = [module.eks.node_security_group_id]

  ami_type       = "AL2023_x86_64_STANDARD"
  instance_types = [var.instance_type]

  min_size     = var.node_count
  max_size     = var.node_count
  desired_size = var.node_count

  disk_size = 100

  labels = {
    "rio.build/role" = "spike-worker"
  }

  taints = {
    spike = {
      key    = "rio.build/spike"
      value  = "true"
      effect = "NO_SCHEDULE"
    }
  }

  metadata_options = {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }

  tags = {
    Project = "rio-spike"
  }

  # Ensure addons (especially vpc-cni) are installed before nodes launch
  depends_on = [module.eks]
}

# --- System node group (untainted, for coredns and other system pods) ---

module "system_node_group" {
  source  = "terraform-aws-modules/eks/aws//modules/eks-managed-node-group"
  version = "~> 21.15"

  name         = "system"
  cluster_name         = module.eks.cluster_name
  cluster_service_cidr = module.eks.cluster_service_cidr

  subnet_ids = module.vpc.private_subnets

  cluster_primary_security_group_id = module.eks.cluster_primary_security_group_id
  vpc_security_group_ids            = [module.eks.node_security_group_id]

  ami_type       = "AL2023_x86_64_STANDARD"
  instance_types = [var.instance_type]

  min_size     = var.system_node_count
  max_size     = var.system_node_count
  desired_size = var.system_node_count

  # No taints — system pods schedule here

  tags = {
    Project = "rio-spike"
  }

  depends_on = [module.eks]
}

# --- CoreDNS addon (needs schedulable nodes, so depends on system node group) ---

resource "aws_eks_addon" "coredns" {
  cluster_name                = module.eks.cluster_name
  addon_name                  = "coredns"
  resolve_conflicts_on_create = "OVERWRITE"

  depends_on = [module.system_node_group]
}
