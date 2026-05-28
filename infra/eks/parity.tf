# nixpkgs-parity campaign engine (rio-parity Jobs in the rio-parity
# namespace). The engine reads/writes ONLY the parity/ prefix of the
# chunk bucket: eval sets (parity/evals/...) and campaign state/reports
# (parity/campaigns/...). Narrower than the store policy: object actions
# are prefix-scoped, ListBucket is condition-scoped to the same prefix.
#
# The ServiceAccount itself is created by `cargo xtask parity launch`
# (the rio-parity namespace is not a chart namespace); xtask reads
# parity_iam_role_arn from outputs.tf and annotates the SA with it.
data "aws_iam_policy_document" "rio_parity_s3" {
  statement {
    effect = "Allow"
    actions = [
      "s3:GetObject",
      "s3:PutObject",
      "s3:DeleteObject",
    ]
    resources = ["${aws_s3_bucket.chunks.arn}/parity/*"]
  }
  statement {
    effect    = "Allow"
    actions   = ["s3:ListBucket"]
    resources = [aws_s3_bucket.chunks.arn]
    condition {
      test     = "StringLike"
      variable = "s3:prefix"
      values   = ["parity/*"]
    }
  }
}

resource "aws_iam_policy" "rio_parity_s3" {
  name   = "${var.cluster_name}-rio-parity-s3"
  policy = data.aws_iam_policy_document.rio_parity_s3.json
}

module "rio_parity_irsa" {
  source  = "terraform-aws-modules/iam/aws//modules/iam-role-for-service-accounts"
  version = "~> 6.0"

  name = "${var.cluster_name}-rio-parity"

  oidc_providers = {
    eks = {
      provider_arn = module.eks.oidc_provider_arn
      # Trust-policy sub must match the pod's projected token:
      # system:serviceaccount:rio-parity:rio-parity. Drift here →
      # AssumeRoleWithWebIdentity AccessDenied → every S3 read/write
      # from the engine fails.
      namespace_service_accounts = ["rio-parity:rio-parity"]
    }
  }

  policies = {
    s3 = aws_iam_policy.rio_parity_s3.arn
  }
}
