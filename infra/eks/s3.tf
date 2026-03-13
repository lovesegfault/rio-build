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
