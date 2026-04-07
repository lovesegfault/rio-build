# Legacy-BIOS boot via grub — x86_64 .metal variant only (I-205).
#
# AWS x86_64 bare-metal SKUs (c6a.metal through c8i.metal-96xl) reject
# UEFI AMIs: `aws ec2 describe-instance-types --filters
# Name=bare-metal,Values=true Name=processor-info.supported-
# architecture,Values=x86_64 --query 'InstanceTypes[].SupportedBootModes'`
# returns `["legacy-bios"]` for every entry. The UEFI/UKI AMI
# (uki-boot.nix) made the rio-builder-metal NodePool unschedulable on x86
# — Karpenter CreateFleet → InvalidParameterValue on every candidate,
# NodeClaims churned forever.
#
# This module is composed ONLY into `.#node-ami-x86_64-bios` (flake.nix
# nodeAmi efi=false). Virtualized x86 and all arm64 stay on uki-boot.nix.
# nixpkgs amazon-image.nix already wires `boot.loader.grub.{device =
# "/dev/xvda", timeout = 1, extraConfig = serial}` when `ec2.efi=false`;
# this module just re-asserts what perlless.nix mkDefault'd off and
# relaxes the perl-free assertion (grub's installer is install-grub.pl).
{ lib, ... }:
{
  # perlless.nix sets `grub.enable = mkDefault false`; amazon-image.nix
  # sets device/efiSupport but not enable. Re-assert.
  boot.loader.grub.enable = true;

  # The perlless profile's behavioral wins (overlay-/etc, userborn,
  # systemd-initrd) are restated explicitly in minimal.nix; only the
  # closure-assertion is dropped here. The UEFI variants keep it.
  system.forbiddenDependenciesRegexes = lib.mkForce [ ];
}
