# Bootstrap: the S3 bucket that holds terraform state for infra/eks.
#
# Chicken-and-egg: infra/eks's backend needs this bucket to exist
# BEFORE `tofu init`. This module creates it. Run ONCE per AWS
# account; its own state is tiny (one bucket) and lives locally
# by design — commit terraform.tfstate for this module only so
# anyone can `tofu destroy` it later without the original applier.
#
# OpenTofu ≥1.6 (and Terraform ≥1.10) support native S3 state
# locking via the bucket's own object lock — no DynamoDB table
# needed. We enable versioning + object lock here; the backend
# config in infra/eks/backend.tf sets `use_lockfile = true`.
#
# Usage:
#   cd infra/bootstrap
#   AWS_PROFILE=beme_sandbox tofu init
#   AWS_PROFILE=beme_sandbox tofu apply
#   # → prints the bucket name; paste into infra/eks/backend.tf

terraform {
  required_version = ">= 1.6"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

variable "region" {
  type    = string
  default = "us-east-2"
}

variable "bucket_name" {
  description = "State bucket name. Must be globally unique. Default includes account ID to avoid collisions."
  type        = string
  default     = ""
}

provider "aws" {
  region = var.region
}

data "aws_caller_identity" "current" {}

locals {
  # Account ID in the name: two people bootstrapping in different
  # accounts with the same naming convention won't collide.
  bucket = var.bucket_name != "" ? var.bucket_name : "rio-tfstate-${data.aws_caller_identity.current.account_id}"
}

resource "aws_s3_bucket" "state" {
  bucket = local.bucket

  # object_lock_enabled must be set at creation time — can't toggle
  # it later. Native S3 locking (no DynamoDB) requires it.
  object_lock_enabled = true

  # State buckets do NOT get force_destroy. Losing state is the
  # worst thing that can happen to a terraform deployment — you
  # end up with orphan cloud resources you can't track. `tofu
  # destroy` on bootstrap should require emptying the bucket
  # manually (which forces you to think about whether every
  # workspace using it has been destroyed first).
}

# Versioning: required for object lock, and also means accidental
# state corruption is recoverable — roll back to the previous
# version. S3 lifecycle can prune old versions after N days if
# it grows (it won't — state files are KB, not GB).
resource "aws_s3_bucket_versioning" "state" {
  bucket = aws_s3_bucket.state.id
  versioning_configuration {
    status = "Enabled"
  }
}

resource "aws_s3_bucket_public_access_block" "state" {
  bucket                  = aws_s3_bucket.state.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

output "bucket" {
  description = "State bucket name — set `bucket = \"<this>\"` in infra/eks/backend.tf"
  value       = aws_s3_bucket.state.bucket
}

output "backend_config" {
  description = "Ready-to-paste backend block"
  value       = <<-EOT
    terraform {
      backend "s3" {
        bucket       = "${aws_s3_bucket.state.bucket}"
        key          = "eks/terraform.tfstate"
        region       = "${var.region}"
        use_lockfile = true
      }
    }
  EOT
}
