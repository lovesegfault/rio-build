# systemd-networkd in stage-1: primary ENI starts DHCP while rootfs
# mounts. flushBeforeStage2=false hands the configured link to stage-2
# networkd intact (shared /run/systemd/netif state) so the
# 80-ec2-primary settings in eks-node.nix (DAD=0, WithoutRA=solicit)
# carry forward without a re-DHCP at the boundary.
_: {
  boot.initrd = {
    # ena MUST be in initrd now — minimal.nix's mkForce[] dropped it.
    # af_packet: networkd's DHCP client + LLDP both use raw packet
    # sockets; without it the link goes "Failed → reconfigure" loop and
    # initrd networkd provides no win. virtio_net stays in
    # nix/tests/nixos-node.nix's own override.
    availableKernelModules = [
      "ena"
      "af_packet"
    ];
    systemd.network = {
      enable = true;
      # Stage-2's eks-node.nix defines the full 80-ec2-primary unit;
      # duplicating the matchConfig + DHCP here (can't reference
      # config.systemd.network — infinite recursion through the module
      # fixpoint). Only the time-critical knobs.
      networks."80-ec2-primary" = {
        matchConfig = {
          Type = "ether";
          Kind = "!*";
          Name = "!eth* !veth*";
        };
        networkConfig = {
          DHCP = "yes";
          IPv6DuplicateAddressDetection = 0;
          # LLDP is useless on a VPC; skip the socket open.
          LLDP = false;
          EmitLLDP = false;
        };
        dhcpV6Config.WithoutRA = "solicit";
      };
    };
    # Don't tear down + re-DHCP at the stage-1→2 boundary.
    network.flushBeforeStage2 = false;
  };
}
