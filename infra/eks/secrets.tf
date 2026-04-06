# External Secrets Operator + bootstrap Job IRSA.
#
# Secrets flow:
#   1. terraform → Aurora creates master password in Secrets Manager
#   2. rio-bootstrap Job (helm pre-install hook) → generates rio/hmac +
#      rio/signing-key + rio/signing-key-pub in Secrets Manager, idempotent
#      via describe-secret guard
#   3. ESO → syncs all of the above into k8s Secrets in rio-system
#
# `just eks deploy` reads the IRSA ARNs + Aurora ARN/endpoint from tofu
# outputs and passes them as helm --set args.

# ────────────────────────────────────────────────────────────────────────
# External Secrets Operator
# ────────────────────────────────────────────────────────────────────────

module "eso_irsa" {
  source  = "terraform-aws-modules/iam/aws//modules/iam-role-for-service-accounts"
  version = "~> 6.0"

  name = "${var.cluster_name}-eso"

  # GetSecretValue + DescribeSecret on the Aurora master password secret
  # AND the rio/* secrets that the bootstrap Job creates. Secrets Manager
  # ARNs have a random 6-char suffix, so the resource pattern needs a
  # trailing wildcard on the prefix.
  policies = {
    eso = aws_iam_policy.eso.arn
  }

  oidc_providers = {
    eks = {
      provider_arn               = module.eks.oidc_provider_arn
      namespace_service_accounts = ["external-secrets:external-secrets"]
    }
  }
}

resource "aws_iam_policy" "eso" {
  name = "${var.cluster_name}-eso"
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect = "Allow"
        Action = [
          "secretsmanager:GetSecretValue",
          "secretsmanager:DescribeSecret",
        ]
        Resource = [
          aws_rds_cluster.rio.master_user_secret[0].secret_arn,
          # rio/hmac, rio/signing-key, rio/signing-key-pub — created by
          # the bootstrap Job. -?????? is Secrets Manager's random suffix.
          "arn:aws:secretsmanager:${var.region}:${data.aws_caller_identity.current.account_id}:secret:rio/*",
        ]
      }
    ]
  })
}

resource "helm_release" "external_secrets" {
  name             = "external-secrets"
  namespace        = "external-secrets"
  create_namespace = true
  repository       = "https://charts.external-secrets.io"
  chart            = "external-secrets"
  # Chart renumbered 0.x→1.x→2.x to align with app version. Main breaking
  # change in this range was v1beta1 API removal at v0.17 — our CRDs'
  # storedVersions are already ["v1"] only and the rio chart's
  # ExternalSecret/ClusterSecretStore manifests use external-secrets.io/v1,
  # so no migration needed. installCRDs defaults true (helm-managed).
  version = "2.2.0"

  # IRSA annotation on the chart's SA. The chart creates the SA; we just
  # annotate it (unlike aws-lbc where we created the SA ourselves).
  set = [
    {
      name  = "serviceAccount.annotations.eks\\.amazonaws\\.com/role-arn"
      value = module.eso_irsa.arn
    }
  ]

  # aws_lbc dep: webhook-ordering only — see addons.tf cert_manager.
  depends_on = [module.eks, helm_release.aws_lbc]
}

# ────────────────────────────────────────────────────────────────────────
# Bootstrap Job IRSA
# ────────────────────────────────────────────────────────────────────────
# The rio-bootstrap SA (in the chart) gets this role. The Job creates
# rio/hmac + rio/signing-key + rio/signing-key-pub in Secrets Manager on
# first install. describe-secret guard → idempotent.

module "rio_bootstrap_irsa" {
  source  = "terraform-aws-modules/iam/aws//modules/iam-role-for-service-accounts"
  version = "~> 6.0"

  name = "${var.cluster_name}-rio-bootstrap"

  policies = {
    bootstrap = aws_iam_policy.rio_bootstrap.arn
  }

  oidc_providers = {
    eks = {
      provider_arn               = module.eks.oidc_provider_arn
      namespace_service_accounts = ["rio-system:rio-bootstrap"]
    }
  }
}

resource "aws_iam_policy" "rio_bootstrap" {
  name = "${var.cluster_name}-rio-bootstrap"
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect = "Allow"
        Action = [
          "secretsmanager:CreateSecret",
          "secretsmanager:PutSecretValue",
          "secretsmanager:DescribeSecret",
        ]
        # Scope to rio/* only. The bootstrap script only writes rio/hmac,
        # rio/signing-key, rio/signing-key-pub.
        Resource = "arn:aws:secretsmanager:${var.region}:${data.aws_caller_identity.current.account_id}:secret:rio/*"
      }
    ]
  })
}
