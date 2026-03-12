# S3 remote state backend. The bucket must exist BEFORE
# `tofu init` — create it via infra/bootstrap/ first.
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
  backend "s3" {
    # Set this after running infra/bootstrap. The bucket name
    # defaults to rio-tfstate-<account-id> there; paste the
    # output here. tofu init will prompt if this is empty.
    #
    # Alternative: `tofu init -backend-config=bucket=<name>` to
    # pass it on the command line (keeps this file generic).
    bucket = ""

    key          = "eks/terraform.tfstate"
    region       = "us-east-2"
    use_lockfile = true
  }
}
