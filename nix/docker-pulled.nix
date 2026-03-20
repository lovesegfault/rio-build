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
  # Chart default (values.yaml devicePlugin.image) uses v1.20.15 but
  # only v1.20.12 exists upstream; the nonpriv values file overrides
  # devicePlugin.image to match this preloaded tag. If upstream adds
  # v1.20.15 later, bump here + drop the values override.
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
}
