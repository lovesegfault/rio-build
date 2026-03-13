# EKS deployment recipes. Invoked as `just eks <recipe>`.
#
# Settings do NOT inherit from the root justfile into modules — both
# `set shell` (pipefail) and `set dotenv-*` (RIO_SSH_PUBKEY, RIO_TFSTATE_*)
# must be re-declared here or they're silently off.
set shell := ["bash", "-euo", "pipefail", "-c"]
set dotenv-load := true
set dotenv-filename := ".env.local"

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
      -backend-config="bucket=$(just eks::_tfstate-bucket)" \
      -backend-config="region=$(just eks::_tfstate-region)"

# ── Bootstrap (S3 state bucket — once per account) ───────────────────
# Self-referential: state lives in the bucket this creates. The
# chicken-and-egg is solved by detecting whether the state object
# exists in S3: if not, init with -backend=false (local state),
# apply to create the bucket, then migrate local → S3. Idempotent
# — first run does the dance, subsequent runs are a normal apply.

# Create/update the S3 state bucket (handles first-time setup)
bootstrap:
    #!/usr/bin/env bash
    set -euo pipefail
    bucket="$(just eks::_tfstate-bucket)"
    region="$(just eks::_tfstate-region)"
    if aws s3api head-object --bucket "$bucket" --key bootstrap/terraform.tfstate >/dev/null 2>&1; then
      echo "[bootstrap] state exists at s3://$bucket/bootstrap/ — normal apply"
      just eks::_tfstate-init infra/eks/bootstrap
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

# ── Deploy ───────────────────────────────────────────────────────────

# tofu's own "No outputs found" sounds like a config problem, not
# a "run apply first" problem. Wrap with a useful error.

[private]
_tf-out NAME:
    @tofu -chdir=infra/eks output -raw {{NAME}} 2>/dev/null || \
      { echo "error: tofu output '{{NAME}}' missing — run 'just eks apply' first?" >&2; exit 1; }

# Init infra/eks backend (bucket from account ID or .env.local)
init: (_tfstate-init "infra/eks")

# tofu apply (prompts for confirmation)
apply: init
    tofu -chdir=infra/eks apply

# tofu apply -auto-approve
apply-auto: init
    tofu -chdir=infra/eks apply -auto-approve

# Configure kubectl for the EKS cluster
kubeconfig:
    $(just eks::_tf-out kubeconfig_command)

# Build docker images + push to ECR (zstd layers, git-SHA tag)
push:
    ./infra/eks/push-images.sh

