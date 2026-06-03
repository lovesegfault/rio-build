# External Secrets Operator + bootstrap Job IRSA.
#
# Secrets flow:
#   1. terraform → Aurora creates master password in Secrets Manager
#   2. rio-bootstrap Job (helm pre-install hook) → generates rio/hmac +
#      rio/signing-key + rio/signing-key-pub in Secrets Manager, idempotent
#      via describe-secret guard
#   3. ESO → syncs all of the above into k8s Secrets in rio-system
#
# xtask reads the IRSA ARNs + Aurora ARN/endpoint from tofu outputs
# and passes them as helm --set args.

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
  version = "2.3.0"

  # IRSA annotation on the chart's SA. The chart creates the SA; we just
  # annotate it (unlike aws-lbc where we created the SA ourselves).
  set = [
    {
      name  = "serviceAccount.annotations.eks\\.amazonaws\\.com/role-arn"
      value = module.eso_irsa.arn
    },
    # hostNetwork: EKS managed API server can't route to overlay pod IPs
    # (Cilium cluster-pool fd42::) for admission webhook calls →
    # "Address is not allowed". hostNetwork puts the webhook on a node
    # VPC IP — and turns every listener (webhook, metrics, readyz) into
    # a host-port claim. The previous 10260/8080/8081 collided with
    # prom-operator (10260) and aws-lbc metrics (8080), so the webhook
    # sat Pending whenever no node had all three free. 9445-9447 sit in
    # the admission-webhook block next to aws-lbc (9443) and KEDA
    # (9444), inside the 9443-10260 control-plane→node SG rule. Full
    # allocation table: main.tf webhooks_from_control_plane.
    {
      name  = "webhook.hostNetwork"
      value = "true"
    },
    {
      name  = "webhook.port"
      value = "9445"
    },
    {
      name  = "webhook.metrics.listen.port"
      value = "9446"
    },
    {
      name  = "webhook.readinessProbe.port"
      value = "9447"
    },
  ]

  # aws_lbc dep: webhook-ordering only — see addons.tf aws_lbc.
  # cilium dep: CNI must be up or pods Pending → wait=true times out.
  depends_on = [helm_release.aws_lbc, helm_release.cilium]
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
          "secretsmanager:DescribeSecret",
        ]
        # Scope to rio/* only. The bootstrap script probes and creates
        # rio/hmac, rio/service-hmac, rio/signing-key{,-pub}, and
        # rio/gateway-host-key. Create-only is the concurrency CAS:
        # CreateSecret on an existing secret fails
        # ResourceExistsException and can never overwrite — which is
        # exactly why this statement does NOT carry PutSecretValue
        # (see the dedicated statement below).
        Resource = "arn:aws:secretsmanager:${var.region}:${data.aws_caller_identity.current.account_id}:secret:rio/*"
      },
      {
        Effect = "Allow"
        # GetSecretValue: the signing-key block's steady-state
        # pair-consistency probe and the pub re-derive/heal paths
        # read rio/signing-key{,-pub} on every run (the derivation
        # itself happens in rio-cli; the shell never decodes key
        # bytes). Without this grant the re-derive branch
        # AccessDenied-aborts fail-closed — visible, but the pair
        # never converges. Apply terraform BEFORE helm install.
        # Kept in lockstep with the script by the
        # bootstrap-iam-parity check (nix/misc-checks.nix), which
        # pins every (action, resource) pair against the script's
        # per-verb target set.
        #
        # Read access is confined to the signing-key pair — the only
        # secrets the script ever READS. A compromised bootstrap pod
        # cannot read rio/hmac, rio/service-hmac, or the gateway host
        # key. `rio/signing-key*` covers both names plus Secrets
        # Manager's random ARN suffix.
        Action   = ["secretsmanager:GetSecretValue"]
        Resource = "arn:aws:secretsmanager:${var.region}:${data.aws_caller_identity.current.account_id}:secret:rio/signing-key*"
      },
      {
        Effect = "Allow"
        # PutSecretValue: the ONLY secret the script ever OVERWRITES
        # is the derived public half (the heal and stale-pub paths).
        # Confined so a compromised bootstrap pod cannot stage
        # attacker-known versions of rio/hmac, rio/service-hmac,
        # rio/signing-key, or the gateway host key for ESO to sync
        # into the cluster (round-17 merged_bug_013 — the write-side
        # twin of the round-16 GetSecretValue confinement; an
        # overwritten HMAC would let the attacker mint valid tokens,
        # strictly stronger than the read that commit closed).
        #
        # RESIDUAL (enumerated, accepted): this pod CAN overwrite the
        # pub half (pub poisoning). Bounded three ways: the poisoned
        # pub diverges from the private half, so every signature
        # verified against it FAILS — a loud denial, never a silent
        # trust grant; the next Job run's pair-consistency probe
        # detects and heals the divergence; and the attacker cannot
        # make the poison self-consistent without the private half,
        # which this role cannot read.
        Action   = ["secretsmanager:PutSecretValue"]
        Resource = "arn:aws:secretsmanager:${var.region}:${data.aws_caller_identity.current.account_id}:secret:rio/signing-key-pub*"
      }
    ]
  })
}
