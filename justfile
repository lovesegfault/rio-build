set dotenv-load := true
set dotenv-filename := ".env.local"
set shell := ["bash", "-euo", "pipefail", "-c"]

# `just` alone = menu, not a surprise build
default:
    @just --list

# ── State backend ────────────────────────────────────────────────────
# Both infra/eks/bootstrap and infra/eks store state in the same S3
# bucket (bootstrap is self-referential — it manages the bucket
# it stores its own state in). Bucket + region are passed via
# -backend-config so nothing account-specific is committed.
#
# Defaults: rio-tfstate-${account_id}, us-east-2. Override via
# RIO_TFSTATE_BUCKET / RIO_TFSTATE_REGION in .env.local.

[private]
_tfstate-bucket:
    @echo "${RIO_TFSTATE_BUCKET:-rio-tfstate-$(aws sts get-caller-identity --query Account --output text)}"

[private]
_tfstate-region:
    @echo "${RIO_TFSTATE_REGION:-us-east-2}"

# -reconfigure: tofu can't tell the dynamic -backend-config is the
# same as last time, prompts "migrate?" even though nothing changed.

[private]
_tfstate-init DIR:
    tofu -chdir={{DIR}} init -reconfigure \
      -backend-config="bucket=$(just _tfstate-bucket)" \
      -backend-config="region=$(just _tfstate-region)"

# ── EKS bootstrap (S3 state bucket — once per account) ──────────────
# Self-referential: state lives in the bucket this creates. The
# chicken-and-egg is solved by detecting whether the state object
# exists in S3: if not, init with -backend=false (local state),
# apply to create the bucket, then migrate local → S3. Idempotent
# — first run does the dance, subsequent runs are a normal apply.

# Create/update the S3 state bucket (handles first-time setup)
eks-bootstrap:
    #!/usr/bin/env bash
    set -euo pipefail
    bucket="$(just _tfstate-bucket)"
    region="$(just _tfstate-region)"
    if aws s3api head-object --bucket "$bucket" --key bootstrap/terraform.tfstate >/dev/null 2>&1; then
      echo "[bootstrap] state exists at s3://$bucket/bootstrap/ — normal apply"
      just _tfstate-init infra/eks/bootstrap
      tofu -chdir=infra/eks/bootstrap apply -var="bucket_name=$bucket" -var="region=$region"
    else
      echo "[bootstrap] no state in S3 — first-time setup (local apply → migrate)"
      # -backend=false: skip S3 backend, use local state. -reconfigure
      # clears any previous .terraform/ that might point at S3.
      tofu -chdir=infra/eks/bootstrap init -backend=false -reconfigure
      tofu -chdir=infra/eks/bootstrap apply -var="bucket_name=$bucket" -var="region=$region"
      echo "[bootstrap] bucket created — migrating local state → S3"
      tofu -chdir=infra/eks/bootstrap init -migrate-state -force-copy \
        -backend-config="bucket=$bucket" -backend-config="region=$region"
      rm -f infra/eks/bootstrap/terraform.tfstate infra/eks/bootstrap/terraform.tfstate.backup
      echo "[bootstrap] done — state at s3://$bucket/bootstrap/terraform.tfstate"
    fi

# ── EKS ──────────────────────────────────────────────────────────────

# tofu's own "No outputs found" sounds like a config problem, not
# a "run apply first" problem. Wrap with a useful error.

[private]
_eks-out NAME:
    @tofu -chdir=infra/eks output -raw {{NAME}} 2>/dev/null || \
      { echo "error: tofu output '{{NAME}}' missing — run 'just eks-apply' first?" >&2; exit 1; }

# Init infra/eks backend (bucket from account ID or .env.local)
eks-init: (_tfstate-init "infra/eks")

# tofu apply (prompts for confirmation)
eks-apply: eks-init
    tofu -chdir=infra/eks apply

# tofu apply -auto-approve
eks-apply-auto: eks-init
    tofu -chdir=infra/eks apply -auto-approve

# Configure kubectl for the EKS cluster
eks-kubeconfig:
    $(just _eks-out kubeconfig_command)

# Build docker images + push to ECR (zstd layers, git-SHA tag)
eks-push:
    ./infra/eks/push-images.sh

# Render kustomize overlay + kubectl apply
eks-deploy:
    ./infra/eks/deploy.sh

# End-to-end smoke test (builds nixpkgs#hello, kills a worker, asserts reassign)
eks-smoke:
    ./infra/eks/smoke-test.sh

[private]
_eks-up APPLY:
    just {{APPLY}}
    just eks-kubeconfig
    just eks-push
    just eks-deploy

# Full bring-up: apply (prompts) → kubeconfig → push → deploy
eks-up: (_eks-up "eks-apply")

# Full bring-up, auto-approves apply
eks-up-auto: (_eks-up "eks-apply-auto")

# WorkerPool finalizers hold pods → NLB → tofu destroy blocks.
eks-destroy:
    kubectl -n rio-system delete workerpool --all --wait=true --ignore-not-found
    tofu -chdir=infra/eks destroy
