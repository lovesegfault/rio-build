# Third-party OCI images pulled as FODs for airgapped k3s VM tests.
#
# nix/docker.nix builds rio-* images from source (parameterized by
# rio-workspace). This file pulls upstream images pinned by digest —
# no rio-workspace dependency → evaluates without the Rust workspace.
#
# To update an image:
#   1. Find the new digest:
#        skopeo inspect docker://<image>:<tag> --format '{{.Digest}}'
#   2. Zero the `hash` field and rebuild a consumer (e.g.
#        `nix build .#checks.x86_64-linux.vm-protocol-warm-k3s`)
#      → hash mismatch error gives the real hash
#   3. Update both `imageDigest` and `hash`
#
# Multi-arch: `imageDigest` is the manifest-LIST digest (what the
# registry returns for `docker pull <tag>`). pullImage follows it to
# the arch-specific manifest via os/arch.
{ pkgs }:
{
  # Bitnami PostgreSQL 18.3.0 for the k3s-full fixture (bitnami subchart
  # v18.6.1 via nixhelm, appVersion=18.3.0). Chart's values.yaml uses
  # tag:latest — k3s-full.nix passes `postgresql.image.tag` via extraSet
  # DERIVED from this FOD's imageTag passthru (no drift window).
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

  # Cilium L7 proxy (standalone DaemonSet). Only loaded when
  # cilium-render.nix gatewayEnabled=true — provides the envoy that
  # Cilium spawns per-Gateway. Tag is the chart 1.19.3 default
  # (envoy.image.tag in the chart's values.yaml). envoy.image.
  # useDigest=false in cilium-render.nix → bare-tag match.
  cilium-envoy = pkgs.dockerTools.pullImage {
    imageName = "quay.io/cilium/cilium-envoy";
    imageDigest = "sha256:ba0ab8adac082d50d525fd2c5ba096c8facea3a471561b7c61c7a5b9c2e0de0d";
    finalImageName = "quay.io/cilium/cilium-envoy";
    finalImageTag = "v1.36.6-1776000132-2437d2edeaf4d9b56ef279bd0d71127440c067aa";
    hash = "sha256-WXKS6yly9bjVTwCBHhdZn754XSwPJvzfWH2RNPOQjfI=";
    os = "linux";
    arch = "amd64";
  };

  # Cilium agent (DaemonSet). Chart 1.19.3. image.useDigest=false in
  # cilium-render.nix → chart renders bare tag, must match finalImageTag.
  cilium-agent = pkgs.dockerTools.pullImage {
    imageName = "quay.io/cilium/cilium";
    imageDigest = "sha256:2e61680593cddca8b6c055f6d4c849d87a26a1c91c7e3b8b56c7fb76ab7b7b10";
    finalImageName = "quay.io/cilium/cilium";
    finalImageTag = "v1.19.3";
    hash = "sha256-5idFC5Ep/bVC2qvblX38jI1STzwMEKgVYwUioFRnegs=";
    os = "linux";
    arch = "amd64";
  };

  # Cilium operator (generic — non-cloud IPAM). operator.image.suffix
  # defaults to "-generic" when no cloud provider is set.
  cilium-operator-generic = pkgs.dockerTools.pullImage {
    imageName = "quay.io/cilium/operator-generic";
    imageDigest = "sha256:205b09b0ed6accbf9fe688d312a9f0fcfc6a316fc081c23fbffb472af5dd62cd";
    finalImageName = "quay.io/cilium/operator-generic";
    finalImageTag = "v1.19.3";
    hash = "sha256-UKGlhslatXOawVR/soWCOqtYJaDOnNF6QogHMwe3eYU=";
    os = "linux";
    arch = "amd64";
  };
}
