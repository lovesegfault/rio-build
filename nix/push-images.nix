# `nix run .#push-images` — build all docker images and skopeo-copy
# them to ECR, tagged with the current git short SHA.
#
# Prereqs:
#   - ECR_REGISTRY env var set (get it from `tofu output -raw ecr_registry`)
#   - AWS_PROFILE set (e.g. beme_sandbox) with ECR push permissions
#   - Clean git tree (the SHA is the tag — a dirty tree means the image
#     content won't match what `git show <sha>` shows)
#
# Output: prints `RIO_IMAGE_TAG=<sha>` on the last line so deploy.sh
# can `eval $(nix run .#push-images | tail -1)` if it wants.
{
  pkgs,
  dockerImages,
}:
let
  images = [
    "gateway"
    "scheduler"
    "store"
    "controller"
    "worker"
    "fod-proxy"
  ];
in
pkgs.writeShellApplication {
  name = "push-images";
  runtimeInputs = [
    pkgs.skopeo
    pkgs.awscli2
    pkgs.git
    pkgs.gzip # skopeo docker-archive needs zcat for gzipped tarballs
  ];
  text = ''
    : "''${ECR_REGISTRY:?ECR_REGISTRY not set (run: export ECR_REGISTRY=\$(tofu -chdir=infra/eks output -raw ecr_registry))}"
    : "''${AWS_PROFILE:?AWS_PROFILE not set}"

    region=''${AWS_REGION:-$(aws configure get region --profile "$AWS_PROFILE")}
    : "''${region:?could not determine region — set AWS_REGION}"

    # Dirty-tree check. Immutable ECR tags mean the SHA must be
    # canonical — pushing with uncommitted changes gives an image
    # that doesn't match what the SHA says it is.
    if ! git diff --quiet || ! git diff --cached --quiet; then
      echo "error: git tree is dirty — commit or stash before pushing" >&2
      echo "       (ECR tags are immutable; the SHA must match the image)" >&2
      exit 1
    fi

    tag=$(git rev-parse --short=12 HEAD)
    echo "pushing ${toString (builtins.length images)} images with tag $tag to $ECR_REGISTRY" >&2

    # skopeo login: ECR auth tokens expire after 12h. Fresh token each
    # run — cheap (single API call) and never hits the "denied: Your
    # authorization token has expired" mid-push surprise.
    aws ecr get-login-password --region "$region" \
      | skopeo login --username AWS --password-stdin "$ECR_REGISTRY"

    # The image tarballs are Nix store paths. writeShellApplication
    # interpolates them at build time — no `nix build` at runtime.
    # Each push is ~30s (layer upload); total ~3min for 5 images.
    #
    # --insecure-policy: skopeo enforces signature verification by
    # default and refuses to run without a policy.json at
    # /etc/containers/ or ~/.config/containers/. We're pushing our
    # own just-built images to our own ECR — no signatures to
    # verify. The flag skips the policy check entirely.
    ${pkgs.lib.concatMapStringsSep "\n" (img: ''
      echo "pushing rio-${img}..." >&2
      skopeo --insecure-policy copy --retry-times 3 \
        docker-archive:${dockerImages.${img}} \
        "docker://$ECR_REGISTRY/rio-${img}:$tag"
    '') images}

    echo >&2
    echo "pushed all ${toString (builtins.length images)} images, tag: $tag" >&2
    echo "RIO_IMAGE_TAG=$tag"
  '';
}
