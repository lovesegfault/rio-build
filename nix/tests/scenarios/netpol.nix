# NetworkPolicy egress enforcement: worker pod blocked from IMDS +
# public internet + k8s API.
#
# networkPolicy.enabled=true (via extraValues in default.nix) renders
# infra/helm/rio-build/templates/networkpolicy.yaml → rio-worker-egress
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
# default-workers-0 gives the right PID.
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

  covTimeoutHeadroom = if common.coverage then 300 else 0;
in
pkgs.testers.runNixOSTest {
  name = "rio-netpol";
  skipTypeCheck = true;

  # k3s bring-up ~4min + kube-router sync + a handful of 5s connect
  # probes. No builds, no rollouts.
  globalTimeout = 600 + covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

    import time

    ${common.kvmPreopen}
    start_all()
    ${fixture.waitReady}
    ${fixture.kubectlHelpers}

    # ── NetworkPolicy object exists (helm template rendered + applied) ──
    # vmtest-full.yaml has networkPolicy.enabled=false; extraValues
    # override → "true" (string, but {{ if }} is truthy on non-empty).
    # helm-render.nix puts NetworkPolicy (not Namespace/RBAC kind) in
    # 02-workloads.yaml → k3s auto-applies it at boot along with
    # everything else. If this get fails, the helm override didn't take.
    kubectl("get networkpolicy rio-worker-egress -o name")

    # kube-router watches NetworkPolicy CRs → iptables rules. Watch
    # latency is usually sub-second but the policy was applied at
    # cluster boot WITH the pods, so kube-router may still be building
    # the chain when waitReady returns. P0220's positive result was
    # with a manually-applied policy mid-test (hot path). 10s is
    # generous; the positive-control probe below is the real gate.
    time.sleep(10)

    # ── Resolve worker pod → VM node → container PID ──────────────────
    # Same node-resolution trick as lifecycle.nix:561-565 — STS may
    # schedule default-workers-0 to either k3s-server or k3s-agent.
    # Then crictl → container PID → nsenter -n into its netns. All
    # containers in one pod share one netns (pause container's), so
    # head -1 of any running container works.
    worker_node = kubectl(
        "get pod default-workers-0 -o jsonpath='{.spec.nodeName}'"
    ).strip()
    worker_vm = k3s_agent if worker_node == "k3s-agent" else k3s_server
    print(f"netpol: default-workers-0 is on {worker_node}")

    # crictl ps --label filters to THIS pod's containers. -q gives bare
    # container IDs. k3s bundles crictl (k3s crictl). .info.pid is the
    # container's PID 1 in the host's PID namespace — nsenter -t target.
    cid = worker_vm.succeed(
        "k3s crictl ps -q "
        "--label io.kubernetes.pod.name=default-workers-0 | head -1"
    ).strip()
    assert cid, "no running container found for default-workers-0"
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
    # rio-worker-egress explicitly allows TCP→rio-scheduler:9001. If
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
    # rio-worker-egress doesn't list the apiserver in its allow-rules
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
            "NetPol NOT enforcing. rio-worker-egress does not allow "
            "apiserver; kube-router should DROP. P0220 proved this IP "
            "IS reachable without policy, so rc=0 means policy absent "
            "or kube-router not loading it."
        )
        print(f"netpol-kubeapi PASS: k8s API {api_ip}:443 blocked (rc={rc})")

    # ══════════════════════════════════════════════════════════════════
    # netpol-imds — IMDS egress blocked (exit criterion)
    # ══════════════════════════════════════════════════════════════════
    # IMDS is the prime threat — a sandbox escapee shouldn't get AWS
    # creds. 169.254.169.254 is link-local; not in any rio-worker-egress
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
            "enforcing. 0.0.0.0/0 not in rio-worker-egress allow-rules."
        )
        print(f"netpol-internet PASS: 1.1.1.1 blocked (curl rc={rc})")

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
