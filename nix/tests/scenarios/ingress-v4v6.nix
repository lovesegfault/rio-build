# 2×2 ingress/egress coverage for the IPv6-only k3s fixture.
#
# Ingress: client-v6 connects directly to k3s-server's NodePort over
# v6; client-v4 connects to edge:22 where socat forwards over v6 to the
# same NodePort. Both run a trivial nix-build over ssh-ng — proves the
# gateway listener doesn't care which family the client came from
# (translation is external infra, r[gw.ingress.*]).
#
# Egress: from a v6-only k3s host, upstream-v6:8080 is reachable
# directly; upstream-v4:8080 is reachable via the 64:ff9b::/96 prefix
# (Jool stateful NAT64 on the edge node). Then from inside a pod's
# netns, CoreDNS dns64 synthesises 64:ff9b::<v4> for upstream-v4 — the
# DNS half of the chain.
#
# Tracey markers: r[verify ...] at the default.nix subtests entry.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) ns;
  inherit (common) busybox busyboxClosure;
  drvs = import ../lib/derivations.nix { inherit pkgs; };

  trivialV6 = drvs.mkTrivial { marker = "ingress-v6-direct"; };
  trivialV4 = drvs.mkTrivial { marker = "ingress-v4-via-edge"; };

  curl = "${pkgs.curl}/bin/curl";
  dig = "${pkgs.dnsutils}/bin/dig";
