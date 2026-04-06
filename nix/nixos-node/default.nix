# NixOS EKS worker node — top-level module.
#
# Composed into a nixosSystem from flake.nix as `.#node-ami-<arch>`. The
# nixpkgs `maintainers/scripts/ec2/amazon-image.nix` builder module is
# imported alongside this one (NOT here) so the same module tree can be
# reused by P2's `nix/tests/nixos-node.nix` VM test without dragging in
# the disk-image machinery.
#
# r[impl infra.node.nixos-ami]
#
# Design: docs/src/decisions/021-nixos-node-ami.md (ADR-021).
{
  lib,
  # OCI archive(s) to import into containerd's content store before
  # kubelet starts (layer-cache warm — r[infra.node.prebake-layer-warm]).
  # Threaded via specialArgs from flake.nix's nodeAmi rather than
  # imported directly so this module tree stays evaluable without the
  # full flake context (the P0-nixos-vm-test composition won't pass it).
  rioSeedImages ? [ ],
  ...
}:
{
  imports = [
    ./minimal.nix
    ./eks-node.nix
    ./hardening.nix
  ];

  services.rio.eksNode.enable = true;
  services.rio.eksNode.seedImages = rioSeedImages;

  # nixpkgs amazon-image.nix pulls in amazon-init.service, which fetches
  # userData and pipes it to `nixos-rebuild switch`. We want the node
  # IMMUTABLE post-boot — userData is consumed by nodeadm-init (eks-node.
  # nix), not by a Nix evaluator. Disabling here (not in minimal.nix) so
  # the option only resolves when amazon-image.nix is actually imported
  # (the VM-test composition won't have it).
  virtualisation.amazon-init.enable = lib.mkDefault false;

  # TODO(P0-nixos-vm-test): nix/tests/nixos-node.nix — boot the toplevel
  # (not the disk image) under QEMU with mocked IMDS, assert nodeadm-init
  # succeeds, kubelet starts, seccomp profiles + device-plugin conf exist,
  # `sysctl user.max_user_namespaces` = 65536.
}
