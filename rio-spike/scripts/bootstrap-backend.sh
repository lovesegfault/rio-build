#!/usr/bin/env bash
#
# bootstrap-backend.sh — Create the S3 bucket for Terraform state
#
# Run this once before the first `terraform init`. Idempotent.
#
# Usage:
#   AWS_PROFILE=beme_sandbox ./rio-spike/scripts/bootstrap-backend.sh
#

set -euo pipefail

export AWS_PROFILE="${AWS_PROFILE:-beme_sandbox}"
REGION="${AWS_REGION:-us-east-2}"
BUCKET="rio-spike-tfstate"

echo "[bootstrap] checking S3 bucket: $BUCKET (region: $REGION, profile: $AWS_PROFILE)"

if aws s3api head-bucket --bucket "$BUCKET" 2>/dev/null; then
    echo "[bootstrap] bucket already exists"
else
    echo "[bootstrap] creating bucket..."
    aws s3api create-bucket \
        --bucket "$BUCKET" \
        --region "$REGION" \
        --create-bucket-configuration LocationConstraint="$REGION"

    # Enable versioning (state recovery)
    aws s3api put-bucket-versioning \
        --bucket "$BUCKET" \
        --versioning-configuration Status=Enabled

    # Block public access
    aws s3api put-public-access-block \
        --bucket "$BUCKET" \
        --public-access-block-configuration \
            BlockPublicAcls=true,IgnorePublicAcls=true,BlockPublicPolicy=true,RestrictPublicBuckets=true

    # Server-side encryption
    aws s3api put-bucket-encryption \
        --bucket "$BUCKET" \
        --server-side-encryption-configuration \
            '{"Rules":[{"ApplyServerSideEncryptionByDefault":{"SSEAlgorithm":"AES256"},"BucketKeyEnabled":true}]}'

    echo "[bootstrap] bucket created with versioning, encryption, and public access block"
fi

echo "[bootstrap] done. Run: cd rio-spike/terraform && terraform init"
