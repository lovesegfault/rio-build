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
  # Cilium spawns per-Gateway. Tag is the chart 1.19.4 default
  # (envoy.image.tag in the chart's values.yaml). envoy.image.
  # useDigest=false in cilium-render.nix → bare-tag match.
  cilium-envoy = pkgs.dockerTools.pullImage {
    imageName = "quay.io/cilium/cilium-envoy";
    imageDigest = "sha256:71d4fa0ec45e8d546dbd5604e169dc77fe92be63b799313bff031d00d89762e3";
    finalImageName = "quay.io/cilium/cilium-envoy";
    finalImageTag = "v1.36.6-1778235340-b87d1e32f522b33bd51701c6476d199326f01496";
    hash = "sha256-Aw9DkgCxvcnDE+5YwBbmFeyTT+krYUUhBZE3iSHNIdU=";
    os = "linux";
    arch = "amd64";
  };

  # Cilium agent (DaemonSet). Chart 1.19.4. image.useDigest=false in
  # cilium-render.nix → chart renders bare tag, must match finalImageTag.
  cilium-agent = pkgs.dockerTools.pullImage {
    imageName = "quay.io/cilium/cilium";
    imageDigest = "sha256:2eb67991eaa9368ba199c2fac2c573cb0ffdeb79184533344f42fc9a7ff6af3c";
    finalImageName = "quay.io/cilium/cilium";
    finalImageTag = "v1.19.4";
    hash = "sha256-w8rCYuiF6TmF5aOlZSn6MjyT1XquWCh0eKR0rNXgkfM=";
    os = "linux";
    arch = "amd64";
  };

  # Cilium operator (generic — non-cloud IPAM). operator.image.suffix
  # defaults to "-generic" when no cloud provider is set.
  cilium-operator-generic = pkgs.dockerTools.pullImage {
    imageName = "quay.io/cilium/operator-generic";
    imageDigest = "sha256:1aa2b62735e7d8ab49ee840ae59c346932024c88901579121395c1271b435f71";
    finalImageName = "quay.io/cilium/operator-generic";
    finalImageTag = "v1.19.4";
    hash = "sha256-fcvRwzp9TEOnJJv5P/TCCSFABzoGQEhz3Ue8z7NuBIw=";
    os = "linux";
    arch = "amd64";
  };
}