# Render infra values from tofu outputs + helm upgrade from the WORKING
# TREE. No git involved — chart changes on a dirty tree deploy directly.
# Image tag comes from .rio-image-tag (push-images.sh writes it).
#
# This is the same information the old ApplicationSet's inline `values:`
# block templated from cluster Secret annotations, delivered via --set
# instead. Push-model: iterate without commit+push.
deploy:
    #!/usr/bin/env bash
    set -euo pipefail
    [[ -f .rio-image-tag ]] || { echo "no .rio-image-tag — run 'just eks push' first" >&2; exit 1; }
    tag=$(<.rio-image-tag)

    ecr=$(just eks::_tf-out ecr_registry)
    bucket=$(just eks::_tf-out chunk_bucket_name)
    store_arn=$(just eks::_tf-out store_iam_role_arn)
    bootstrap_arn=$(just eks::_tf-out bootstrap_iam_role_arn)
    db_arn=$(just eks::_tf-out db_secret_arn)
    db_host=$(just eks::_tf-out db_endpoint)
    region=$(just eks::_tf-out region)

    echo "[deploy] tag=$tag registry=$ecr"

    # CRDs first, server-side apply (WorkerPool schema is ~15KB —
    # client-side apply bloats last-applied-configuration).
    kubectl apply --server-side -f infra/helm/crds/

    # Namespace + SSH authorized_keys Secret (operator-provided, not in
    # Secrets Manager). Capture rio_authorized_keys into a variable FIRST
    # so a missing/bad pubkey aborts under set -e BEFORE kubectl runs —
    # otherwise the pipeline races and applies an empty-keyed Secret
    # (gateway silently rejects all SSH auth) before pipefail catches it.
    #
    # Namespace is created HERE (not by the chart — namespace.create=false
    # below) because: (a) the SSH Secret must exist before helm runs; (b)
    # Helm refuses to adopt a namespace it didn't create ("invalid
    # ownership metadata"). PSA label (privileged) is load-bearing:
    # workers need SYS_ADMIN for FUSE mounts.
    source infra/lib/ssh-key.sh
    authorized_keys=$(rio_authorized_keys)
    kubectl create ns rio-system --dry-run=client -o yaml | kubectl apply -f -
    kubectl label ns rio-system pod-security.kubernetes.io/enforce=privileged --overwrite
    printf '%s\n' "$authorized_keys" | kubectl -n rio-system create secret generic rio-gateway-ssh \
        --from-file=authorized_keys=/dev/stdin --dry-run=client -o yaml | kubectl apply -f -

    # Subchart: helm validates charts/ against Chart.yaml BEFORE evaluating
    # condition: postgresql.enabled — so even though EKS uses Aurora (not
    # the subchart), the chart dir must exist. Gitignored; nix store symlink.
    mkdir -p infra/helm/rio-build/charts
    ln -sfn "$(nix build --no-link --print-out-paths .#helm-postgresql)" \
        infra/helm/rio-build/charts/postgresql

    # Chart from the working tree. --wait blocks on Deployment Available
    # (replaces what Argo's sync-wave ordering used to do — we don't
    # actually need ordered bring-up, just "don't return until Ready").
    helm upgrade --install rio infra/helm/rio-build \
        --namespace rio-system \
        --set namespace.create=false \
        --set global.image.registry="$ecr" \
        --set global.image.tag="$tag" \
        --set global.region="$region" \
        --set store.chunkBackend.bucket="$bucket" \
        --set store.logS3Bucket="$bucket" \
        --set store.serviceAccount.annotations."eks\.amazonaws\.com/role-arn"="$store_arn" \
        --set externalSecrets.enabled=true \
        --set externalSecrets.auroraSecretArn="$db_arn" \
        --set externalSecrets.auroraEndpoint="$db_host" \
        --set bootstrap.enabled=true \
        --set bootstrap.serviceAccount.annotations."eks\.amazonaws\.com/role-arn"="$bootstrap_arn" \
        --set-json 'gateway.service.annotations={"service.beta.kubernetes.io/aws-load-balancer-type":"external","service.beta.kubernetes.io/aws-load-balancer-nlb-target-type":"ip","service.beta.kubernetes.io/aws-load-balancer-scheme":"internal","service.beta.kubernetes.io/aws-load-balancer-listener-attributes.TCP-22":"tcp.idle_timeout.seconds=3600"}' \
        --wait --timeout 5m

# Rollback to a previous release (REV from `just eks history`; omit for previous)
rollback REV="0":
    helm rollback rio {{REV}} --namespace rio-system --wait --timeout 5m

# Release history (what values, what tag, when)
history:
    helm history rio --namespace rio-system

# End-to-end smoke test (builds nixpkgs#hello, kills a worker, asserts reassign)
smoke:
    ./infra/eks/smoke-test.sh

[private]
_up APPLY:
    just eks::{{APPLY}}
    just eks::kubeconfig
    just eks::push
    just eks::deploy

# Full bring-up: apply (prompts) → kubeconfig → push → deploy
up: (_up "apply")

# Full bring-up, auto-approves apply
up-auto: (_up "apply-auto")

# WorkerPool finalizers hold pods → NLB → tofu destroy blocks.
destroy:
    kubectl -n rio-system delete workerpool --all --wait=true --ignore-not-found
    tofu -chdir=infra/eks destroy
