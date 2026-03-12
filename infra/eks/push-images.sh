#!/usr/bin/env nix-shell
#!nix-shell -i bash -p awscli2 skopeo git nix
# shellcheck shell=bash
#
# push-images.sh — build all docker images and skopeo-copy to ECR,
# tagged with the current git short-SHA. zstd layers, OCI manifest.
#
# Reads ECR_REGISTRY + AWS_REGION from tofu outputs (same pattern
# as deploy.sh). No manual exports needed.
#
# Output: prints `RIO_IMAGE_TAG=<sha>` on the last line so
# deploy.sh can `eval $(... | tail -1)` if chained.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
TF_DIR="$REPO_ROOT/infra/eks"
# skopeo refuses to run without a policy.json. "insecureAcceptAnything"
# is a misleading name — it means "don't require signature
# verification." Source is docker-archive: (local nix store), dest
# is our own ECR. No signatures exist to verify. --policy is a
# GLOBAL skopeo flag (before the subcommand), not per-subcommand.
POLICY="$REPO_ROOT/infra/eks/containers-policy.json"

log() { echo "[push-images] $*" >&2; }
die() { log "FATAL: $*"; exit 1; }

tf() { tofu -chdir="$TF_DIR" output -raw "$1" 2>/dev/null \
  || die "tofu output '$1' not found — has 'tofu apply' run in $TF_DIR?"; }

ECR_REGISTRY=$(tf ecr_registry)
AWS_REGION=$(tf region)

# Dirty-tree check. ECR tags are immutable — pushing with
# uncommitted changes means the SHA doesn't match the image.
if ! git -C "$REPO_ROOT" diff --quiet || ! git -C "$REPO_ROOT" diff --cached --quiet; then
  die "git tree is dirty — commit or stash before pushing (immutable ECR tags, SHA must be canonical)"
fi

tag=$(git -C "$REPO_ROOT" rev-parse --short=12 HEAD)

# Build the linkFarm aggregate. Out-link goes to a tmpdir so we
# don't clobber ./result (which might be pointing at something
# the user cares about — a workspace build, coverage, whatever).
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
log "building images (nix build .#dockerImages)..."
nix build "$REPO_ROOT#dockerImages" --out-link "$out/images"

# ECR auth — 12h token, fresh each run. skopeo login doesn't
# consult policy (just writes an auth file), no --policy here.
log "ECR login ($ECR_REGISTRY, $AWS_REGION)..."
aws ecr get-login-password --region "$AWS_REGION" \
  | skopeo login --username AWS --password-stdin "$ECR_REGISTRY"

# Glob the linkFarm. Filenames are ${name}.tar.zst (flake.nix
# linkFarm). basename → ECR repo suffix (rio-${name}). Adding an
# image to nix/docker.nix automatically gets pushed — no list here.
#
# --dest-compress-format zstd: layers pushed to ECR are zstd, not
# gzip. -f oci: docker-v2s2 manifests don't carry zstd layer media
# types, OCI does. ECR supports OCI since 2021, zstd since 2023.
# Containerd on the EKS nodes pulls zstd layers natively.
shopt -s nullglob
pushed=0
for f in "$out/images"/*.tar.zst; do
  name=$(basename "$f" .tar.zst)
  log "pushing rio-$name..."
  skopeo --policy "$POLICY" copy --retry-times 3 \
    --dest-compress-format zstd --dest-compress-level 6 -f oci \
    "docker-archive:$f" \
    "docker://$ECR_REGISTRY/rio-$name:$tag"
  pushed=$((pushed + 1))
done

(( pushed > 0 )) || die "no images found in linkFarm — nix build silently produced nothing?"

log "done — pushed $pushed images, tag: $tag"
echo "RIO_IMAGE_TAG=$tag"
