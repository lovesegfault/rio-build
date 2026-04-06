# ECR repositories for the rio images. push-images.sh builds each via
# nix/docker.nix and skopeo-copies here with a git-SHA tag. `bootstrap`
# is the helm pre-install hook Job image (awscli + openssl + nix for
# seeding rio/* secrets in Secrets Manager).

locals {
  # Must stay in sync with nix/docker.nix — push-images.sh globs the
  # linkFarm and pushes everything, so a drift shows up as "repository
  # does not exist" at push time.
  rio_images = [
    "gateway", "scheduler", "store", "controller", "builder",
    "fetcher", "bootstrap", "seccomp-bootstrap", "dashboard", "all",
  ]
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

  # Allow tofu destroy even when images exist. Repo renames (e.g.
  # worker→builder) otherwise fail with RepositoryNotEmptyException
  # and require manual `aws ecr delete-repository --force`.
  force_delete = true

  # Scan on push: free AWS ECR vulnerability scanning. Scans the
  # image layers against CVE databases. Results show in the ECR
  # console + `aws ecr describe-image-scan-findings`. Not a gate
  # (doesn't block the push) but good to have.
  image_scanning_configuration {
    scan_on_push = true
  }
}

# Lifecycle: keep last 30 images per repo. git-SHA tags accumulate
# forever otherwise. Multi-arch pushes produce 3 tags per logical
# version (`-amd64`, `-arm64`, and the manifest index), so 30
# images ≈ 10 versions — enough for "roll back to last week"
# while keeping ECR storage bounded (each rio image is ~50-150MB;
# 9 repos × 30 images × 150MB ≈ 40GB worst case).
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
      description  = "Keep last 30 images (10 multi-arch versions × 3 tags)"
      selection = {
        tagStatus   = "any"
        countType   = "imageCountMoreThan"
        countNumber = 30
      }
      action = { type = "expire" }
    }]
  })
}

# Convenience: the `<account>.dkr.ecr.<region>.amazonaws.com` prefix.
# Every skopeo/helm invocation needs this; deriving it once here keeps
# the output clean.
data "aws_caller_identity" "current" {}
data "aws_partition" "current" {}

locals {
  ecr_registry = "${data.aws_caller_identity.current.account_id}.dkr.ecr.${var.region}.amazonaws.com"
}
