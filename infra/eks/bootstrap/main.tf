# Bootstrap: the S3 bucket that holds terraform state for infra/eks
# AND for this module itself (self-referential).
#
# Chicken-and-egg solved by `cargo xtask k8s -p eks up --bootstrap`:
# it checks whether the state object already exists in S3. If not
# (first time), it inits with -backend=false (local state), applies
# to create the bucket, then migrates local → S3. If yes, normal
# init + apply. Idempotent from any machine, nothing state-like is
# committed.
#
# OpenTofu ≥1.6 (and Terraform ≥1.10) support native S3 state
# locking via the bucket's own object lock — no DynamoDB table
# needed. We enable versioning + object lock here; both backend
# blocks (here and in infra/eks/backend.tf) set `use_lockfile = true`.
#
# bucket_name and region are REQUIRED vars, always passed by xtask
# (computed from sts, or RIO_TFSTATE_* in .env.local). No defaults
# here — single source of truth in xtask, so backend config and the
# created bucket can't drift apart.

terraform {
  required_version = ">= 1.6"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 6.0"
    }
  }

  # Self-referential: this bucket IS aws_s3_bucket.state below.
  # bucket + region are passed via -backend-config (xtask computes
  # rio-tfstate-${account_id} from sts, or reads RIO_TFSTATE_BUCKET /
  # RIO_TFSTATE_REGION from .env.local). See header comment for
  # first-time-setup bootstrap dance.
  backend "s3" {
    key          = "bootstrap/terraform.tfstate"
    use_lockfile = true
  }
}

# No defaults: xtask always passes these (sts-computed, or
# RIO_TFSTATE_* from .env.local). See header comment.
variable "region" {
  type = string
}

variable "bucket_name" {
  description = "State bucket name. Must be globally unique."
  type        = string
}

provider "aws" {
  region = var.region
}

resource "aws_s3_bucket" "state" {
  bucket = var.bucket_name

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
  description = "State bucket name (xtask computes this from account ID, but the output is handy for verification)"
  value       = aws_s3_bucket.state.bucket
}
