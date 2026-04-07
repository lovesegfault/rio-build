# NetworkPolicy egress enforcement: worker pod blocked from IMDS +
# public internet + k8s API.
#
# networkPolicy.enabled=true (via extraValues in default.nix) renders
# infra/helm/rio-build/templates/networkpolicy.yaml → rio-builder-egress
# policy (allows DNS + scheduler:9001 + store:9002 + fod-proxy:3128 ONLY).
# Stock k3s kube-router enforces (P0220 pre-verify: deny-all-egress
# blocks in-cluster 10.43.0.1:443 and external 1.1.1.1; deleting the
# policy restores connectivity → no Calico preload needed).
#
# Probe mechanics: worker image has NO curl/wget (docker.nix: only cacert
# + tzdata + rio-workspace + nix + fuse3 + util-linux). `kubectl exec --
# curl` is a no-go. Instead: nsenter -n into the pod's netns from the VM
# host (which HAS curl — k3sBase systemPackages), keeping the host's
# mountns so ${pkgs.curl}/${pkgs.netcat} store paths resolve. All
# containers in a k8s pod share one netns, so any running container in
# rio-builder-x86-64-0 gives the right PID.
#
# "Proves nothing" guard (review pattern — test asserts its own
# precondition): rc≠0 to 169.254.169.254 / 1.1.1.1 is vacuous in an
# airgapped VM (unreachable regardless of NetPol). Positive control
# first: nc -z to rio-scheduler's ClusterIP:9001 MUST succeed (NetPol
# explicitly allows it). If THAT is blocked, the policy is over-broad
# or kube-router hasn't synced → the rc≠0 asserts below are worthless.
# Then probe the k8s API ClusterIP (10.43.0.1:443 — P0220's proven
# in-cluster differentiator): reachable without NetPol, blocked with.
#
# Ingress policy: SKIPPED (Phase 5). FOD proxy egress allowlist: P0243.
{
  pkgs,
  common,
  fixture,
}:
let
  # Store-path interpolation pulls tools into the VM closure. curl is
  # already in k3sBase systemPackages (k3s-full.nix:189) but explicit
  # paths survive PATH surprises inside nsenter.
  curl = "${pkgs.curl}/bin/curl";
  nc = "${pkgs.netcat}/bin/nc";
  jq = "${pkgs.jq}/bin/jq";
  dig = "${pkgs.dnsutils}/bin/dig";
  inherit (fixture) nsBuilders nsStore;
  drvs = import ../lib/derivations.nix { inherit pkgs; };
  # Ephemeral workers: spawn one via a long-sleep build so the netns
  # probes have a pod to enter. 300s outlives the probe sequence.
  warmupDrv = drvs.mkTrivial {
    marker = "netpol-warmup";
    sleepSecs = 300;
  };
