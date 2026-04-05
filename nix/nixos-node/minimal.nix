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
  boot.kernelPackages = lib.mkDefault pkgs."linuxPackages_${pins.node_kernel_minor}";

  documentation.enable = false;
  programs.command-not-found.enable = false;
  environment.defaultPackages = lib.mkForce [ ];

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
