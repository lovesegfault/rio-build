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

variable "cluster" {
  description = "Cluster-level pins from nix/pins.toml via generated.auto.tfvars.json. kubernetes_version: 1.33+ required for hostUsers: false (user namespace isolation per ADR-012)."
  type = object({
    kubernetes_version = string
  })
}

# Addon pins — sourced from nix/pins.toml via generated.auto.tfvars.json
# so nix/tests/ and infra/eks/ agree. No default: if the generated file
# is missing, `tofu plan` fails loudly instead of silently diverging
# from the flake's pins. A new field in pins.toml must also be added to
# the object type here (plan errors on unexpected attributes), which
# keeps the two in sync deliberately.

variable "addons" {
  description = "Cluster addon pins (helm chart versions + Gateway API CRDs)."
  type = object({
    # cilium chart version (helm.cilium.io). Same pin as nix/cilium-render.nix.
    cilium = object({ version = string })
    # Gateway API CRD release tag; crds_hash is the fetchurl hash for the
    # standard-install.yaml bundle (consumed by nix/cilium-render.nix, not
    # terraform — it rides along because the whole pins tree lands here).
    gateway_api = object({
      version   = string
      crds_hash = string
    })
    # aws-load-balancer-controller chart version (eks-charts repo).
    aws_load_balancer_controller = object({ version = string })
    # Karpenter chart version (OCI public.ecr.aws/karpenter).
    karpenter = object({ version = string })
    # external-dns chart version (kubernetes-sigs.github.io/external-dns).
    external_dns = object({ version = string })
  })
}

variable "hubble_ui_enabled" {
  description = "Deploy the Hubble web UI. Off by default; xtask up sets this true for dev/QA clusters."
  type        = bool
  default     = false
}

variable "gateway_dns" {
  description = <<-EOT
    Stable DNS for the rio-gateway NLB (see dns.tf). external-dns
    watches the Service and keeps `prefix.zone` (or `zone` when
    prefix="") pointed at whatever NLB currently exists.
      provider — "route53", "cloudflare", or "" (disabled; default)
      zone     — DNS zone the provider hosts (e.g. "rio.example.test")
      prefix   — record label under zone ("gw"), or "" for apex
      create_route53_zone — provision the zone (route53 only); false
        means it must already exist (data lookup fails plan if not)
      cloudflare_zone_id  — optional --zone-id-filter scoping
  EOT
  type = object({
    provider            = string
    zone                = string
    prefix              = string
    create_route53_zone = optional(bool, false)
    cloudflare_zone_id  = optional(string, "")
  })
  default = { provider = "", zone = "", prefix = "" }
  validation {
    condition     = contains(["", "route53", "cloudflare"], var.gateway_dns.provider)
    error_message = "gateway_dns.provider must be \"route53\", \"cloudflare\", or \"\" (disabled)."
  }
  validation {
    condition     = var.gateway_dns.provider == "" || var.gateway_dns.zone != ""
    error_message = "gateway_dns.zone is required when gateway_dns.provider is set."
  }
}

variable "cloudflare_api_token" {
  description = "Cloudflare API token with Zone:DNS:Edit on gateway_dns.zone. Only read when gateway_dns.provider == \"cloudflare\". Prefer TF_VAR_cloudflare_api_token in env over committing to a tfvars file."
  type        = string
  sensitive   = true
  default     = ""
}

variable "system_instance_type" {
  description = "Instance type for system nodegroup (scheduler/store/gateway/controller)"
  type        = string
  default     = "m5.large"
}

variable "express_supported_az_ids" {
  description = <<-EOT
    AZ-IDs where S3 Express One Zone is available (see ADR-022 Design
    Overview §9). The per-AZ chunk-cache directory buckets
    (s3-express.tf) are created for the intersection of this set with
    the AZ-IDs the cluster's subnets actually land in
    (local.express_az_ids); an AZ outside the intersection simply gets
    no bucket and its store replicas read S3 standard directly.

    Set to [] to disable the Express cache tier cluster-wide (no
    buckets, no IAM policy, empty express_bucket_by_az_id output).

    Default is the supported set as of 2026-04. AWS expands this list
    over time — extend it here (and in the design overview) when a new
    AZ-ID gains Express support; nothing else needs to change.
  EOT
  type        = list(string)
  default = [
    "use1-az4", "use1-az5", "use1-az6",
    "use2-az1", "use2-az2",
    "usw2-az1", "usw2-az3", "usw2-az4",
    "aps1-az1", "aps1-az3",
    "apne1-az1", "apne1-az4",
    "euw1-az1", "euw1-az3",
    "eun1-az1", "eun1-az2", "eun1-az3",
  ]
  validation {
    # Catch zone NAMES (us-east-2a) being pasted where zone IDs
    # (use2-az1) belong — the name→ID mapping is account-randomized,
    # so a name here would silently never intersect.
    condition     = alltrue([for id in var.express_supported_az_ids : can(regex("^[a-z]+[0-9]+-az[0-9]+$", id))])
    error_message = "express_supported_az_ids entries must be AZ IDs (e.g. \"use2-az1\"), not AZ names (e.g. \"us-east-2a\")."
  }
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
# nix/nixos-node/ directly from nix/pins.toml. Declared (loosely typed)
# only because generated.auto.tfvars.json carries the whole pins tree
# and an undeclared key would warn on every plan/apply. `any` so new
# node pins flow through without touching variables.tf. Unused in
# *.tf — `tofu validate` is fine with that.
# ──────────────────────────────────────────────────────────────────────

variable "node" {
  description = "NixOS-node AMI pins (kernel, nodeadm, ecr-credential-provider) — consumed by nix/nixos-node/, not terraform."
  type        = any
}
