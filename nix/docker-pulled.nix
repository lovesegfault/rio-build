# Third-party OCI images pulled as FODs for airgapped k3s VM tests.
#
# nix/docker.nix builds rio-* images from source (parameterized by
# rio-workspace). This file pulls upstream images pinned by digest —
# no rio-workspace dependency → evaluates without the Rust workspace.
#
# To update an image:
#   1. Find the new digest:
#        skopeo inspect docker://<image>:<tag> --format '{{.Digest}}'
#   2. Zero the `hash` field and `nix build .#pulledImages.<name>`
#      → hash mismatch error gives the real hash
#   3. Update both `imageDigest` and `hash`
#
# Multi-arch: `imageDigest` is the manifest-LIST digest (what the
# registry returns for `docker pull <tag>`). pullImage follows it to
# the arch-specific manifest via os/arch.
{ pkgs }:
{
  # Bitnami PostgreSQL 18.3.0 for the k3s-full fixture (bitnami subchart
  # v18.5.6 via nixhelm, appVersion=18.3.0). Chart's values.yaml uses
  # tag:latest — `values/vmtest-full.yaml` overrides `postgresql.
  # image.tag` to match this digest-pinned tag.
  #
  # Only image needed: chart's volumePermissions (os-shell) and metrics
  # (postgres-exporter) both default enabled:false.
  #
  # When nixhelm bumps the chart, this digest will be stale. The k3s-
  # full test's `kubectl wait pod/rio-postgresql-0` step ImagePullBack
  # → clear signal to bump here.
  bitnami-postgresql = pkgs.dockerTools.pullImage {
    imageName = "registry-1.docker.io/bitnami/postgresql";
    imageDigest = "sha256:106cae6ba66dc1498dba57037b16d6d0f3470277bfcaf440860b1df2f967bf14";
    # finalImageName/Tag: what `ctr images ls` shows. MUST match what
    # the bitnami chart renders (docker.io/bitnami/postgresql:<tag>) —
    # containerd does exact-string image lookup; "bitnami/postgresql"
    # ≠ "docker.io/bitnami/postgresql" → cache miss → tries to pull
    # from network → ImagePullBackOff in the airgapped VM.
    finalImageName = "docker.io/bitnami/postgresql";
    finalImageTag = "18.3.0";
    hash = "sha256-MpAV88ItXcTgRTAtF48I1SL+08Yg1Mn233Cry/96gCY=";
    os = "linux";
    arch = "amd64";
  };

  # smarter-device-manager for the privileged-hardening-e2e VM test
  # (vm-security-nonpriv-k3s, see nix/tests/scenarios/security.nix +
  # values/vmtest-full-nonpriv.yaml). Exposes /dev/fuse as the
  # `smarter-devices/fuse` extended resource — worker pods request it
  # via resources.limits instead of hostPath, which makes
  # hostUsers:false work (kernel rejects idmap mounts on device nodes).
  #
  # Chart default (values.yaml devicePlugin.image) is digest-pinned to
  # the same v1.20.12 digest; the nonpriv values file overrides to the
  # bare `:v1.20.12` tag because containerd's airgap cache is tag-
  # indexed (finalImageTag below), not digest-indexed. Keep imageDigest
  # here and the chart default's @sha256 suffix in lockstep.
  #
  # finalImageName/Tag MUST match `devicePlugin.image` in
  # vmtest-full-nonpriv.yaml exactly — containerd exact-string lookup
  # (same gotcha as bitnami above).
  smarter-device-manager = pkgs.dockerTools.pullImage {
    imageName = "ghcr.io/smarter-project/smarter-device-manager";
    imageDigest = "sha256:228f7f44594a3182571559e62f2e3fe8a3f26180fb5dd7fc0cb7bf7d22a5bbcd";
    finalImageName = "ghcr.io/smarter-project/smarter-device-manager";
    finalImageTag = "v1.20.12";
    hash = "sha256-Ojn8/Uaj2QekiO8qY6Fido0JeD/fM2tXF672NRJo814=";
    os = "linux";
    arch = "amd64";
  };

  # Envoy Gateway operator (gateway-helm v1.7.1 via nixhelm). Both the
  # Deployment and the certgen Job use this image. The operator
  # reconciles Gateway/GRPCRoute/EnvoyProxy CRDs into an envoy data-
  # plane Deployment — that envoy image is NOT referenced by the chart
  # (it's compiled into the operator as a default) so it's a separate
  # pullImage below.
  envoy-gateway = pkgs.dockerTools.pullImage {
    imageName = "registry-1.docker.io/envoyproxy/gateway";
    imageDigest = "sha256:8bb273728bacf981cb2862ed11a6ba8b70970e3b31e3a00429f34f8478c94b8b";
    finalImageName = "docker.io/envoyproxy/gateway";
    finalImageTag = "v1.7.1";
    hash = "sha256-obS8rfPfmJN15Zb4NpKHgAeMSAYvVu8oIf5k7OmyVJU=";
    os = "linux";
    arch = "amd64";
  };

  # Envoy data-plane (distroless). v1.37.1 is the compiled-in default
  # for gateway-helm v1.7.1 (api/v1alpha1/shared_types.go
  # DefaultEnvoyProxyImage). The rio chart's dashboard-gateway-tls.yaml
  # pins this via EnvoyProxy.spec.provider.kubernetes.envoyDeployment.
  # container.image so the operator doesn't try to pull something else.
  envoy-distroless = pkgs.dockerTools.pullImage {
    imageName = "registry-1.docker.io/envoyproxy/envoy";
    imageDigest = "sha256:4d9226b9fd4d1449887de7cde785beb24b12e47d6e79021dec3c79e362609432";
    finalImageName = "docker.io/envoyproxy/envoy";
    finalImageTag = "distroless-v1.37.1";
    hash = "sha256-s0SFWzm0n5lR4+WopRgglqEC7TdtTGhxy+bz+hb6Kic=";
    os = "linux";
    arch = "amd64";
  };
}
