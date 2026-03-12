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
#   - `./infra/eks/push-images.sh` completed (RIO_IMAGE_TAG set OR
#     we derive it from git HEAD)
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

# shellcheck source=infra/lib/ssh-key.sh
source "$REPO_ROOT/infra/lib/ssh-key.sh"

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

# Image tag resolution, in order:
#   1. RIO_IMAGE_TAG env (explicit override)
#   2. .rio-image-tag file written by push-images.sh — carries
#      the dirty suffix if there was one, and catches drift:
#      push at commit X, then you commit, then deploy — the
#      file still says X's tag, which is correct (X is what's
#      in ECR). The old derive-from-HEAD approach would use
#      the new HEAD and deploy a tag that doesn't exist.
#   3. git rev-parse HEAD — fallback for "I know the SHA is in
#      ECR from a previous push" without the file.
# If the resolved tag isn't in ECR, pods ImagePullBackOff with
# "manifest not found" — easy to diagnose.
TAG_FILE="$REPO_ROOT/.rio-image-tag"
if [[ -n "${RIO_IMAGE_TAG:-}" ]]; then
  log "image tag from env: $RIO_IMAGE_TAG"
elif [[ -f "$TAG_FILE" ]]; then
  RIO_IMAGE_TAG=$(<"$TAG_FILE")
  log "image tag from $TAG_FILE: $RIO_IMAGE_TAG"
else
  RIO_IMAGE_TAG=$(git rev-parse --short=12 HEAD)
  log "image tag derived from HEAD: $RIO_IMAGE_TAG (no .rio-image-tag — run push-images.sh?)"
fi
export RIO_IMAGE_TAG

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
  # Do NOT name this TMPDIR — that's the env var mktemp itself reads,
  # and the later `rm -rf` would leave it pointing at a deleted dir,
  # breaking the RENDER=$(mktemp -d) below.
  keytmp=$(mktemp -d)
  trap 'rm -rf "$keytmp"' RETURN
  nix-store --generate-binary-cache-key "rio-${RIO_CHUNK_BUCKET}" \
    "$keytmp/key.sec" "$keytmp/key.pub"
  kubectl -n "$NS" create secret generic rio-signing-key \
    --from-file=key="$keytmp/key.sec"
  log "signing public key (add to nix.conf trusted-public-keys):"
  cat "$keytmp/key.pub" >&2
  rm -rf "$keytmp"
  trap - RETURN
else
  log "rio-signing-key Secret already exists, skipping generation"
fi

# --- SSH authorized_keys Secret ---
# Key from RIO_SSH_PUBKEY (.env.local, default ~/.ssh/id_ed25519.pub)
# with the comment stripped → single-tenant mode (scheduler skips
# tenant lookup on empty name). Set RIO_SSH_TENANT to keep a comment;
# that tenant must then exist (`kubectl exec deploy/rio-scheduler --
# rio-cli create-tenant <name>`). smoke-test.sh replaces this Secret
# with its own tenant-tagged key for the automated flow.
#
# Capture-then-create: if validation fails, set -e aborts before any
# kubectl call. A pipe would race (kubectl might read empty stdin
# before pipefail kills the line).
if ! kubectl -n "$NS" get secret rio-gateway-ssh >/dev/null 2>&1; then
  log "installing SSH authorized_keys from ${RIO_SSH_PUBKEY:-~/.ssh/id_ed25519.pub}"
  AUTHORIZED_KEYS=$(rio_authorized_keys)
  kubectl -n "$NS" create secret generic rio-gateway-ssh \
    --from-file=authorized_keys=/dev/stdin <<<"$AUTHORIZED_KEYS"
else
  log "rio-gateway-ssh Secret already exists, skipping"
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
mkdir -p "$RENDER/infra/k8s/overlays"
ln -s "$REPO_ROOT/infra/k8s/base" "$RENDER/infra/k8s/base"
ln -s "$REPO_ROOT/infra/k8s/overlays/prod" "$RENDER/infra/k8s/overlays/prod"
mkdir "$RENDER/infra/k8s/overlays/eks"
# sed-replace the 4 vars. `|` as delimiter because ARNs contain `:`.
sed \
  -e "s|\${STORE_IAM_ROLE_ARN}|$STORE_IAM_ROLE_ARN|g" \
  -e "s|\${ECR_REGISTRY}|$ECR_REGISTRY|g" \
  -e "s|\${RIO_CHUNK_BUCKET}|$RIO_CHUNK_BUCKET|g" \
  -e "s|\${RIO_IMAGE_TAG}|$RIO_IMAGE_TAG|g" \
  "$REPO_ROOT/infra/k8s/overlays/eks/kustomization.yaml" \
  > "$RENDER/infra/k8s/overlays/eks/kustomization.yaml"

log "applying manifests"
kubectl apply -k "$RENDER/infra/k8s/overlays/eks"

# --- Wait for rollout ---
log "waiting for control-plane pods"
kubectl -n "$NS" wait --for=condition=Available \
  deployment/rio-store deployment/rio-gateway deployment/rio-controller \
  --timeout=300s \
  || die "control-plane deployments didn't become Available"

# Scheduler is different: readinessProbe gates on is_leader (so the
# Service routes only to the active replica). replicas=2 means
# exactly 1 is ever Ready — the standby stays not-Ready by design.
# The Available condition needs availableReplicas >= replicas -
# MaxUnavailable, and for Recreate strategy MaxUnavailable is 0
# (it only applies to RollingUpdate). So Available needs 2/2 Ready,
# which is permanently false. Wait for 1 Ready explicitly instead.
kubectl -n "$NS" wait deployment/rio-scheduler \
  --for=jsonpath='{.status.readyReplicas}'=1 --timeout=300s \
  || die "scheduler never elected a leader"

log "done. Next: ./infra/eks/smoke-test.sh"
