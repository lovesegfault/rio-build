# S3 remote state backend. The bucket must exist BEFORE
# `tofu init` — create it via `just eks bootstrap` first.
#
# use_lockfile = true: native S3 state locking (OpenTofu ≥1.6,
# Terraform ≥1.10). Uses S3's conditional-write support — no
# DynamoDB table needed. Two concurrent applies → second one
# fails cleanly with "state is locked".
#
# Migrating from local state (if you already applied before this
# commit landed):
#   tofu init -migrate-state
# Answer "yes" to copy local state to S3. Then delete the local
# terraform.tfstate file.

terraform {
  # bucket + region via -backend-config — `just eks init` computes
  # rio-tfstate-${account_id} from sts (or RIO_TFSTATE_BUCKET /
  # RIO_TFSTATE_REGION from .env.local). Keeps this file free of
  # account-specific literals.
  backend "s3" {
    key          = "eks/terraform.tfstate"
    use_lockfile = true
  }
}
