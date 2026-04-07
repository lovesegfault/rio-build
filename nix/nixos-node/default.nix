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
  # OCI archive(s) to import into containerd's content store concurrent
  # with kubelet start (layer-cache warm — r[infra.node.prebake-layer-warm]).
  # Threaded via specialArgs from flake.nix's nodeAmi.
  rioSeedImages,
  ...
}:
{
  # `rioSeedImages` MUST be passed via specialArgs (the `? []` formal
  # default is dead code in NixOS module context — the module system
  # always passes a `_module.args` lookup-thunk, so the default never
  # fires). This mkDefault makes the [] fallback actually work for any
  # composition that doesn't pass it (e.g., a future VM-test fixture).
  _module.args.rioSeedImages = lib.mkDefault [ ];

  imports = [
    ./minimal.nix
    ./eks-node.nix
    ./hardening.nix
  ];

  # amazon-image.nix wires three IMDS-racing services: fetch-ec2-metadata
  # (inline) plus apply-ec2-data/print-host-key (via ec2-data.nix import).
  # nodeadm + networkd-DHCP cover hostname; sshd is disabled (minimal.nix);
  # the rest is dead. Runs PARALLEL to nodeadm so
  # the win is contention-only — closure shrink (openssh, lzip, file,
  # hostname-debian) is the real prize.
  disabledModules = [ "virtualisation/ec2-data.nix" ];
  systemd.services.fetch-ec2-metadata.enable = lib.mkForce false;

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