in
pkgs.testers.runNixOSTest {
  name = "rio-netpol";
  skipTypeCheck = true;

  # k3s bring-up ~4min + kube-router sync + a handful of 5s connect
  # probes. No builds, no rollouts.
  # +180s for sshKeySetup gateway-bounce + warmup Job spawn.
  globalTimeout = 780 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

    import time

    ${common.kvmCheck}
    start_all()
    ${fixture.waitReady}
    ${fixture.kubectlHelpers}
    ${fixture.sshKeySetup}
    ${common.seedBusybox "k3s-server"}
    ${common.mkBuildHelperV2 {
      gatewayHost = "k3s-server";
      dumpLogsExpr = ''print("(netpol: build failed; pod logs follow)")'';
    }}

    # Ephemeral workers: no pod exists until a build is queued. Submit
    # a long-sleep build in the background so the probes below have a
    # netns to enter. The build is never awaited — test ends before it
    # completes.
    client.succeed(
        "nohup nix-build --no-out-link --store ssh-ng://k3s-server "
        "--arg busybox '(builtins.storePath ${common.busybox})' "
        "${warmupDrv} >/tmp/warmup.log 2>&1 &"
    )
    wp = wait_worker_pod(timeout=180)
    print(f"netpol: warmup spawned worker pod {wp}")

    # ── NetworkPolicy object exists (helm template rendered + applied) ──
    # vmtest-full.yaml has networkPolicy.enabled=false; extraValues
    # override → "true" (string, but {{ if }} is truthy on non-empty).
    # helm-render.nix puts NetworkPolicy (not Namespace/RBAC kind) in
    # 02-workloads.yaml → k3s auto-applies it at boot along with
    # everything else. If this get fails, the helm override didn't take.
    # ADR-019: builder-egress policy lives in rio-builders namespace.
    kubectl("get networkpolicy builder-egress -o name", ns="${nsBuilders}")

    # kube-router watches NetworkPolicy CRs → iptables rules. Watch
    # latency is usually sub-second but the policy was applied at
    # cluster boot WITH the pods, so kube-router may still be building
    # the chain when waitReady returns. P0220's positive result was
    # with a manually-applied policy mid-test (hot path). 10s is
    # generous; the positive-control probe below is the real gate.
    time.sleep(10)

    # ── Resolve worker pod → VM node → container PID ──────────────────
    # The Job pod may schedule to either k3s-server or k3s-agent. Then
    # crictl → container PID → nsenter -n into its netns. All
    # containers in one pod share one netns (pause container's), so
    # head -1 of any running container works.
    worker_node = kubectl(
        f"get pod {wp} -o jsonpath='{{.spec.nodeName}}'",
        ns="${nsBuilders}",
    ).strip()
    worker_vm = k3s_agent if worker_node == "k3s-agent" else k3s_server
    print(f"netpol: {wp} is on {worker_node}")

    # crictl ps --label filters to THIS pod's containers. -q gives bare
    # container IDs. k3s bundles crictl (k3s crictl). .info.pid is the
    # container's PID 1 in the host's PID namespace — nsenter -t target.
    cid = worker_vm.succeed(
        "k3s crictl ps -q "
        f"--label io.kubernetes.pod.name={wp} | head -1"
    ).strip()
    assert cid, f"no running container found for {wp}"
    pid = worker_vm.succeed(
        f"k3s crictl inspect {cid} | ${jq} -r .info.pid"
    ).strip()
    assert pid and pid != "0", f"crictl inspect returned bad pid: {pid!r}"
    print(f"netpol: worker container pid={pid}")

    # nsenter -n (netns ONLY — keep host mountns so store-path curl/nc
    # resolve). DNS inside the probe uses the HOST's /etc/resolv.conf
    # (we didn't enter mountns), NOT the pod's CoreDNS — so all probes
    # are raw-IP, no hostnames.
    def netns_exec(cmd):
        return worker_vm.execute(f"nsenter -t {pid} -n -- {cmd}")

    # ══════════════════════════════════════════════════════════════════
    # netpol-positive — ALLOWED egress works (non-vacuous gate)
    # ══════════════════════════════════════════════════════════════════
    # rio-builder-egress explicitly allows TCP→rio-scheduler:9001. If
    # this nc -z fails, either (a) kube-router hasn't synced the allow
    # rule yet (10s wasn't enough), or (b) the policy is broken (blocks
    # everything), or (c) nsenter/PID resolution is wrong. In ANY of
    # those cases, the rc≠0 asserts below prove NOTHING — they'd pass
    # on a dead network.
    #
    # Raw ClusterIP (no DNS — see nsenter note above). nc -z -w5: pure
    # TCP connect, 5s timeout, exit 0 iff SYN-ACK received. The 9001
    # port is mTLS gRPC — nc doesn't care, it just wants the handshake.
    with subtest("netpol-positive: allowed egress (scheduler:9001) connects"):
        sched_ip = kubectl(
            "get svc rio-scheduler -o jsonpath='{.spec.clusterIP}'"
        ).strip()
        assert sched_ip, "rio-scheduler Service has no ClusterIP"
        rc, out = netns_exec(f"${nc} -z -w5 {sched_ip} 9001")
        assert rc == 0, (
            f"nc -z to rio-scheduler:{sched_ip}:9001 FAILED (rc={rc}). "
            "NetPol ALLOWS this — if blocked, either kube-router not "
            "synced or policy over-broad. Subsequent rc!=0 asserts "
            f"would be VACUOUS.\n{out}"
        )
        print(f"netpol-positive PASS: scheduler ClusterIP {sched_ip}:9001 "
              "reachable from worker netns (NetPol allow-rule firing)")

    # ══════════════════════════════════════════════════════════════════
    # netpol-kubeapi — k8s API ClusterIP BLOCKED (P0220 differentiator)
    # ══════════════════════════════════════════════════════════════════
    # P0220 empirically validated this exact probe: 10.43.0.1:443 (the
    # `kubernetes` Service in default ns, k3s's fixed apiserver VIP) is
    # REACHABLE from pods without NetPol, BLOCKED with deny-all-egress.
    # rio-builder-egress doesn't list the apiserver in its allow-rules
    # (deliberate — a sandbox escapee shouldn't get a k8s API token
    # AND be able to use it). kube-router DROP → nc hangs until -w5.
    #
    # Stronger than the IMDS/1.1.1.1 probes below: those fail in an
    # airgapped VM regardless. THIS one would succeed if NetPol were
    # off (P0220 proved it).
    with subtest("netpol-kubeapi: k8s API ClusterIP blocked"):
        api_ip = k3s_server.succeed(
            "k3s kubectl get svc kubernetes -n default "
            "-o jsonpath='{.spec.clusterIP}'"
        ).strip()
        assert api_ip, "kubernetes Service has no ClusterIP"
        rc, out = netns_exec(f"${nc} -z -w5 {api_ip} 443")
        assert rc != 0, (
            f"nc -z to kubernetes API {api_ip}:443 SUCCEEDED (rc=0) — "
            "NetPol NOT enforcing. rio-builder-egress does not allow "
            "apiserver; kube-router should DROP. P0220 proved this IP "
            "IS reachable without policy, so rc=0 means policy absent "
            "or kube-router not loading it."
        )
        print(f"netpol-kubeapi PASS: k8s API {api_ip}:443 blocked (rc={rc})")

    # ══════════════════════════════════════════════════════════════════
    # netpol-imds — IMDS egress blocked (exit criterion)
    # ══════════════════════════════════════════════════════════════════
    # IMDS is the prime threat — a sandbox escapee shouldn't get AWS
    # creds. 169.254.169.254 is link-local; not in any rio-builder-egress
    # allow-rule → default-deny. kube-router DROP → curl times out.
    #
    # WEAK in VM context: no IMDS listener exists in a NixOS QEMU VM
    # anyway (not an EC2 instance). rc≠0 would happen regardless. The
    # netpol-kubeapi subtest above is the REAL proof; this one satisfies
    # the plan's exit criterion and matches production's threat model.
    with subtest("netpol-imds: IMDS egress blocked"):
        rc, out = netns_exec(
            "${curl} --max-time 5 -sS http://169.254.169.254/latest/meta-data/"
        )
        assert rc != 0, (
            "IMDS curl succeeded (rc=0) — NetPol NOT enforcing. "
            "169.254.169.254 is link-local, not in any allow-rule; "
            f"kube-router should DROP.\n{out}"
        )
        print(f"netpol-imds PASS: IMDS blocked (curl rc={rc})")

    # ══════════════════════════════════════════════════════════════════
    # netpol-internet — public egress blocked (exit criterion)
    # ══════════════════════════════════════════════════════════════════
    # Same WEAK-in-VM caveat: airgapped VM has no route to 1.1.1.1.
    # Plan exit criterion; netpol-kubeapi above carries the proof.
    with subtest("netpol-internet: public egress blocked"):
        rc, out = netns_exec(
            "${curl} --max-time 5 -sS http://1.1.1.1/"
        )
        assert rc != 0, (
            "public egress to 1.1.1.1 succeeded (rc=0) — NetPol NOT "
            "enforcing. 0.0.0.0/0 not in rio-builder-egress allow-rules."
        )
        print(f"netpol-internet PASS: 1.1.1.1 blocked (curl rc={rc})")

    # ══════════════════════════════════════════════════════════════════
    # netpol-dns-tcp — DNS over TCP/53 allowed (T3 fix)
    # ══════════════════════════════════════════════════════════════════
    # builder-egress now allows BOTH UDP+TCP/53 to CoreDNS. DNS falls
    # back to TCP for responses >512 bytes (DNSSEC, large SRV records).
    # dig +tcp forces TCP; without the T3 fix kube-router DROPs the SYN
    # and dig returns "connection refused" (rc≠0). Reuses the builder
    # netns_exec from above — same pod, same nsenter PID.
    with subtest("netpol-dns-tcp: DNS over TCP/53 allowed"):
        coredns_ip = k3s_server.succeed(
            "k3s kubectl -n kube-system get svc kube-dns "
            "-o jsonpath='{.spec.clusterIP}'"
        ).strip()
        assert coredns_ip, "kube-dns Service has no ClusterIP"
        # +tcp forces TCP transport. +short: bare answer only. +time=5:
        # per-try timeout. Resolving rio-scheduler's cluster-local name
        # — no external network needed, pure in-cluster CoreDNS probe.
        rc, out = netns_exec(
            f"${dig} +tcp +short +time=5 "
            f"rio-scheduler.rio-system.svc.cluster.local @{coredns_ip}"
        )
        assert rc == 0 and out.strip(), (
            f"dig +tcp to CoreDNS {coredns_ip} FAILED (rc={rc}). "
            "builder-egress should allow TCP/53 to kube-system; if "
            "blocked, the T3 fix didn't render or kube-router hasn't "
            f"synced.\n{out}"
        )
        print(f"netpol-dns-tcp PASS: dig +tcp resolved via {coredns_ip} "
              f"→ {out.strip()}")

    # ══════════════════════════════════════════════════════════════════
    # netpol-store-egress — store pod IMDS blocked, postgres allowed
    # ══════════════════════════════════════════════════════════════════
    # store-egress NetworkPolicy (T2): DNS + postgres:5432/RFC1918 +
    # optional S3:443. NOT IMDS, NOT k8s API, NOT public IPs. The store
    # holds S3 + postgres creds — a compromised store MUST NOT reach
    # IMDS for role escalation.
    #
    # Same nsenter mechanics as the builder probe above, but for a
    # rio-store pod (lives in rio-store namespace per ADR-019).
    with subtest("netpol-store-egress: store IMDS blocked, postgres allowed"):
        kubectl("get networkpolicy store-egress -o name", ns="${nsStore}")

        store_pod = kubectl(
            "get pod -l app.kubernetes.io/name=rio-store "
            "-o jsonpath='{.items[0].metadata.name}'",
            ns="${nsStore}",
        ).strip()
        assert store_pod, "no rio-store pod found in ${nsStore}"
        store_node = kubectl(
            f"get pod {store_pod} -o jsonpath='{{.spec.nodeName}}'",
            ns="${nsStore}",
        ).strip()
        store_vm = k3s_agent if store_node == "k3s-agent" else k3s_server
        print(f"netpol-store: {store_pod} on {store_node}")

        store_cid = store_vm.succeed(
            f"k3s crictl ps -q "
            f"--label io.kubernetes.pod.name={store_pod} | head -1"
        ).strip()
        assert store_cid, f"no running container for {store_pod}"
        store_pid = store_vm.succeed(
            f"k3s crictl inspect {store_cid} | ${jq} -r .info.pid"
        ).strip()
        assert store_pid and store_pid != "0", (
            f"bad pid for store: {store_pid!r}"
        )

        def store_netns(cmd):
            return store_vm.execute(
                f"nsenter -t {store_pid} -n -- {cmd}"
            )

        # Positive control: store-egress ALLOWS postgres:5432 on
        # RFC1918. The bitnami rio-postgresql Service lives in
        # rio-system (NOT rio-store — it's a control-plane dependency)
        # with a 10.43.x.x ClusterIP (k3s Service CIDR, RFC1918). The
        # policy matches by ipBlock CIDR, not namespace, so cross-ns
        # is fine. If this nc -z fails, the policy is over-broad or
        # kube-router hasn't synced → the IMDS rc≠0 assert below is
        # VACUOUS.
        pg_ip = kubectl(
            "get svc rio-postgresql -o jsonpath='{.spec.clusterIP}'"
        ).strip()
        assert pg_ip, "rio-postgresql Service has no ClusterIP"
        rc, out = store_netns(f"${nc} -z -w5 {pg_ip} 5432")
        assert rc == 0, (
            f"nc -z to postgres {pg_ip}:5432 FAILED (rc={rc}). "
            "store-egress ALLOWS RFC1918:5432 — if blocked, policy "
            f"over-broad or kube-router not synced.\n{out}"
        )
        print(f"netpol-store positive PASS: postgres {pg_ip}:5432 "
              "reachable from store netns")

        # Negative: IMDS blocked. Same WEAK-in-VM caveat as netpol-imds
        # above (no IMDS listener in QEMU). The positive control +
        # store-egress presence check together prove the policy is
        # loaded and selective.
        rc, out = store_netns(
            "${curl} --max-time 5 -sS "
            "http://169.254.169.254/latest/meta-data/"
        )
        assert rc != 0, (
            "IMDS curl from store netns succeeded (rc=0) — store-egress "
            "NOT enforcing. 169.254.0.0/16 is not in any allow-rule; "
            f"kube-router should DROP.\n{out}"
        )
        print(f"netpol-store IMDS PASS: blocked (curl rc={rc})")

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
