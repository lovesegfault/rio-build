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
  '';
}
