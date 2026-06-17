# ADR-022 castore-FUSE kernel requirements. Standalone (no pins/
# specialArgs deps) so nix/tests/fixtures/ imports the same module
# the AMI uses.
#
# The castore transport is exclusively fuse-over-io_uring
# (FUSE_OVER_IO_URING, kernel >= 6.14); that floor also covers
# FUSE_PASSTHROUGH (Kconfig default since 6.9). No kernelPatches:
# FUSE_FS/OVERLAY_FS come from autoModules. Stock kernel = binary
# cache hit.
{ config, lib, ... }:
{
  # r[impl infra.node.kernel-fuse-passthrough]
  assertions = [
    {
      assertion = lib.versionAtLeast config.boot.kernelPackages.kernel.version "6.14";
      message = ''
        rio-builder needs FUSE_OVER_IO_URING (kernel >= 6.14) — the
        castore-FUSE's only wire transport. Got
        ${config.boot.kernelPackages.kernel.version}. Bump pins.node_kernel_minor.
      '';
    }
  ];

  # Loaded in basic.target — /dev/fuse exists before rio-mountd starts.
  boot.kernelModules = [
    "fuse"
    "overlay"
  ];
}
