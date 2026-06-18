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
  # Cilium spawns per-Gateway. Tag is the chart 1.19.5 default
  # (envoy.image.tag in the chart's values.yaml). envoy.image.
  # useDigest=false in cilium-render.nix → bare-tag match.
  cilium-envoy = pkgs.dockerTools.pullImage {
    imageName = "quay.io/cilium/cilium-envoy";
    imageDigest = "sha256:326f872e19ce8aa45170efbf583b3f301586ba3feead14b864676d4baf3b45ed";
    finalImageName = "quay.io/cilium/cilium-envoy";
    finalImageTag = "v1.36.8-1781157951-a7f42a3390781539911b5b9107881b35ecc4e752";
    hash = "sha256-KSApiYQ42KpNuVVW/UfrihzEEcI1WjVjqw4MT/Jgr6I=";
    os = "linux";
    arch = "amd64";
  };

  # Cilium agent (DaemonSet). Chart 1.19.5. image.useDigest=false in
  # cilium-render.nix → chart renders bare tag, must match finalImageTag.
  cilium-agent = pkgs.dockerTools.pullImage {
    imageName = "quay.io/cilium/cilium";
    imageDigest = "sha256:20fbbc14ac20b55a292c0dcda5571bf31cde30a7dbc68c29db3e709390ab0732";
    finalImageName = "quay.io/cilium/cilium";
    finalImageTag = "v1.19.5";
    hash = "sha256-nUDyat/hQc72dw6WCq5ajEisvPhrn/+dhNHBVBnzFNU=";
    os = "linux";
    arch = "amd64";
  };

  # Cilium operator (generic — non-cloud IPAM). operator.image.suffix
  # defaults to "-generic" when no cloud provider is set.
  cilium-operator-generic = pkgs.dockerTools.pullImage {
    imageName = "quay.io/cilium/operator-generic";
    imageDigest = "sha256:be848a365776e07d0c5a895eda7aec928ddc52a5a1fa2f432fd7a286609e1db4";
    finalImageName = "quay.io/cilium/operator-generic";
    finalImageTag = "v1.19.5";
    hash = "sha256-TIiLGfqDtH2bUF+7FvMcaqbuVjsUVM6s3oum5BNfR3A=";
    os = "linux";
    arch = "amd64";
  };
}
