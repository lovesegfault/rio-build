# Stable DNS for the rio-gateway NLB.
#
# `helm uninstall` (and any Service-recreating change) destroys the
# NLB; the replacement gets a fresh `…-<uniqueId>.elb.<region>.
# amazonaws.com` hostname. external-dns watches the Service and keeps
# `gateway_dns.prefix.gateway_dns.zone` pointed at whatever NLB
# currently exists, so an external CNAME (set once) never needs
# updating. Provider is pluggable: route53 (IRSA-auth) or cloudflare
# (API token via k8s Secret). `provider=""` → nothing installed.

locals {
  dns_enabled      = var.gateway_dns.provider != ""
  dns_route53      = var.gateway_dns.provider == "route53"
  dns_cloudflare   = var.gateway_dns.provider == "cloudflare"
  gateway_dns_fqdn = var.gateway_dns.prefix == "" ? var.gateway_dns.zone : "${var.gateway_dns.prefix}.${var.gateway_dns.zone}"
}

# ───────────────────────── route53 ─────────────────────────

# Subdomain-delegation model: the zone IS the FQDN (e.g.
# `gw.rio.example.test`), and the parent zone (wherever the registrar
# is) NS-delegates that one label here. external-dns then writes the
# A/AAAA at the zone apex.
resource "aws_route53_zone" "gateway" {
  count   = local.dns_route53 && var.gateway_dns.create_route53_zone ? 1 : 0
  name    = local.gateway_dns_fqdn
  comment = "rio-gateway NLB ALIAS target (external-dns managed)"
  # Allow tofu destroy even when external-dns has written records.
  # Without this, destroy fails with HostedZoneNotEmpty and requires
  # manual `aws route53 delete-resource-record-sets` first. Same
  # convention as s3.tf / ecr.tf.
  force_destroy = true
}

# create_route53_zone=false → zone must already exist (created
# out-of-band or imported). Looking it up here gives a concrete ARN
# for IRSA scoping AND fails the plan loudly if the precondition
# (zone exists) isn't met — better than external-dns silently
# erroring at runtime.
data "aws_route53_zone" "gateway" {
  count = local.dns_route53 && !var.gateway_dns.create_route53_zone ? 1 : 0
  name  = local.gateway_dns_fqdn
}

locals {
  # Exactly one of resource/data is non-empty when dns_route53.
  gateway_zone_arn = try(
    aws_route53_zone.gateway[0].arn,
    data.aws_route53_zone.gateway[0].arn,
    "",
  )
}

module "external_dns_irsa" {
  count   = local.dns_route53 ? 1 : 0
  source  = "terraform-aws-modules/iam/aws//modules/iam-role-for-service-accounts"
  version = "~> 6.0"

  name = "${var.cluster_name}-external-dns"

  # Module built-in: route53:ChangeResourceRecordSets on the listed
  # zones + route53:ListHostedZones/ListResourceRecordSets on *.
  # Scoped to the one zone above — no `["*"]` write fallback.
  attach_external_dns_policy    = true
  external_dns_hosted_zone_arns = [local.gateway_zone_arn]

  oidc_providers = {
    eks = {
      provider_arn               = module.eks.oidc_provider_arn
      namespace_service_accounts = ["kube-system:external-dns"]
    }
  }
}

# ───────────────────────── cloudflare ─────────────────────────
#
# No terraform cloudflare provider — external-dns talks to the CF API
# itself. Terraform only stashes the token where the pod can read it.

resource "kubernetes_secret_v1" "external_dns_cloudflare" {
  count = local.dns_cloudflare ? 1 : 0
  metadata {
    name      = "external-dns-cloudflare"
    namespace = "kube-system"
  }
  data = {
    apiToken = var.cloudflare_api_token
  }
  depends_on = [helm_release.cilium]
}

# ───────────────────────── controller ─────────────────────────

resource "helm_release" "external_dns" {
  count      = local.dns_enabled ? 1 : 0
  name       = "external-dns"
  namespace  = "kube-system"
  repository = "https://kubernetes-sigs.github.io/external-dns/"
  chart      = "external-dns"
  version    = var.external_dns_version

  # values is a list of YAML docs helm merges in order. Separate docs
  # per provider sidesteps terraform's `cond ? {a,b} : {}` type-unify
  # failure (both ternary arms must share an object schema even before
  # merge() sees them). concat([], cond ? [doc] : []) is list(string)
  # on both arms.
  values = concat(
    [yamlencode({
      # external-dns calls the Route53 provider "aws", not "route53".
      # var.gateway_dns.provider is OUR knob (route53|cloudflare|"");
      # map it to external-dns's vocabulary here.
      provider = { name = local.dns_route53 ? "aws" : var.gateway_dns.provider }
      sources  = ["service"]
      # domainFilters = parent zone (what the provider hosts).
      # external-dns matches the Service's hostname annotation against
      # this suffix; the apex of a subdomain-delegated zone is still
      # under the parent's domainFilter.
      domainFilters = [var.gateway_dns.zone]
      # Ownership TXT: external-dns will only touch records whose
      # paired TXT carries this id. Lets two clusters share a zone
      # without clobbering each other.
      txtOwnerId = var.cluster_name
      # sync (not upsert-only): deletes are scoped by the TXT registry
      # to records carrying owner=txtOwnerId, so manually-added entries
      # in the same zone are untouched. Gains self-cleanup when a
      # Service is renamed/removed.
      policy = "sync"
    })],
    local.dns_route53 ? [yamlencode({
      serviceAccount = {
        annotations = {
          "eks.amazonaws.com/role-arn" = module.external_dns_irsa[0].arn
        }
      }
      env = [{ name = "AWS_DEFAULT_REGION", value = var.region }]
    })] : [],
    local.dns_cloudflare ? [yamlencode({
      env = [{
        name = "CF_API_TOKEN"
        valueFrom = {
          secretKeyRef = {
            name = kubernetes_secret_v1.external_dns_cloudflare[0].metadata[0].name
            key  = "apiToken"
          }
        }
      }]
      # Narrow to one zone when given (CF accounts often hold many).
      extraArgs = var.gateway_dns.cloudflare_zone_id == "" ? [] : [
        "--zone-id-filter=${var.gateway_dns.cloudflare_zone_id}",
      ]
    })] : [],
  )

  depends_on = [helm_release.cilium]
}
