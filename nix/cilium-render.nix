# Render the Cilium chart for airgapped k3s VM tests.
#
# Template → split → k3s-manifests; shared logic in lib/helm-split.nix.
# Output
# is a directory with numbered YAML files that k3s applies in filename
# order:
#
#   00-gateway-api-crds.yaml   gateway.networking.k8s.io CRDs (only if
#                              gatewayEnabled — must exist BEFORE the
#                              cilium chart applies or gatewayAPI.
#                              enabled is silently ignored, research-A
#                              caveat C1)
#   01-cilium-rbac.yaml        ServiceAccount / ClusterRole / Bindings
#   02-cilium.yaml             DaemonSet / operator Deployment / ConfigMap
#
# No Cilium-CRD file: cilium-operator installs Cilium CRDs at runtime.
#
# Installed BEFORE any other manifest — Cilium IS the CNI, nothing
# else can schedule until cilium-agent is Ready on the node. k3s
# manifest ordering is filename-alphabetical, so the fixture wires
# these as `000-cilium-*` to sort before everything.
#
# Airgap: image.useDigest=false makes the chart render bare tags
# (quay.io/cilium/cilium:v<pins.addons.cilium.version>) so containerd's
# exact-string image lookup matches the pullImage finalImageName/Tag
# in docker-pulled.nix.
#
# devices=eth1: NixOS test VMs are dual-NIC — eth0 is QEMU user-net
# (10.0.2.x slirp, mgmt-only, no inter-VM routing), eth1 is the vde
# vlan (192.168.1.x, the actual cluster network — same iface flannel
# was told via `--flannel-iface eth1`). Cilium auto-detection picks
# eth0; pinning eth1 mirrors what flannel had.
#
# cgroup.hostRoot=/sys/fs/cgroup + autoMount=false: attach socketLB at
# the host's existing unified hierarchy instead of nsenter-mounting a
# separate /run/cilium/cgroupv2. bpf.masquerade=true uses eBPF SNAT
# (modprobe iptable_nat warns in this kernel; eBPF masq is faster
# anyway). socketLB.hostNamespaceOnly=false makes the connect-hook
# cover pod-ns sockets, not just host-ns. bpf.hostLegacyRouting=true
# routes pod→host via the iptables-compat path; combined with the
# fixture's `networking.firewall.trustedInterfaces` for cilium_host/
# lxc+ and `checkReversePath=false`, this is what makes the post-
# socketLB-redirect packet (pod 10.42.0.x → node-own-IP 192.168.1.3:
# 6443) actually reach the apiserver. Without these, bpftool confirms
# cil_sock4_connect IS attached at /sys/fs/cgroup root and the pod IS
# a descendant — the redirect fires, but the redirected packet is
# dropped by the host firewall/rp_filter on cilium_host (local-path-
# provisioner: `dial tcp 10.43.0.1:443: i/o timeout` → CrashLoopBackOff
# → PVC never binds → PG never Ready).
#
# gatewayEnabled flips on Cilium's Gateway API. envoy.enabled is
# pinned FALSE (embedded mode — envoy runs inside cilium-agent, xDS is
# in-process) because the standalone cilium-envoy DaemonSet's EDS fetch
# times out in this fixture: cilium-agent never pushes
# ClusterLoadAssignment to the separate DS (`gRPC config: initial fetch
# timed out for ...endpoint.v3.ClusterLoadAssignment`). Embedded mode
# bypasses the agent→envoy network xDS hop entirely.
{
  pkgs,
  nixhelm,
  system,
  gatewayEnabled ? false,
}:
let
  pins = import ./pins.nix;
  subcharts = import ./helm-charts.nix { inherit nixhelm system; };
  # Eval-time guard for the airgap invariant: the chart nixhelm ships and
  # the image pins (pins.addons.cilium.version → docker-pulled.nix tags) must
  # agree, or the rendered manifests reference image tags that are not in
  # the airgapped store and every k3s VM test dies in ImagePullBackOff at
  # its global timeout. nixhelm's chartsMetadata is pure data (no IFD), so
  # a skewed nixhelm bump fails evaluation immediately with this message
  # instead.
  chartVersion = nixhelm.chartsMetadata.cilium.cilium.version;
  chart =
    assert pkgs.lib.assertMsg (chartVersion == pins.addons.cilium.version) ''
      cilium chart from nixhelm is ${chartVersion} but nix/pins.toml pins ${pins.addons.cilium.version}.
      Bump pins.toml addons.cilium.version, the cilium image digests/tags
      in nix/docker-pulled.nix, and the infra/eks tfvars together with
      the nixhelm flake input.'';
    subcharts.cilium;
  split = import ./lib/helm-split.nix { inherit (pkgs) lib; };

  # Upstream Gateway API standard-channel CRDs. Cilium does NOT vendor
  # these (it expects them pre-installed). Standard channel = Gateway,
  # GatewayClass, HTTPRoute, GRPCRoute, ReferenceGrant — exactly what
  # dashboard-gateway.yaml uses; no experimental/TLSRoute needed.
  gatewayApiCrds = pkgs.fetchurl {
    url = "https://github.com/kubernetes-sigs/gateway-api/releases/download/${pins.addons.gateway_api.version}/standard-install.yaml";
    sha256 = pins.addons.gateway_api.crds_hash;
  };

  # Cilium creates per-Gateway Services as type:LoadBalancer. With k3s
  # --disable=servicelb and no LB-IPAM pool, the Service stays Pending
  # for an external IP → Gateway never Programmed:True. This pool lets
  # Cilium's lbipam assign an address from the eth1 vlan v6 subnet
  # (2001:db8:1::/64, ::f0-::ff reserved for LB IPs — node IPs are
  # low-numbered). l2announcements makes a Cilium node NDP-reply for
  # the IP on eth1 so it's actually reachable (without it the IP only
  # exists in Service.status, no node claims it → curl times out).
  lbIpamPool = pkgs.writeText "lbipam-pool.yaml" ''
    apiVersion: cilium.io/v2
    kind: CiliumLoadBalancerIPPool
    metadata:
      name: vm-test-pool
    spec:
      blocks:
        - cidr: "2001:db8:1::f0/124"
    ---
    apiVersion: cilium.io/v2alpha1
    kind: CiliumL2AnnouncementPolicy
    metadata:
      name: rio-gateway-l2
    spec:
      serviceSelector:
        matchLabels:
          gateway.networking.k8s.io/gateway-name: rio-dashboard
      interfaces:
        - eth1
      externalIPs: false
      loadBalancerIPs: true
  '';
