#!/usr/bin/env nix-shell
#!nix-shell -i bash -p awscli2 kubectl jq openssl nix
# shellcheck shell=bash
#
# deploy.sh — turn `tofu apply` outputs into a running rio cluster.
#
# Reads terraform outputs, builds the Aurora connection string from
# Secrets Manager, generates HMAC + signing keys (idempotent — skips
# if Secrets already exist), sed-replaces ${VARS} in the eks overlay,
# and applies.
#
# Prerequisites:
#   - `tofu apply` completed (run from infra/eks/)
#   - `nix run .#push-images` completed (RIO_IMAGE_TAG set OR we
#     derive it from git HEAD)
#   - kubectl configured (`$(tofu output -raw kubeconfig_command)`)
#   - AWS_PROFILE set
#
# Usage (from repo root):
#   ./infra/eks/deploy.sh
#
# Idempotent: reruns skip existing secrets, reapply manifests
# (kubectl apply is a 3-way merge, safe to rerun).

set -euo pipefail

NS=rio-system
REPO_ROOT="$(git rev-parse --show-toplevel)"
TF_DIR="$REPO_ROOT/infra/eks"

log() { echo "[deploy] $*" >&2; }
die() { log "FATAL: $*"; exit 1; }

# tofu output wrapper — die with a clear message if an output is
# missing (terraform hasn't been applied, or wrong working dir).
tf() { tofu -chdir="$TF_DIR" output -raw "$1" 2>/dev/null \
  || die "tofu output '$1' not found — has 'tofu apply' run in $TF_DIR?"; }

# --- Terraform outputs ---
log "reading terraform outputs"
export STORE_IAM_ROLE_ARN; STORE_IAM_ROLE_ARN=$(tf store_iam_role_arn)
export ECR_REGISTRY;       ECR_REGISTRY=$(tf ecr_registry)
export RIO_CHUNK_BUCKET;   RIO_CHUNK_BUCKET=$(tf chunk_bucket_name)
DB_ENDPOINT=$(tf db_endpoint)
DB_SECRET_ARN=$(tf db_secret_arn)
REGION=$(tf region)

# Image tag: from env (set by push-images) or derive from HEAD.
# If the derived SHA isn't in ECR, the pods will ImagePullBackOff
# with a clear "manifest not found" — easy to diagnose.
export RIO_IMAGE_TAG=${RIO_IMAGE_TAG:-$(git rev-parse --short=12 HEAD)}
log "using image tag: $RIO_IMAGE_TAG (ensure 'nix run .#push-images' has pushed it)"

# --- Namespace (cert-manager Certificate CRs need it to exist
# before apply, and the Secrets below go into it) ---
kubectl get ns "$NS" >/dev/null 2>&1 || kubectl create ns "$NS"

# --- Aurora connection string → rio-postgres Secret ---
# manage_master_user_password puts a JSON blob in Secrets Manager:
#   {"username":"rio","password":"<generated>"}
# sslmode=require: Aurora has rds.force_ssl=1. sqlx's tls-rustls-
# aws-lc-rs feature handles the handshake. `require` = encrypt,
# don't verify (RDS CA not in container). Good enough for in-VPC.
log "building Aurora connection string"
DB_PASSWORD=$(aws secretsmanager get-secret-value \
  --region "$REGION" --secret-id "$DB_SECRET_ARN" \
  --query SecretString --output text | jq -r .password)
DB_URL="postgres://rio:${DB_PASSWORD}@${DB_ENDPOINT}:5432/rio?sslmode=require"

# --dry-run=client -o yaml | apply: idempotent create-or-update.
# Secrets don't have a native `kubectl create --update` but this
# pattern is the standard workaround.
kubectl -n "$NS" create secret generic rio-postgres \
  --from-literal=url="$DB_URL" \
  --dry-run=client -o yaml | kubectl apply -f -

# --- HMAC key (scheduler signs, store verifies) ---
# Only generate if the Secret doesn't exist — regenerating would
# invalidate in-flight assignment tokens. 32 bytes raw, not hex.
if ! kubectl -n "$NS" get secret rio-hmac >/dev/null 2>&1; then
  log "generating HMAC key"
  HMAC=$(openssl rand 32 | base64 -w0)
  kubectl -n "$NS" create secret generic rio-hmac \
    --from-literal=key="$(echo "$HMAC" | base64 -d)"
