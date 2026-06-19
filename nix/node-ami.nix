# NixOS EKS node AMI builder (ADR-021). The amazon-image.nix builder
# module emits a directory with the disk image +
# nix-support/image-info.json (consumed by `xtask ami push`).
#
# `nodeSystem` is the TARGET arch, independent of the eval host — same
# shape as the dockerImages multi-arch build. specialArgs threads the
# pins attrset (nix/pins.toml via the pins.nix shim) through so module
# files can read kernel/nodeadm pins without importing it scattershot.
#
# Extracted from flake.nix's perSystem `let` block. The
# `self.packages.${nodeSystem}` reference is a flake fixpoint —
# flake-parts already resolves it; passing `self` as an arg here is
# the same value `inputs.self` was at the original callsite.
{ nixpkgs, self }:
nodeSystem:
{
  # I-205: x86_64 .metal SKUs are legacy-bios ONLY (zero
  # support UEFI per `aws ec2 describe-instance-types`). The
  # bios variant swaps uki-boot.nix for bios-boot.nix and
  # registers boot_mode=legacy-bios; everything else is
  # identical so the rio-metal EC2NodeClass can select it for
  # §13b metal NodeClaims (§13c: hwClasses with nodeClass:
  # rio-metal) while rio-default keeps the UEFI/UKI image for
  # virtualized + arm64 .metal.
  efi ? true,
  # #58: when false, omit the executorSeed OCI archive from the
  # baked-in seedImages. The seedless drvPath only moves when
  # nixos-node config / nixpkgs does (NOT every rust commit), so
  # the content-addressed rio.build/ami tag stays stable and
  # `up --ami` is a find_existing hit. Dev default; the prod image
  # (seedExecutor=true) keeps r[infra.node.prebake-layer-warm].
  seedExecutor ? true,
}:
(nixpkgs.lib.nixosSystem {
  system = nodeSystem;
  specialArgs = {
    pins = import ./pins.nix;
    # Layer-cache warm for ephemeral executor pods
    # (PLAN-PREBAKE / r[infra.node.prebake-layer-warm]).
    # self.packages.${nodeSystem} is safe inside perSystem
    # — flake-parts resolves the cross-arch attr without
    # recursion (nodeSystem ≠ eval system is the common
    # case: x86 host builds the aarch64 AMI).
    rioSeedImages = nixpkgs.lib.optionals seedExecutor [
      self.packages.${nodeSystem}.dockerImages.executorSeed
    ];
  };
  modules = [
    (nixpkgs + "/nixos/maintainers/scripts/ec2/amazon-image.nix")
    ./nixos-node
    (if efi then ./nixos-node/uki-boot.nix else ./nixos-node/bios-boot.nix)
    {
      # raw → coldsnap uploads directly to an EBS snapshot
      # via the EBS Direct API (no S3 / VM-Import round-trip,
      # ~20min → ~2min for an 8 GB image).
      amazonImage.format = "raw";
      virtualisation.diskSize = "auto";
      ec2.efi = efi;
    }
  ];
}).config.system.build.amazonImage
