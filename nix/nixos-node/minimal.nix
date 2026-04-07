# Strip the node image to kubelet + containerd + nodeadm + ssm-agent.
#
# Builder/fetcher nodes are ephemeral (~5 min lifetime, single build per
# pod). No interactive login, no package manager, no docs. Debugging is
# `kubectl debug node/…` (ephemeral container with its own toolbox) or
# SSM Session Manager — neither needs on-image tooling.
{
  lib,
  pins,
  pkgs,
  ...
}:
{
  # Node never evaluates Nix — builds run inside the builder POD, which
  # carries its own nix. Dropping the daemon saves ~80 MB closure and
  # removes a root-socket attack surface. (ADR-021 Q2.)
  nix.enable = false;

  # Pinned kernel minor (ADR-021 Q3). pins.node_kernel_minor is a string
  # like "6_18" → resolves pkgs.linuxPackages_6_18. A nixpkgs flake-input
  # bump can't surprise-rebuild the kernel; bump pins.nix deliberately.
  boot = {
    kernelPackages = lib.mkDefault pkgs."linuxPackages_${pins.node_kernel_minor}";

    # systemd-in-initrd: parallel device probe + structured stage-1
    # logging (journalctl -b shows initrd). amazon-image.nix's NVMe-
    # root + ENA modules already load via availableKernelModules; this
    # just swaps the busybox stage-1 script for systemd.
    initrd = {
      systemd.enable = lib.mkDefault true;
      # Nitro only: drop xen-blkfront/dm_mod and the 28 SATA/USB/HID
      # defaults. nvme stays via amazon-image.nix's availableKernelModules;
      # ext4 via fileSystems."/".fsType → supportedFilesystems. A future
      # QEMU VM-test fixture will need virtio_blk/virtio_pci added back.
      kernelModules = lib.mkForce [ ];
      includeDefaultModules = false;
    };

    # systemd's status output goes to ttyS0 @ 115200 baud; the per-unit
    # "[  OK  ] Started …" lines are serial-bound. `quiet` drops them.
    # Kernel printk stays at the NixOS default loglevel=4.
    kernelParams = [ "quiet" ];
  };

  documentation.enable = false;
  programs.command-not-found.enable = false;
  environment.defaultPackages = lib.mkForce [ ];
  fonts.fontconfig.enable = false;
  xdg = {
    autostart.enable = false;
    icons.enable = false;
    menus.enable = false;
    mime.enable = false;
    sounds.enable = false;
  };

  # Headless: keep booting on initrd/mount failure — there's no console to
  # reach an emergency shell from; a wedged unit is worse than a degraded
  # boot Karpenter can drift-replace. nodeadm-init orders on
  # network.target (not network-online), so wait-online itself is dead
  # weight.
  systemd.enableEmergencyMode = false;
  systemd.network.wait-online.enable = false;

  # VPC security groups are the network boundary; the nixpkgs firewall
  # default-denies NodePort + kubelet :10250 health, which breaks the
  # `kubectl debug node` and metrics-server paths.
  networking.firewall.enable = false;

  # SSM Session Manager only (amazon-image.nix enables ssm-agent by
  # default). No sshd → no host key, no port 22 SG rule.
  services.openssh.enable = lib.mkForce false;

  # boot.growPartition is set by amazon-image.nix (expands / to fill the
  # blockDeviceMappings volume on first boot). stateVersion: image is
  # immutable + ephemeral, so the value is documentation, not migration-
  # gating — track the nixpkgs release it was built against.
  system.stateVersion = lib.mkDefault lib.trivial.release;
}
