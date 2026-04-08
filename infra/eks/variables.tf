variable "region" {
  description = "AWS region"
  type        = string
  default     = "us-east-2"
}

variable "cluster_name" {
  description = "EKS cluster name (also used as prefix for IAM roles, S3 bucket, RDS, etc). Changing this recreates everything."
  type        = string
  default     = "rio-build"
  validation {
    # S3 bucket names (which derive from this) are lowercase, no
    # underscores, 3-63 chars. RDS identifiers: lowercase, hyphens
    # only, start with a letter. Enforce the intersection.
    condition     = can(regex("^[a-z][a-z0-9-]{2,30}$", var.cluster_name))
    error_message = "cluster_name must be 3-31 chars, lowercase alphanumeric + hyphens, starting with a letter (used in S3 bucket and RDS identifier names)."
  }
}

variable "kubernetes_version" {
  description = "K8s version for the EKS control plane. 1.33+ required for hostUsers: false (user namespace isolation per ADR-012). Sourced from nix/pins.nix via generated.auto.tfvars.json."
  type        = string
}

# Addon chart versions — sourced from nix/pins.nix via
# generated.auto.tfvars.json so nix/tests/ and infra/eks/ agree. No
# default: if the generated file is missing, `tofu plan` fails loudly
# instead of silently diverging from the flake's pins.

variable "cert_manager_version" {
  description = "cert-manager chart version (jetstack repo publishes as vX.Y.Z)."
  type        = string
}

variable "aws_lbc_version" {
  description = "aws-load-balancer-controller chart version (eks-charts repo)."
  type        = string
}

variable "karpenter_version" {
  description = "Karpenter chart version (OCI public.ecr.aws/karpenter)."
  type        = string
}

variable "system_instance_type" {
  description = "Instance type for system nodegroup (scheduler/store/gateway/controller)"
  type        = string
  default     = "m5.large"
}

# worker_instance_type / worker_min_size / worker_max_size removed —
# worker nodes are Karpenter-provisioned (karpenter.tf). Instance
# families are configured per-NodePool in the chart
# (values.yaml karpenter.nodePools).

# chunk_bucket var removed — now terraform-managed (s3.tf). Bucket name
# is derived from cluster_name + random suffix (S3 bucket names are
# global). Output: chunk_bucket_name.

# ──────────────────────────────────────────────────────────────────────
# NixOS-node AMI build pins. NOT terraform inputs — consumed by
# nix/nixos-node/ directly from nix/pins.nix. Declared here only to
# silence tofu's "Value for undeclared variable" warning ×6 on every
# plan/apply: generated.auto.tfvars.json is the JSON-serialized pins
# file and carries every key, terraform-relevant or not, so a key
# added to pins.nix lands in tfvars automatically (no mapping layer
# to forget). Unused in *.tf — `tofu validate` is fine with that.
# ──────────────────────────────────────────────────────────────────────

variable "node_kernel_minor" {
  description = "NixOS-node kernel minor (consumed by nix/nixos-node/, not terraform)."
  type        = string
}

variable "nodeadm_rev" {
  description = "amazon-eks-ami nodeadm release tag (consumed by nix/nixos-node/, not terraform)."
  type        = string
}

variable "nodeadm_src_hash" {
  description = "nodeadm source hash (consumed by nix/nixos-node/, not terraform)."
  type        = string
}

variable "ecr_credential_provider_rev" {
  description = "cloud-provider-aws ecr-credential-provider tag (consumed by nix/nixos-node/, not terraform)."
  type        = string
}

variable "ecr_credential_provider_src_hash" {
  description = "ecr-credential-provider source hash (consumed by nix/nixos-node/, not terraform)."
  type        = string
}

variable "ecr_credential_provider_vendor_hash" {
  description = "ecr-credential-provider Go vendor hash (consumed by nix/nixos-node/, not terraform)."
  type        = string
}
