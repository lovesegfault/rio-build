# S3 bucket for NAR chunk storage. Previously a manual prerequisite
# ("aws s3 mb s3://..." before terraform apply) — now terraform-managed
# so `tofu apply` is the only bring-up step.

# Random suffix: S3 bucket names are globally unique. Two people
# deploying rio-build with the same cluster_name in different AWS
# accounts would collide without this. 8 hex chars = 4 billion
# possibilities; enough.
resource "random_id" "bucket_suffix" {
  byte_length = 4
}

resource "aws_s3_bucket" "chunks" {
  bucket = "${var.cluster_name}-chunks-${random_id.bucket_suffix.hex}"

  # force_destroy: `tofu destroy` deletes even non-empty buckets.
  # Without this, destroy fails if any chunks exist — fine for
  # prod (you WANT that protection), annoying for a dev/test
  # cluster. This is a dev/test deployment (beme_sandbox), so
  # delete-with-contents is the right default. For prod, override
  # via a tfvars file.
  force_destroy = true
}

# Block all public access. rio-store reads/writes via IRSA-assumed
# IAM role; nothing public touches this bucket.
resource "aws_s3_bucket_public_access_block" "chunks" {
  bucket                  = aws_s3_bucket.chunks.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

# Versioning: off. NAR chunks are content-addressed (the object key
# IS the blake3 hash of the content) — there's no "new version of
# the same chunk". A chunk either exists with exactly its expected
# content or it doesn't. Versioning would just accumulate delete
# markers when GC runs.

# Build-log expiry backstop. rio-store's hourly TTL sweep is the primary
# retention mechanism for logs/ (it deletes the PG manifest rows and
# their S3 objects together, default 30 days — RIO_LOG_RETENTION_DAYS).
# This rule exists for the objects the sweep can never find because no
# manifest row references them:
#   - a chunk PUT that succeeded but whose manifest INSERT never ran
#     (store crash between the two; the object is invisible to readers),
#   - the pre-cutover .log.zst / .partial.log.zst blobs that migration
#     063's DROP TABLE drv_logs orphaned.
# 37 = the 30-day default retention + 7 days of headroom so the rule
# only ever collects objects the sweep has already had every chance to
# delete — it must never race the sweep on a still-referenced chunk.
# If RIO_LOG_RETENTION_DAYS is ever raised above 30, raise this too.
resource "aws_s3_bucket_lifecycle_configuration" "chunks" {
  bucket = aws_s3_bucket.chunks.id

  rule {
    id     = "expire-build-logs"
    status = "Enabled"

    filter {
      prefix = "logs/"
    }

    expiration {
      days = 37
    }
  }
}
