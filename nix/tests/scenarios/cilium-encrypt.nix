# WireGuard transparent encryption: cilium_wg0 up with peers on both
# nodes, cilium-dbg confirms encryption mode.
#
# Asserts the data-plane property the rest of the security model leans
# on once app-level mTLS is removed (Phase 5): pod-to-pod traffic is
# kernel-encrypted between nodes. The fixture's waitReady has already
# gated on `rollout status ds/cilium` + `get ciliumnode k3s-agent`, so
# by the time this runs the mesh is established — these are pure
# assertions, not waits.
#
# `wg show` on the host (NOT inside a pod): the cilium_wg0 device lives
# in the node's root netns. wireguard-tools is in k3sBase systemPackages.
{
  pkgs,
  common,
  fixture,
}:
pkgs.testers.runNixOSTest {
  name = "rio-cilium-encrypt";
  skipTypeCheck = true;

  # Bring-up only — no builds, no rollouts beyond waitReady. ~4min k3s
  # + cilium DS + assertions.
  globalTimeout = 600 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}
    ${common.kvmCheck}
    start_all()
    ${fixture.waitReady}

    with subtest("cilium-wireguard: cilium_wg0 has peer on both nodes"):
        for n in [k3s_server, k3s_agent]:
            out = n.succeed("wg show cilium_wg0")
            print(f"{n.name} wg show:\n{out}")
            assert "peer:" in out, (
                f"no WireGuard peer on {n.name} — cilium_wg0 exists but the "
                f"mesh has not established a tunnel to the other node:\n{out}"
            )

    with subtest("cilium-wireguard: cilium-dbg reports Wireguard encryption"):
        enc = k3s_server.succeed(
            "k3s kubectl -n kube-system exec ds/cilium -- "
            "cilium-dbg encrypt status"
        )
        print(enc)
        assert "Wireguard" in enc, (
            f"cilium-dbg encrypt status does not report Wireguard:\n{enc}"
        )

    # NodePort GUA-v6 frontend: regression guard for the EKS NLB→NodePort
    # RST bug (commit 022ae5a3). Cilium's nodeport-addresses auto-detect
    # programs BPF LB frontends only for link-local + ULA, not global-
    # unicast IPv6 — so an external LB sending to the node's GUA finds no
    # LB-map entry and gets RST'd. The fixture's existing v4 banner check
    # (k3s-full.nix waitReady) and any in-cluster hostNetwork test are
    # FALSE POSITIVES: socket-LB intercepts connect() via the [::] wildcard
    # before the physical-IP datapath is exercised. `client` is a non-
    # Cilium VM, so its connect() hits the real BPF NodePort path.
    with subtest("cilium-nodeport: GUA-v6 frontend programmed in BPF LB map"):
        import re
        lb = k3s_server.succeed(
            "k3s kubectl -n kube-system exec ds/cilium -- "
            "cilium-dbg bpf lb list 2>/dev/null"
        )
        print(lb)
        assert re.search(r"2001:db8:1::[0-9a-f]+\]:32222", lb), (
            "no GUA-v6 NodePort frontend in BPF LB map — auto-detect "
            "programs only link-local + ULA. Set nodePort.addresses in "
            f"cilium-render.nix (mirrors infra/eks/addons.tf). LB map:\n{lb}"
        )

    with subtest("cilium-nodeport: external client to GUA-v6 NodePort returns banner"):
        v6 = client.succeed(
            "getent ahosts k3s-server | awk '/:/{print $1;exit}'"
        ).strip()
        print(f"k3s-server v6 = {v6}")
        # Precheck: client must have a v6 route to the server (test-driver
        # auto-assigns 2001:db8:1::N/64 on eth1; if this fails, the
        # behavioral check below cannot exercise the NodePort path).
        client.succeed(f"ip -6 route get {v6}")
        # wait_until_succeeds: the structural check above proves the
        # frontend is programmed, but endpoint-sync to the per-node BPF
        # map can lag waitReady's v4-socketLB poll by a few seconds.
        banner = client.wait_until_succeeds(
            f"${pkgs.netcat}/bin/nc -6 -w3 {v6} 32222 </dev/null | grep SSH-2.0",
            timeout=30,
        )
        print(f"banner: {banner}")
  '';
}
