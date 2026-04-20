# Dual-stack "external infra" node for k3s-full: simulates AWS NAT GW
# (egress NAT64 via Jool) + AWS NLB v4-listener (ingress socat v4→v6).
# This is the ONLY node in the fixture that holds both address families;
# rio never runs here.
#
# Egress: pods send to 64:ff9b::<v4>/96 (CoreDNS dns64 synthesises the
# AAAA), k3s nodes have a static route 64:ff9b::/96 via this node's
# eth1-v6, Jool stateful-NAT64s onto this node's eth1-v4, packet leaves
# as v4 to upstream-v4.
#
# Ingress: client-v4 connects to this node's eth1-v4:22, socat forwards
# over v6 to k3s-server's gateway NodePort. Mirrors AWS NLB
# enable-prefix-for-ipv6-source-nat — the NLB is dual-stack, the target
# group is v6-only.
{
  pkgs,
  nodes,
  ...
}:
let
  gatewayV6 = nodes.k3s-server.networking.primaryIPv6Address;
  gatewayPort = 32222;
in
{
  # Keep both auto-assigned families (test-driver default). This is the
  # ONLY fixture node that does — every other node mkForce-strips one.

  boot.kernel.sysctl = {
    "net.ipv4.conf.all.forwarding" = 1;
    "net.ipv6.conf.all.forwarding" = 1;
  };

  # Stateful NAT64. pool6 is the RFC 6052 well-known prefix; Jool's
  # default already, set explicitly for grep-ability. Netfilter mode
  # hooks PREROUTING — no manual iptables needed.
  networking.jool = {
    enable = true;
    nat64.default.global.pool6 = "64:ff9b::/96";
  };

  # Ingress v4→v6 proxy: simulates NLB dualstack-listener + v6 SNAT.
  # Listens on this node's v4:22, forwards to gateway NodePort over v6.
  # The test-driver doesn't run sshd (it uses the QEMU backdoor) so :22
  # is free.
  systemd.services.edge-ingress-v4 = {
    wantedBy = [ "multi-user.target" ];
    after = [ "network-online.target" ];
    wants = [ "network-online.target" ];
    serviceConfig = {
      ExecStart = ''
        ${pkgs.socat}/bin/socat \
          TCP4-LISTEN:22,fork,reuseaddr \
          TCP6:[${gatewayV6}]:${toString gatewayPort}
      '';
      Restart = "always";
    };
  };

  # Forwarding box — Jool handles its own netfilter hooks; the default
  # NixOS firewall would drop forwarded 64:ff9b traffic in FORWARD.
  networking.firewall.enable = false;

  environment.systemPackages = [
    pkgs.jool-cli # `jool global display` / `jool session display` for diagnostics
    pkgs.socat
  ];

  virtualisation.memorySize = 512;
}
