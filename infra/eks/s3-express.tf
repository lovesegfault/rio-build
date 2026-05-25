# Per-AZ S3 Express One Zone directory buckets — the read-through chunk
# cache tier in front of aws_s3_bucket.chunks (ADR-023, P0553).
#
# r[impl infra.express.cache-tier]
# (documentary marker — tracey does not scan .tf, so this is not a
# scannable impl site and the rule currently has none anywhere; it
# stays listed by `tracey query uncovered` until the main line puts
# the marker on the consuming code, rio-store's
# ChunkBackendKind::Tiered / TieredChunkBackend. The spec text lives
# in ADR-023, tiered chunk backend.)
#
# One bucket per AZ that (a) hosts a cluster subnet AND (b) is in the
# S3-Express-supported AZ-ID set. Store replicas in an AZ with a bucket
# read through it; replicas in an AZ without one degrade to direct
# S3-standard reads (TieredChunkBackend local=None) — slower, not down.
# No CSI driver, no PVC, no Lustre kernel module: both tiers are plain
# S3 clients, the Express tier is just addressed by a `--x-s3` bucket
# name (the aws-sdk routes those to the zonal s3express endpoint and
# uses the CreateSession auth flow automatically).
#
# Everything in this file no-ops when the intersection is empty
# (for_each over an empty set / count 0) — set express_supported_az_ids
# = [] to disable the cache tier cluster-wide without touching any
# other resource.

locals {
  # Zone-name ↔ zone-ID mapping for the AZs the VPC actually uses.
  # module.vpc is given `slice(...names, 0, 3)`; the AWS API returns
  # `names` and `zone_ids` as parallel index-aligned lists, so the same
  # slice of zone_ids is exactly the cluster's physical AZ set. The
  # letter suffix (us-east-2a) is account-randomized; the ID (use2-az1)
  # is physical — Express availability and bucket placement are both
  # keyed by ID.
  #
  # NOTE: this re-derives the `slice(..., 0, 3)` expression main.tf
  # passes to module.vpc (the module echoes back names via
  # module.vpc.azs but has no zone-id output, so reading it would not
  # remove the duplication) — if the AZ count or selection in main.tf
  # ever changes, change it here too.
  cluster_az_names   = slice(data.aws_availability_zones.available.names, 0, 3)
  cluster_az_ids     = slice(data.aws_availability_zones.available.zone_ids, 0, 3)
  zone_name_by_az_id = zipmap(local.cluster_az_ids, local.cluster_az_names)

  # AZs that get a directory bucket: cluster AZs ∩ Express-supported
  # set. Empty → cache tier disabled cluster-wide (no buckets, no IAM
  # policy, helm keeps store.chunkBackend.kind=s3).
  express_az_ids = [
    for id in local.cluster_az_ids : id if contains(var.express_supported_az_ids, id)
  ]
}

# Directory bucket names are NOT globally unique (unlike S3 standard)
# — they are unique per account+region — but two rio clusters in the
# same account+region would still collide, so the base name carries
# cluster_name like every other resource here. The `--<az-id>--x-s3`
# suffix is mandatory and must match `location.name`.
#
# cluster_name is capped at 31 chars (variables.tf validation), so the
# longest possible name is 31 + len("-chunk-cache--apse1-az99--x-s3")
# = 61 ≤ 63.
resource "aws_s3_directory_bucket" "cache" {
  for_each = toset(local.express_az_ids)

  bucket = "${var.cluster_name}-chunk-cache--${each.key}--x-s3"

  location {
    name = each.key
    type = "AvailabilityZone"
  }

  # Cache tier: every object is reproducible from S3 standard (Express
  # is filled only by read-through). Deleting a non-empty cache bucket
  # on `tofu destroy` is always safe.
  force_destroy = true
}

# Defense-in-depth age ceiling. Directory buckets support ONLY
# age-based expiration (no transitions, no size targets), so this
# cannot enforce the 8 TiB working-set bound — the authoritative
# size-target sweep is the application-level eviction loop (P0585,
# r[infra.express.bounded-eviction]). What this DOES guarantee: a
# chunk that no replica in this AZ has cold-missed for 30 days is
# deleted even if the sweep is broken or not yet deployed, so a dead
# cluster's cache bill decays to zero instead of growing forever.
resource "aws_s3_bucket_lifecycle_configuration" "cache" {
  for_each = aws_s3_directory_bucket.cache

  bucket = each.value.bucket

  rule {
    id     = "expire-stale-cache"
    status = "Enabled"

    filter {
      prefix = ""
    }

    expiration {
      days = 30
    }
  }
}

# Lifecycle on directory buckets is performed by the
# lifecycle.s3.amazonaws.com service principal creating its own
# ReadWrite session against the bucket — without this bucket policy the
# configuration above is accepted but never executes. aws:SourceAccount
# scopes the grant to lifecycle rules owned by this account (confused-
# deputy guard).
resource "aws_s3_bucket_policy" "cache_lifecycle" {
  for_each = aws_s3_directory_bucket.cache

  bucket = each.value.bucket

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid       = "LifecycleExpiration"
        Effect    = "Allow"
        Principal = { Service = "lifecycle.s3.amazonaws.com" }
        Action    = "s3express:CreateSession"
        Resource  = each.value.arn
        Condition = {
          StringEquals = {
            "s3express:SessionMode" = "ReadWrite"
            "aws:SourceAccount"     = data.aws_caller_identity.current.account_id
          }
        }
      }
    ]
  })
}

# Store IRSA grant for the cache tier. Directory-bucket auth is
# session-based: the principal needs ONLY s3express:CreateSession on
# the bucket — the returned session token then authorizes all
# object-level operations (GET/PUT/DELETE/LIST) within it. No
# `s3express:*`: that would additionally grant DeleteBucket /
# PutBucketPolicy / PutLifecycleConfiguration, i.e. a compromised
# store pod could delete the cache buckets or rewrite the lifecycle
# grant above. Bucket management stays terraform-only.
#
# Standalone attachment (not an entry in module.rio_store_irsa's
# `policies` map) so the whole Express feature lives in this one file
# and disabling it (empty express_az_ids → count 0) leaves the base
# S3-standard policy untouched.
data "aws_iam_policy_document" "rio_store_s3express" {
  count = length(local.express_az_ids) > 0 ? 1 : 0

  statement {
    sid       = "ExpressCacheSession"
    effect    = "Allow"
    actions   = ["s3express:CreateSession"]
    resources = [for b in aws_s3_directory_bucket.cache : b.arn]
  }
}

resource "aws_iam_policy" "rio_store_s3express" {
  count = length(local.express_az_ids) > 0 ? 1 : 0

  name   = "${var.cluster_name}-rio-store-s3express"
  policy = data.aws_iam_policy_document.rio_store_s3express[0].json
}

resource "aws_iam_role_policy_attachment" "rio_store_s3express" {
  count = length(local.express_az_ids) > 0 ? 1 : 0

  role       = module.rio_store_irsa.name
  policy_arn = aws_iam_policy.rio_store_s3express[0].arn
}
