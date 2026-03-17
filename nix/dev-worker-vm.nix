# Lightweight QEMU VM for local development with a real rio-worker.
#
# The worker needs cgroup v2 delegation, FUSE, overlayfs, and CAP_SYS_ADMIN —
# features that require a real Linux kernel. This config produces a QEMU run
# script via NixOS's system.build.vm (the same machinery behind
# `nixos-rebuild build-vm`).
#
# QEMU user-mode networking (SLiRP) connects the VM to the host: the worker
# reaches the host's scheduler/store at 10.0.2.2:{9001,9002}. Port forwarding
# exposes health + metrics endpoints to the host for process-compose probes.
#
# Usage (standalone):
#   nix build .#worker-vm -o result-worker-vm
#   result-worker-vm/bin/run-rio-worker-dev-vm
#
# Usage (via process-compose):
#   process-compose up
#   # then start rio-worker-vm from the TUI (it's disabled by default)
{ modulesPath, ... }:
{
  imports = [
    (modulesPath + "/virtualisation/qemu-vm.nix")
    ./modules/worker.nix
  ];

  # -- Worker service config --------------------------------------------------
  # services.rio.package is set from flake.nix so the VM always runs the
  # current workspace build.
  services = {
    rio.logFormat = "pretty";

    rio.worker = {
      enable = true;
      # SLiRP gateway: guest connects to 10.0.2.2 to reach the host.
      schedulerAddr = "10.0.2.2:9001";
      storeAddr = "10.0.2.2:9002";
      maxBuilds = 2;
    };

    # Serial console auto-login for debugging (no password prompt).
    getty.autologinUser = "root";
  };

  # -- VM settings -------------------------------------------------------------
  networking.hostName = "rio-worker-dev";
  # Allow forwarded ports through the guest firewall (SLiRP delivers via eth0).
  networking.firewall.allowedTCPPorts = [
    9093 # metrics
    9193 # health
  ];

  virtualisation = {
    memorySize = 1024;
    diskSize = 4096;
    cores = 4; # tokio needs spare threads for FUSE callbacks
    graphics = false; # serial console only

    # Worker VMs must NOT have a writable /nix/store. With writableStore=true
    # (the NixOS-test default), /nix/store is itself an overlayfs (tmpfs upper
    # on 9p lower). Per-build overlays use /nix/store as a lower layer;
    # overlay-on-overlay breaks copy-up. writableStore=false keeps /nix/store
    # as the plain 9p mount.
    writableStore = false;

    # Port forwarding: host:port -> guest:port via SLiRP.
    forwardPorts = [
      {
        from = "host";
        host.port = 9093;
        guest.port = 9093;
      } # metrics
      {
        from = "host";
        host.port = 9193;
        guest.port = 9193;
      } # health
    ];
  };

  # With writableStore=false, /nix/var is on the RO 9p mount too. Mount tmpfs
  # so the host nix-daemon + synth-DB bind target have writable paths.
  fileSystems."/nix/var" = {
    fsType = "tmpfs";
    neededForBoot = true;
  };

  system.stateVersion = "25.11";
}
