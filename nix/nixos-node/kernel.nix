# ADR-022 castore-FUSE kernel requirements. Standalone (no pins/
# specialArgs deps) so nix/tests/fixtures/ imports the same module
# the AMI uses.
#
# No kernelPatches: FUSE_PASSTHROUGH=y is the Kconfig default since
# 6.9; FUSE_FS/OVERLAY_FS come from autoModules. Stock kernel = binary
# cache hit.
{ config, lib, ... }:
{
  # r[impl infra.node.kernel-fuse-passthrough]
  assertions = [
    {
      assertion = lib.versionAtLeast config.boot.kernelPackages.kernel.version "6.9";
      message = ''
        rio-builder needs FUSE_PASSTHROUGH (kernel >= 6.9, commit 7dc4e97a4f9a).
        Got ${config.boot.kernelPackages.kernel.version}. Bump pins.node_kernel_minor.
      '';
    }
  ];

  # Loaded in basic.target — /dev/fuse exists before rio-mountd starts.
  boot.kernelModules = [
    "fuse"
    "overlay"
  ];
}
