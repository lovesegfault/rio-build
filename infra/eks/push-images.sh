#!/usr/bin/env nix-shell
#!nix-shell -i bash -p awscli2 skopeo manifest-tool git
# shellcheck shell=bash
#
# NOTE: `nix` intentionally NOT in -p — nixpkgs' nix (2.31.x) hangs on
# ssh-ng remote stores after eval. The system's patched nix on PATH works.
#
# push-images.sh — build all docker images for x86_64 + aarch64, skopeo-
# copy each to ECR with arch-suffixed tags, then push a multi-arch
# manifest list at the base tag. zstd layers, OCI manifest.
#
# Multi-arch flow:
#   1. nix build .#packages.{x86_64,aarch64}-linux.dockerImages
#      (flake-parts perSystem already exposes both; dockerTools sets
#      the image's architecture field from the build's pkgs.go.GOARCH)
#   2. skopeo copy → rio-foo:$tag-amd64, rio-foo:$tag-arm64
#   3. manifest-tool push from-args → rio-foo:$tag (OCI image index
#      referencing both arch digests)
#
# Pods pull rio-foo:$tag; containerd picks the layer matching the
# node's arch. Karpenter can schedule on m6g/m7g/r7g Graviton nodes
# without `exec format error`.
#
# Reads ECR_REGISTRY + AWS_REGION from tofu outputs. No manual exports needed.
#
# Writes the resolved tag to .rio-image-tag at the repo root —
# `just eks deploy` reads it from there (not from `git rev-parse`, so a
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

# Nix system → OCI arch. The flake's perSystem exposes both; we build
# each and tag by OCI arch (what k8s nodes advertise via
# kubernetes.io/arch and what containerd matches against).
declare -A ARCHES=(
  [x86_64-linux]=amd64
  [aarch64-linux]=arm64
)

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

# Build both arch linkFarms. Out-links go to a tmpdir so we don't
# clobber ./result (which might be pointing at something the user
# cares about — a workspace build, coverage, whatever).
#
# RIO_REMOTE_STORE (optional, e.g. ssh-ng://builder): build on a
# remote store then copy back. Rust compilation is heavy — offloading
# avoids slow local builds and leverages the remote's cache from CI
# runs. Images must land locally for skopeo's docker-archive: reads.
#
# aarch64 builds need the remote to support aarch64-linux — either a
# native ARM builder, or binfmt_misc emulation (boot.binfmt.
# emulatedSystems = ["aarch64-linux"] on NixOS), or a remote-of-
# remote in /etc/nix/machines. Without one of these, the aarch64
# build fails with "a 'aarch64-linux' with features {} is required
# to build <drv>, but I am a 'x86_64-linux'".
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT

build_arch() {
  local sys=$1 arch=$2
  local attr="$REPO_ROOT#packages.$sys.dockerImages"
  if [ -n "${RIO_REMOTE_STORE:-}" ]; then
    log "building $arch images on $RIO_REMOTE_STORE..."
    local outpath
    outpath=$(nix build "$attr" -L --no-link --print-out-paths \
      --eval-store auto --store "$RIO_REMOTE_STORE")
    log "copying $outpath from $RIO_REMOTE_STORE..."
    nix copy --from "$RIO_REMOTE_STORE" --no-check-sigs "$outpath"
    ln -sfn "$outpath" "$out/images-$arch"
  else
    log "building $arch images locally (set RIO_REMOTE_STORE to offload)..."
    nix build "$attr" -L --out-link "$out/images-$arch"
  fi
}

for sys in "${!ARCHES[@]}"; do
  build_arch "$sys" "${ARCHES[$sys]}"
done

# ECR auth — 12h token, fresh each run. skopeo login doesn't
# consult policy (just writes an auth file), no --policy here.
# manifest-tool reads the same ~/.docker/config.json.
log "ECR login ($ECR_REGISTRY, $AWS_REGION)..."
aws ecr get-login-password --region "$AWS_REGION" \
  | skopeo login --username AWS --password-stdin "$ECR_REGISTRY"

# Glob the linkFarms. Filenames are ${name}.tar.zst (flake.nix
# linkFarm). basename → ECR repo suffix (rio-${name}). Adding an
# image to nix/docker.nix automatically gets pushed — no list here.
#
# --dest-compress-format zstd: layers pushed to ECR are zstd, not
# gzip. -f oci: docker-v2s2 manifests don't carry zstd layer media
# types, OCI does. ECR supports OCI since 2021, zstd since 2023.
# Containerd on the EKS nodes pulls zstd layers natively.
#
# Pushes run in parallel (both arches × all images). ECR handles
# concurrent pushes fine; skopeo's --retry-times covers transient
# throttling. Each push's output goes to its own log file so the
# terminal stays readable — interleaved skopeo progress bars are
# illegible. Failures print their log.
shopt -s nullglob

declare -A pids      # pid -> name-arch (for logging)
declare -A seen      # name -> 1 (for manifest loop; dedup across arches)

for sys in "${!ARCHES[@]}"; do
  arch=${ARCHES[$sys]}
  images=( "$out/images-$arch"/*.tar.zst )
  (( ${#images[@]} > 0 )) \
    || die "no $arch images in linkFarm — nix build silently produced nothing?"
  for f in "${images[@]}"; do
    name=$(basename "$f" .tar.zst)
    seen[$name]=1
    log "pushing rio-$name:$tag-$arch (background)..."
    skopeo --policy "$POLICY" copy --retry-times 3 \
      --dest-compress-format zstd --dest-compress-level 6 -f oci \
      "docker-archive:$f" \
      "docker://$ECR_REGISTRY/rio-$name:$tag-$arch" \
      >"$out/$name-$arch.log" 2>&1 &
    pids[$!]="$name-$arch"
  done
done

# Wait for ALL pushes (not just the first failure) so every error
# surfaces at once. bare `wait` returns 0 even if jobs failed —
# must `wait $pid` individually to get each exit status.
failed=()
for pid in "${!pids[@]}"; do
  if wait "$pid"; then
    log "  rio-${pids[$pid]}: ok"
  else
    failed+=("${pids[$pid]}")
    log "  rio-${pids[$pid]}: FAILED"
    sed 's/^/    /' "$out/${pids[$pid]}.log" >&2
  fi
done
(( ${#failed[@]} == 0 )) || die "${#failed[@]} push(es) failed: ${failed[*]}"

# Manifest list (OCI image index) per image. ECR treats the index as
# its own immutable tag — pods pull rio-foo:$tag, containerd picks
# the arch-matching entry. manifest-tool's from-args template uses
# ARCH as placeholder (literal, it substitutes amd64/arm64 from the
# --platforms list).
#
# Sequential: manifest push is a small metadata-only PUT (references
# existing blobs by digest, no layer upload). ~1s each; not worth
# parallelizing and interleaving errors.
log "creating multi-arch manifest lists..."
for name in "${!seen[@]}"; do
  log "  rio-$name:$tag → {amd64,arm64}"
  manifest-tool push from-args \
    --platforms linux/amd64,linux/arm64 \
    --template "$ECR_REGISTRY/rio-$name:$tag-ARCH" \
    --target   "$ECR_REGISTRY/rio-$name:$tag"
done

log "done — pushed ${#seen[@]} images × 2 arches + manifest lists, tag: $tag"
# `just eks deploy` reads this file (gitignored). Flows the dirty suffix
# through, and catches push-at-X-deploy-at-Y drift that the old
# derive-from-HEAD approach would silently get wrong.
echo "$tag" > "$REPO_ROOT/.rio-image-tag"
echo "RIO_IMAGE_TAG=$tag"
