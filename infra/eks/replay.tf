# Build-replay campaign engine (rio-replay Jobs in the rio-replay
# namespace). The engine reads/writes ONLY the replay/ prefix of the
# chunk bucket: replay archives (replay/archives/...) and campaign
# state/reports (replay/campaigns/...). Narrower than the store policy:
# object actions are prefix-scoped. s3:DeleteObject is forward-
# provisioning — the engine has no deletion path today.
#
# The ServiceAccount itself is created by `cargo xtask replay launch`
# (the rio-replay namespace is not a chart namespace); xtask reads
# replay_iam_role_arn from outputs.tf and annotates the SA with it.
data "aws_iam_policy_document" "rio_replay_s3" {
  statement {
    effect = "Allow"
    actions = [
      "s3:GetObject",
      "s3:PutObject",
      "s3:DeleteObject",
    ]
    resources = ["${aws_s3_bucket.chunks.arn}/replay/*"]
  }
  # Unconditional ListBucket on the bucket (rio-scheduler precedent): the
  # engine never lists objects; the grant exists so HEAD/GET of
  # nonexistent replay/ keys return 404 (NoSuchKey/NotFound) instead of
  # 403 — which the engine's first-run probes (archive completion checks,
  # download_state_if_missing) depend on — and a prefix-conditioned grant
  # is not documented to provide that.
  statement {
    effect    = "Allow"
    actions   = ["s3:ListBucket"]
    resources = [aws_s3_bucket.chunks.arn]
  }
}

resource "aws_iam_policy" "rio_replay_s3" {
  name   = "${var.cluster_name}-rio-replay-s3"
  policy = data.aws_iam_policy_document.rio_replay_s3.json
}

module "rio_replay_irsa" {
  source  = "terraform-aws-modules/iam/aws//modules/iam-role-for-service-accounts"
  version = "~> 6.0"

  name = "${var.cluster_name}-rio-replay"

  oidc_providers = {
    eks = {
      provider_arn = module.eks.oidc_provider_arn
      # Trust-policy sub must match the pod's projected token:
      # system:serviceaccount:rio-replay:rio-replay. Drift here →
      # AssumeRoleWithWebIdentity AccessDenied → every S3 read/write
      # from the engine fails.
      namespace_service_accounts = ["rio-replay:rio-replay"]
    }
  }

  policies = {
    s3 = aws_iam_policy.rio_replay_s3.arn
  }
}
