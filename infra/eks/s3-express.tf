# ADR-023: per-AZ S3 Express One Zone directory buckets — the `local`
# tier of the store's TieredChunkBackend (read-through cache over the
# authoritative chunks bucket in s3.tf).
#
# One directory bucket per supported AZ-ID. The `--<az-id>--x-s3`
# suffix is MANDATORY directory-bucket naming, not a convention; the
# aws-sdk routes requests to the zonal endpoint and switches to
# CreateSession auth based on it.
#
# Everything here is disposable cache state: S3 standard holds every
# chunk byte (r[infra.express.cache-tier]), so force_destroy and
# `tofu destroy` are never data loss.

locals {
  # `use2-az1` (physical, what Express supports and bucket names embed)
  # → `us-east-2a` (logical, what topology.kubernetes.io/zone carries).
  # The letter suffix is account-randomized; zone_ids/names from the
  # same data source are index-aligned, so zipmap is the account-local
  # truth.
  az_id_to_name = zipmap(
    data.aws_availability_zones.available.zone_ids,
    data.aws_availability_zones.available.names,
  )
}

resource "aws_s3_directory_bucket" "express_cache" {
  for_each = toset(var.express_az_ids)

  bucket = "${var.cluster_name}-express-${random_id.bucket_suffix.hex}--${each.key}--x-s3"

  location {
    name = each.key
  }

  # Cache tier — deleting a non-empty bucket loses nothing (read-through
  # refills from S3 standard). Same rationale as s3.tf force_destroy.
  force_destroy = true
}

# 30-day age expiration: defense-in-depth ceiling under the
# application-level eviction sweep (ADR-023). Directory buckets support
# ONLY age-based expiration (no size targets), so the byte budget is
# enforced by the sweeper; this catches orphaned objects if the sweep
# is disabled or the backend is flipped back to `kind: s3`.
resource "aws_s3_bucket_lifecycle_configuration" "express_cache" {
  for_each = aws_s3_directory_bucket.express_cache

  bucket = each.value.bucket

  rule {
    id     = "expire-30d"
    status = "Enabled"

    filter {
      prefix = ""
    }

    expiration {
      days = 30
    }
  }
}
