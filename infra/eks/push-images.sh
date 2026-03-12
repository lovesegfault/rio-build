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
# Writes the resolved tag to .rio-image-tag at the repo root —
# deploy.sh reads it from there (not from `git rev-parse`, so a
# dirty push's suffix flows through, and a push-at-X-deploy-at-Y
# mismatch errors loudly instead of silently deploying the wrong
# tag). Also prints RIO_IMAGE_TAG= on stdout for shell chaining.

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

# Tag is git short-SHA, plus a -dirty-${diffhash} suffix if the
# tree has uncommitted changes. ECR tags are immutable, so the tag
# name must uniquely identify the content: a bare SHA on a dirty
# tree would be a lie (and a second dirty push at the same SHA
# would fail "tag exists"). Hashing `git diff HEAD` makes the
# suffix deterministic — same dirty state → same tag → re-push is
# a no-op, not a conflict. `diff HEAD` (not bare `diff`) includes
# staged changes too.
sha=$(git -C "$REPO_ROOT" rev-parse --short=12 HEAD)
if git -C "$REPO_ROOT" diff --quiet HEAD; then
  tag="$sha"
else
  # Untracked files aren't in `git diff` — a new file that changes
  # the build wouldn't change the hash. `git status --porcelain`
  # lists them; folding it into the hash covers that case.
  diffhash=$( { git -C "$REPO_ROOT" diff HEAD; git -C "$REPO_ROOT" status --porcelain; } \
              | sha256sum | cut -c1-8 )
  tag="${sha}-dirty-${diffhash}"
  log "dirty tree — tagging $tag"
fi

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
# deploy.sh reads this file (gitignored). Flows the dirty suffix
# through, and catches push-at-X-deploy-at-Y drift that the old
# derive-from-HEAD approach would silently get wrong.
echo "$tag" > "$REPO_ROOT/.rio-image-tag"
echo "RIO_IMAGE_TAG=$tag"