else
  log "rio-hmac Secret already exists, skipping generation"
fi

# --- NAR signing key (ed25519, nix secret-key format) ---
# Same idempotence: regenerating would invalidate existing
# narinfo signatures (clients with the old public key would
# reject everything).
if ! kubectl -n "$NS" get secret rio-signing-key >/dev/null 2>&1; then
  log "generating NAR signing key"
  # nix-store --generate-binary-cache-key writes two files:
  # <name>.sec and <name>.pub. Format is `name:base64-seed`.
  TMPDIR=$(mktemp -d)
  trap 'rm -rf "$TMPDIR"' RETURN
  nix-store --generate-binary-cache-key "rio-${RIO_CHUNK_BUCKET}" \
    "$TMPDIR/key.sec" "$TMPDIR/key.pub"
  kubectl -n "$NS" create secret generic rio-signing-key \
    --from-file=key="$TMPDIR/key.sec"
  log "signing public key (add to nix.conf trusted-public-keys):"
  cat "$TMPDIR/key.pub" >&2
  rm -rf "$TMPDIR"
  trap - RETURN
else
  log "rio-signing-key Secret already exists, skipping generation"
fi

# --- SSH authorized_keys Secret ---
# Empty placeholder if not set — smoke-test.sh will replace it
# with a real key. Creating it here means the gateway pod can
# start (volumeMount doesn't fail on missing Secret) and smoke-test
# just rolls it out.
if ! kubectl -n "$NS" get secret rio-gateway-ssh >/dev/null 2>&1; then
  log "creating placeholder rio-gateway-ssh Secret (smoke-test.sh replaces it)"
  kubectl -n "$NS" create secret generic rio-gateway-ssh \
    --from-literal=authorized_keys=""
fi

# --- Wait for cert-manager (terraform installs it, but Ready takes
# ~60s after helm_release returns) ---
log "waiting for cert-manager"
kubectl -n cert-manager wait --for=condition=Available \
  deployment/cert-manager --timeout=120s \
  || die "cert-manager not Ready (terraform helm_release applied?)"

# --- Render + apply ---
# kustomize doesn't do env-var substitution. Copy the overlay to
# a tmpdir, sed-replace the ${VARS}, apply -k.
#
# Only the eks/ kustomization.yaml has ${VARS}. It references
# ../prod which references ../../base — those are resolved
# RELATIVE to the kustomization.yaml's location, so the tmpdir
# copy needs the same directory depth. Symlinks back to the real
# dirs keep it simple.
log "rendering overlay"
RENDER=$(mktemp -d)
trap 'rm -rf "$RENDER"' EXIT
mkdir -p "$RENDER/deploy/overlays"
ln -s "$REPO_ROOT/deploy/base" "$RENDER/deploy/base"
ln -s "$REPO_ROOT/deploy/overlays/prod" "$RENDER/deploy/overlays/prod"
mkdir "$RENDER/deploy/overlays/eks"
# sed-replace the 4 vars. `|` as delimiter because ARNs contain `:`.
sed \
  -e "s|\${STORE_IAM_ROLE_ARN}|$STORE_IAM_ROLE_ARN|g" \
  -e "s|\${ECR_REGISTRY}|$ECR_REGISTRY|g" \
  -e "s|\${RIO_CHUNK_BUCKET}|$RIO_CHUNK_BUCKET|g" \
  -e "s|\${RIO_IMAGE_TAG}|$RIO_IMAGE_TAG|g" \
  "$REPO_ROOT/deploy/overlays/eks/kustomization.yaml" \
  > "$RENDER/deploy/overlays/eks/kustomization.yaml"

log "applying manifests"
kubectl apply -k "$RENDER/deploy/overlays/eks"

# --- Wait for rollout ---
log "waiting for control-plane pods"
kubectl -n "$NS" wait --for=condition=Available \
  deployment/rio-store deployment/rio-scheduler \
  deployment/rio-gateway deployment/rio-controller \
  --timeout=300s \
  || die "control-plane deployments didn't become Available"

log "done. Next: ./infra/eks/smoke-test.sh"
