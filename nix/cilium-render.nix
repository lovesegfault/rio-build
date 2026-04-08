# Render the Cilium chart for airgapped k3s VM tests.
#
# Same template → split → k3s-manifests pattern as envoy-gateway-render.nix
# (shared logic in lib/helm-split.nix). Output is a directory with
# numbered YAML files that k3s applies in filename order:
#
#   01-cilium-rbac.yaml   ServiceAccount / ClusterRole / Bindings
#   02-cilium.yaml        DaemonSet / operator Deployment / ConfigMap
#
# No 00-crds file: the Cilium chart does NOT template CRDs — cilium-operator
# installs them at runtime on first start.
#
# Installed BEFORE any other manifest — Cilium IS the CNI, nothing
# else can schedule until cilium-agent is Ready on the node. k3s
# manifest ordering is filename-alphabetical, so the fixture wires
# these as `000-cilium-*` to sort before everything.
#
# Airgap: image.useDigest=false makes the chart render bare tags
# (quay.io/cilium/cilium:v1.19.2) so containerd's exact-string image
# lookup matches the pullImage finalImageName/Tag in docker-pulled.nix.
# With useDigest=true (chart default) it renders tag@sha256 which would
# not match a tag-only preloaded image.
#
# envoy.enabled=false: Phase 3 is L4-only (WireGuard + CiliumNetworkPolicy);
# the standalone cilium-envoy DaemonSet is for L7 policy / Gateway API and
# would add a third image to preload. Phase 4 flips this on.
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
{
  pkgs,
  nixhelm,
  system,
}:
let
  subcharts = import ./helm-charts.nix { inherit nixhelm system; };
  chart = subcharts.cilium;
  split = import ./lib/helm-split.nix { inherit (pkgs) lib; };
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

    helm template cilium ${chart} \
      --namespace kube-system \
      --set kubeProxyReplacement=true \
      --set k8sServiceHost=k3s-server \
      --set k8sServicePort=6443 \
      --set devices=eth1 \
      --set cgroup.autoMount.enabled=false \
      --set cgroup.hostRoot=/sys/fs/cgroup \
      --set bpf.masquerade=true \
      --set socketLB.enabled=true \
      --set socketLB.hostNamespaceOnly=false \
      --set bpf.hostLegacyRouting=true \
      --set ipam.mode=kubernetes \
      --set operator.replicas=1 \
      --set encryption.enabled=true \
      --set encryption.type=wireguard \
      --set envoy.enabled=false \
      --set gatewayAPI.enabled=false \
      --set image.useDigest=false \
      --set operator.image.useDigest=false \
      --set ipv6.enabled=true \
      > all.yaml

    yq 'select(${split.kindIs split.rbacKinds})' all.yaml > $out/01-cilium-rbac.yaml
    yq 'select(${split.kindIsNot split.rbacKinds})' all.yaml > $out/02-cilium.yaml
    ${split.assertNonEmpty "$out"}
  ''
