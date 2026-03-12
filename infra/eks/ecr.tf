# ECR repositories for the 5 rio images. `nix run .#push-images`
# builds each via nix/docker.nix and skopeo-copies here with a
# git-SHA tag.

locals {
  rio_images = ["gateway", "scheduler", "store", "controller", "worker"]
}

resource "aws_ecr_repository" "rio" {
  for_each = toset(local.rio_images)
  name     = "rio-${each.key}"

  # Immutable tags: a given tag can only be pushed once. Pushing
  # the same git SHA twice fails with ImageTagAlreadyExistsException.
  # This is what you want — a tag IS a commit, it shouldn't change.
  # If you need to re-push (e.g., after a flake.lock bump that
  # changed the closure), make a new commit.
  image_tag_mutability = "IMMUTABLE"

  # Scan on push: free AWS ECR vulnerability scanning. Scans the
  # image layers against CVE databases. Results show in the ECR
  # console + `aws ecr describe-image-scan-findings`. Not a gate
  # (doesn't block the push) but good to have.
  image_scanning_configuration {
    scan_on_push = true
  }
}

# Lifecycle: keep last 10 images per repo. git-SHA tags accumulate
# forever otherwise. 10 is plenty for "roll back to last week" and
# keeps ECR storage costs bounded (each rio image is ~50-150MB;
# 5 repos × 10 images × 150MB ≈ 7.5GB worst case).
#
# Rule matches all tags (tagStatus: any). ECR lifecycle rules
# can't filter by tag prefix within a single rule — if you want
# "keep all `v*` tags, prune `sha-*` tags", you need two rules
# with different priorities. For now, one rule keeps it simple.
resource "aws_ecr_lifecycle_policy" "rio" {
  for_each   = aws_ecr_repository.rio
  repository = each.value.name

  policy = jsonencode({
    rules = [{
      rulePriority = 1
      description  = "Keep last 10 images"
      selection = {
        tagStatus   = "any"
        countType   = "imageCountMoreThan"
        countNumber = 10
      }
      action = { type = "expire" }
    }]
  })
}

# Convenience: the `<account>.dkr.ecr.<region>.amazonaws.com` prefix.
# Every skopeo/kustomize/kubectl-set-image invocation needs this;
# deriving it once here keeps the output clean.
data "aws_caller_identity" "current" {}

locals {
  ecr_registry = "${data.aws_caller_identity.current.account_id}.dkr.ecr.${var.region}.amazonaws.com"
}