in
pkgs.testers.runNixOSTest {
  name = "rio-ingress-v4v6";
  skipTypeCheck = true;

  # k3s bring-up ~4min + two trivial builds + a handful of curl probes.
  globalTimeout = 900 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.mkBootstrap {
      inherit fixture;
      # mkBootstrap's withSsh runs fixture.sshKeySetup on client_v6 only;
      # we need keys on BOTH clients with a SHARED authorized_keys, so do
      # it manually below.
      withSsh = false;
    }}

    # ══════════════════════════════════════════════════════════════════
    # SSH key setup — both clients share one authorized_keys Secret
    # ══════════════════════════════════════════════════════════════════
    # The gateway's authorized_keys file is one-Secret-many-keys; both
    # clients' pubkeys go in. Each client's ssh_config (mkClientNode)
    # already routes Host k3s-server → :32222 (v6) / Host edge → :22 (v4).
    pubkeys = []
    for c in (client_v6, client_v4):
        c.succeed(
            "mkdir -p /root/.ssh && "
            "ssh-keygen -t ed25519 -N ''' -C ''' -f /root/.ssh/id_ed25519"
        )
        pubkeys.append(c.succeed("cat /root/.ssh/id_ed25519.pub").strip())
    k3s_server.succeed(
        "k3s kubectl -n ${ns} create secret generic rio-gateway-ssh "
        f"--from-literal=authorized_keys='{chr(10).join(pubkeys)}' "
        "--dry-run=client -o yaml | k3s kubectl apply -f -"
    )
    ${fixture.bounceGatewayForSecret}

    # edge socat must be up before client-v4 connects. wait_for_open_port
    # probes 127.0.0.1 by default, but socat binds TCP4-LISTEN (any v4).
    edge.wait_for_unit("edge-ingress-v4.service")
    edge.wait_for_open_port(22)

    # SSH banner over each path (same structural check as
    # fixture.sshKeySetupFor — proves kube-proxy synced AND russh
    # accept-loop bound, not just port open).
    client_v6.wait_until_succeeds(
        "(${pkgs.netcat}/bin/nc -w2 k3s-server 32222 </dev/null 2>&1 "
        "|| true) | grep -q ^SSH-",
        timeout=60,
    )
    client_v4.wait_until_succeeds(
        "(${pkgs.netcat}/bin/nc -w2 edge 22 </dev/null 2>&1 "
        "|| true) | grep -q ^SSH-",
        timeout=60,
    )

    # ══════════════════════════════════════════════════════════════════
    # ingress: both clients seed busybox + build
    # ══════════════════════════════════════════════════════════════════
    for c, host, drv, label in [
        (client_v6, "k3s-server", "${trivialV6}", "v6-direct"),
        (client_v4, "edge", "${trivialV4}", "v4-via-edge"),
    ]:
        with subtest(f"ingress-{label}: nix-build over ssh-ng://{host}"):
            # Seed busybox closure (each client needs it locally for
            # nix copy). mkClientNode already pulled it into the VM
            # store via environment.systemPackages.
            c.succeed("ls ${busybox}")
            c.succeed(
                "nix copy --no-check-sigs "
                f"--to 'ssh-ng://{host}' "
                "$(cat ${busyboxClosure}/store-paths)"
            )
            out = c.succeed(
                f"nix-build --no-out-link --store 'ssh-ng://{host}' "
                "--arg busybox '(builtins.storePath ${busybox})' "
                f"{drv} 2>&1"
            )
            print(f"ingress-{label}: {out}")
            assert "/nix/store/" in out, (
                f"nix-build via {label} did not produce a store path:\n{out}"
            )

    # ══════════════════════════════════════════════════════════════════
    # egress: NAT64 datapath (Jool) + DNS64 synthesis (CoreDNS)
    # ══════════════════════════════════════════════════════════════════
    upstream_v4_ip = upstream_v4.succeed(
        "ip -4 -o addr show dev eth1 | "
        "${pkgs.gawk}/bin/awk '{split($4,a,\"/\"); print a[1]}'"
    ).strip()
    print(f"upstream-v4 eth1 = {upstream_v4_ip}")
    # RFC 6052 v4-embedded form. inet_pton accepts dotted-quad in the
    # last 32 bits; equivalent to 64:ff9b::c0a8:01NN.
    nat64_addr = f"64:ff9b::{upstream_v4_ip}"

    with subtest("egress-v6-direct: k3s host reaches upstream-v6:8080"):
        # Positive control — if THIS fails, the v6 vlan is broken and
        # the NAT64 check below is meaningless.
        k3s_server.succeed(
            "${curl} -sf --connect-timeout 5 http://upstream-v6:8080/ -o /dev/null"
        )

    with subtest("egress-nat64: k3s host reaches upstream-v4 via 64:ff9b::"):
        # k3s nodes have NO eth1-v4 (k3sV6Only). The only path to a
        # v4-only upstream is the 64:ff9b::/96 static route → edge →
        # Jool. http.server returns 200 on / (directory listing).
        # wait_until_succeeds: Jool's instance is up (waitReady gated
        # on the unit) but the first translated SYN can race route-
        # install on a slow boot.
        print(k3s_server.succeed(f"ip -6 route get {nat64_addr}"))
        print(edge.succeed("jool global display 2>&1"))
        try:
            k3s_server.wait_until_succeeds(
                f"${curl} -sf --connect-timeout 5 'http://[{nat64_addr}]:8080/' -o /dev/null",
                timeout=30,
            )
        except Exception:
            print("=== NAT64 FAIL — diagnostics ===")
            print(edge.succeed("jool stats display --all 2>&1 || true"))
            print(edge.succeed("ip -4 addr; ip -4 route; ip -6 route"))
            print(k3s_server.execute(
                f"${curl} -sv --connect-timeout 5 'http://[{nat64_addr}]:8080/' 2>&1"
            )[1])
            raise
        # Jool session table proves the translation actually went
        # through this box (not some accidental v4 leak).
        sess = edge.succeed("jool session display --numeric 2>&1")
        print(f"jool sessions:\n{sess}")
        assert upstream_v4_ip in sess, (
            f"expected upstream-v4 {upstream_v4_ip} in Jool session "
            f"table — NAT64 path not exercised:\n{sess}"
        )

    with subtest("egress-dns64: CoreDNS resolves upstream-v4 → 64:ff9b:: AAAA"):
        # Prove CoreDNS dns64 synthesises. Query kube-dns directly
        # from the k3s host — socketLB intercepts the connect to the
        # Service ClusterIP. (Avoids pulling a dig image into the
        # airgap set; nsenter-into-pod-netns would still use the
        # HOST resolv.conf, not the pod's.)
        coredns_ip = k3s_server.succeed(
            "k3s kubectl -n kube-system get svc kube-dns "
            "-o jsonpath='{.spec.clusterIP}'"
        ).strip()
        out = k3s_server.succeed(
            f"${dig} +short @{coredns_ip} upstream-v4 AAAA"
        )
        print(f"dns64 lookup upstream-v4 AAAA:\n{out}")
        assert "64:ff9b:" in out, (
            f"CoreDNS dns64 did not synthesise 64:ff9b:: AAAA for "
            f"upstream-v4 (A-only in coredns-custom hosts block):\n{out}"
        )
        # Positive control: upstream-v6 has a real AAAA, no synthesis.
        out6 = k3s_server.succeed(
            f"${dig} +short @{coredns_ip} upstream-v6 AAAA"
        )
        assert "2001:db8:" in out6 and "64:ff9b:" not in out6, (
            f"upstream-v6 should resolve to its real GUA, not a "
            f"synthesised 64:ff9b:: address:\n{out6}"
        )

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