in
pkgs.runCommand "cilium-rendered"
  {
    nativeBuildInputs = [
      pkgs.kubernetes-helm
      pkgs.yq-go
    ];
  }
  ''
    mkdir -p $out

    ${pkgs.lib.optionalString gatewayEnabled ''
      cp ${gatewayApiCrds} $out/00-gateway-api-crds.yaml
      cp ${lbIpamPool} $out/03-cilium-lbipam-pool.yaml
    ''}

    helm template cilium ${chart} \
      --namespace kube-system \
      --set kubeProxyReplacement=true \
      --set k8sServiceHost=k3s-server \
      --set k8sServicePort=6443 \
      --set devices=eth1 \
      `# nodePort.addresses: auto-detect (empty) programs BPF NodePort` \
      `# frontends only for link-local + ULA, NOT global-unicast IPv6.` \
      `# An external LB sending to the node GUA finds no LB-map entry` \
      `# and gets RST (the EKS NLB bug, commit 022ae5a3). In-cluster` \
      `# tests are socket-LB false positives — connect() is intercepted` \
      `# via [::] wildcard before the physical-IP path. Mirrors` \
      `# infra/eks/addons.tf. v6-only: 192.168.1.0/24 dropped (k3s` \
      `# nodes have no eth1-v4).` \
      --set "nodePort.addresses={2001:db8:1::/64}" \
      --set cgroup.autoMount.enabled=false \
      --set cgroup.hostRoot=/sys/fs/cgroup \
      --set bpf.masquerade=true \
      --set socketLB.enabled=true \
      --set socketLB.hostNamespaceOnly=false \
      --set bpf.hostLegacyRouting=true \
      --set ipam.mode=kubernetes \
      `# live_056-a: exclude per-Job identity labels from identity` \
      `# relevance — the Job-per-build plane minted one CiliumIdentity` \
      `# per build (8.3K live) and collapsed policy enforcement.` \
      `# Single-sourced from nix/pins.toml addons.cilium` \
      `# identity_label_filter (bug_104: one value, three consumers —` \
      `# this render, addons.tf via the generated tfvars lane, and` \
      `# the cilium-labels-filter check). EXCLUSIONS-ONLY semantics` \
      `# and the whitelist-mode hazard are documented at the pin.` \
      --set 'labels=${pins.addons.cilium.identity_label_filter}' \
      --set operator.replicas=1 \
      `# r[impl sec.transport.cilium-wireguard]` \
      --set encryption.enabled=true \
      --set encryption.type=wireguard \
      --set l2announcements.enabled=${if gatewayEnabled then "true" else "false"} \
      --set envoy.enabled=false \
      --set gatewayAPI.enabled=${if gatewayEnabled then "true" else "false"} \
      ${pkgs.lib.optionalString gatewayEnabled ''
        --api-versions gateway.networking.k8s.io/v1 \
        --api-versions gateway.networking.k8s.io/v1/GatewayClass \
        --api-versions gateway.networking.k8s.io/v1/Gateway \
        --api-versions gateway.networking.k8s.io/v1/HTTPRoute \
        --api-versions gateway.networking.k8s.io/v1/GRPCRoute \
        --api-versions gateway.networking.k8s.io/v1beta1/ReferenceGrant \
      ''} \
      --set image.useDigest=false \
      --set operator.image.useDigest=false \
      --set ipv6.enabled=true \
      `# v6 single-stack (de-risks the matching infra/eks/addons.tf` \
      `# change). Top-level underlayProtocol — NOT` \
      `# tunnel.underlayProtocol, which silently no-ops.` \
      `# ipv6NativeRoutingCIDR not required for routingMode=tunnel.` \
      --set ipv4.enabled=false \
      --set routingMode=tunnel \
      --set tunnelProtocol=geneve \
      --set underlayProtocol=ipv6 \
      `# geneve+wireguard: auto-detect leaves cilium_geneve at native` \
      `# MTU but cilium_wg0 at native-80 → full-size pod packets drop` \
      `# at wg0 egress. Pin to native-80 so geneve/host derive from a` \
      `# value that fits through wg0. addons.tf uses 8921 (jumbo); the` \
      `# test-driver vlan is vde_switch with a 1514-byte frame cap, so` \
      `# eth1 stays at 1500 and this is 1500-80.` \
      --set MTU=1420 \
      > all.yaml

    yq 'select(${split.kindIs split.rbacKinds})' all.yaml > $out/01-cilium-rbac.yaml
    yq 'select(${split.kindIsNot split.rbacKinds})' all.yaml > $out/02-cilium.yaml
    ${split.assertNonEmpty "$out"}
  ''
