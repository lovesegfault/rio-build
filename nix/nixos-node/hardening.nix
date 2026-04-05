# Parity with the retired Bottlerocket userData (ADR-021 P2): everything
# the EC2NodeClass `userData:` TOML used to inject at boot is baked into
# the AMI here. karpenter.yaml renders NO userData — Karpenter's AL2023
# NodeConfig MIME (consumed by nodeadm-init) is the only runtime input.
#
# r[impl infra.node.nixos-ami]
{ lib, ... }:
{
  boot = {
    # ── sysctl ────────────────────────────────────────────────────────
    # r[impl sec.pod.host-users-false]
    # Bottlerocket defaults user.max_user_namespaces=0; the old userData
    # raised it via [settings.kernel.sysctl]. Worker pods set hostUsers:
    # false (userns-mapped root, ADR-012) which clones a userns to idmap-
    # mount /dev/fuse. Without this the pod sandbox dies "fork/exec: no
    # space left on device" (ENOSPC on the userns clone, not disk). NixOS
    # ships a non-zero default, but pin it explicitly so a future nixpkgs
    # hardening preset can't regress it.
    kernel.sysctl."user.max_user_namespaces" = 65536;

    # ── kernel config (P3, baked now since the AMI is rebuilding) ─────
    # EROFS_FS_ONDEMAND + CACHEFILES_ONDEMAND: the per-page FUSE / riofs
    # track. NETFS_SUPPORT is the dependency CACHEFILES selects upstream;
    # listed explicitly so `node-kernel-config` (P3 check) can assert all
    # four. structuredExtraConfig: the nixpkgs kernel builder merges this
    # into the generated .config — yes-if-unset, error-if-conflicting.
    kernelPatches = [
      {
        name = "rio-ondemand";
        patch = null;
        structuredExtraConfig = with lib.kernel; {
          EROFS_FS = yes;
          EROFS_FS_ONDEMAND = yes;
          CACHEFILES = yes;
          CACHEFILES_ONDEMAND = yes;
          NETFS_SUPPORT = yes;
        };
      }
    ];

    # ── tmpfs /tmp ────────────────────────────────────────────────────
    # Builder emptyDir scratch lives under the kubelet root, not /tmp;
    # this only covers host-side (containerd unpack, nodeadm temp). Keeps
    # / from filling on a bad image pull.
    tmp.useTmpfs = true;
  };

  # ── seccomp profiles ────────────────────────────────────────────────
  # r[impl builder.seccomp.localhost-profile+2]
  # Replaces the rio-seccomp-bootstrap container (P0541): profiles are
  # store paths in the AMI, symlinked into kubelet's seccomp dir before
  # kubelet starts. By the time any pod schedules the file is guaranteed
  # present — controller.seccompPreinstalled=true, no wait-seccomp init.
  # `C` (copy, not `L` symlink): kubelet/runc open the profile via the
  # Localhost path; a /nix/store symlink target would change on every
  # AMI rebuild and confuse Drift-based diffs of node state.
  systemd.tmpfiles.rules = [
    "d /var/lib/kubelet/seccomp/operator 0755 root root -"
    "C /var/lib/kubelet/seccomp/operator/rio-builder.json 0644 root root - ${./seccomp/rio-builder.json}"
    "C /var/lib/kubelet/seccomp/operator/rio-fetcher.json 0644 root root - ${./seccomp/rio-fetcher.json}"
  ];

  # security.lockKernelModules left false until the riofs kmod list is
  # final (ADR-021 §Security posture). Set deliberately so a future
  # hardening import doesn't flip it under us.
  security.lockKernelModules = lib.mkDefault false;
}
