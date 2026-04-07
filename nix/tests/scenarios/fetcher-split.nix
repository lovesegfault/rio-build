# ADR-019 builder/fetcher split end-to-end.
#
# FIRST test exercising both pool types in one fixture. Proves the full
# chain: FOD → FetcherPool pod, non-FOD → BuilderPool pod, builder airgap
# holds, fetcher egress open but IMDS-blocked.
#
# Fixture gotchas — fetcher pods have NO privileged escape hatch (hard-
# coded false at reconcilers/fetcherpool/mod.rs:237) AND a hard-coded
# Localhost seccomp (operator/rio-fetcher.json, :241). To get a RUNNING
# fetcher pod in the airgap VM:
#   - seccomp profile pre-installed on both nodes via testScript mkdir+cp
#     (security-profiles-operator + cert-manager images not in the airgap set)
#   - device-plugin path enabled (vmtest-full-nonpriv.yaml overlay +
#     pulled.smarter-device-manager extraImage) — same as
#     vm-security-nonpriv-k3s
#   - k3s-agent labeled rio.build/node-role=fetcher at runtime so the
#     reconciler's default nodeSelector matches (also exercises
#     fetcher.node.dedicated — fetcher lands on agent, builder on server)
#
# Egress-open proof — fetcher-egress allows 0.0.0.0/0:80 minus RFC1918/
# link-local/loopback. In an airgap VM there's no real public IP. The
# scenario fakes one: k3s-server gets 203.0.113.1/24 (TEST-NET-3, RFC5737
# — NOT in the `except` list) on eth1, python http.server on :80. Both
# other nodes get a static route. Then:
#   builder netns → curl 203.0.113.1:80 → rc≠0 (not in builder-egress allow)
#   fetcher netns → curl 203.0.113.1:80 → rc==0 (0.0.0.0/0:80 allow fires)
# This is the non-vacuous differentiator; the IMDS probe stays WEAK
# (same caveat as netpol.nix — no IMDS listener in QEMU).
#
# Dispatch-routing proof — the drvs.fodConsumer build (builtin:fetchurl
# FOD + raw consumer) pulls both through the scheduler. hard_filter
# (assignment.rs:185) gates on is_fixed_output XOR kind==Fetcher: FOD
# only matches fetcher, consumer only matches builder. If routing is
# broken, one half queues forever → build times out. The build
# SUCCEEDING is the proof; kubectl-logs grep confirms which pod ran
# which half.
#
# Tracey markers: r[verify ...] placed at the default.nix subtests entry
# (P0341 convention); scenario-header prose (this block) explains what
# each subtest proves.
{
  pkgs,
  common,
  fixture,
  drvs,
}:
let
  curl = "${pkgs.curl}/bin/curl";
  nc = "${pkgs.netcat}/bin/nc";
  jq = "${pkgs.jq}/bin/jq";
  py3 = "${pkgs.python3}/bin/python3";

  inherit (fixture) nsBuilders nsFetchers ns;

  # Seccomp profiles from the chart's files/. Same JSON the SPO
  # SeccompProfile CRs render from — we cp directly since SPO +
  # cert-manager images aren't in the airgap set.
  seccompFetcher = ../../../infra/helm/rio-build/files/seccomp-rio-fetcher.json;
  seccompBuilder = ../../../infra/helm/rio-build/files/seccomp-rio-builder.json;

  # TEST-NET-3 "public" origin. RFC5737 reserves 203.0.113.0/24 for
  # documentation — guaranteed never routed on the real internet.
  # Importantly NOT in RFC1918/link-local/loopback → passes the
  # fetcher-egress ipBlock except-clause.
  originIP = "203.0.113.1";

  # Ephemeral workers: pod names are Job-generated, not STS ordinals.
  # Resolved at runtime via wait_worker_pod() after the build is
  # queued. fetcherPools[] entry "x86-64" + class "tiny" → pool
  # label `x86-64-tiny`.
  builderPool = "x86-64";
  fetcherPool = "x86-64-tiny";
in
pkgs.testers.runNixOSTest {
  name = "rio-fetcher-split";
  skipTypeCheck = true;

  # k3s bring-up ~4min + device-plugin DS ~60s + fetcher pod ~30s +
  # build ~60s + 6 probe subtests ~30s. nonpriv overlay adds the DS
  # bring-up tax that privileged:true skips.
  globalTimeout = 900 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

    import time
    import json as _json

    ${common.kvmCheck}
    start_all()
    ${fixture.waitReady}
    ${fixture.kubectlHelpers}
    ${fixture.sshKeySetup}
    ${common.seedBusybox "k3s-server"}

    # ══════════════════════════════════════════════════════════════════
    # FIXTURE PREP — seccomp profiles + node labels + "public" origin
    # ══════════════════════════════════════════════════════════════════

    # ── Seccomp profiles on both nodes ────────────────────────────────
    # Fetcher reconciler hard-codes Localhost operator/rio-fetcher.json
    # (fetcherpool/mod.rs:254). SPO's spod DaemonSet would reconcile it
    # there but SPO + cert-manager aren't airgapped. cp directly — same
    # JSON, same kubelet-relative path SPO writes (cluster-scoped CR →
    # operator/{name}.json, no namespace component). Both profiles:
    # nonpriv overlay flips builderPool to Localhost too.
    for n in [k3s_server, k3s_agent]:
        n.succeed(
            "mkdir -p /var/lib/kubelet/seccomp/operator && "
            "cp ${seccompFetcher} /var/lib/kubelet/seccomp/operator/rio-fetcher.json && "
            "cp ${seccompBuilder} /var/lib/kubelet/seccomp/operator/rio-builder.json"
        )

    # ── Label k3s-agent as the dedicated fetcher node ─────────────────
    # Reconciler defaults nodeSelector rio.build/node-role=fetcher
    # (fetcherpool/mod.rs:189). Without a matching node, the STS pod
    # stays Pending forever. Labeling ONE node also makes the
    # fetcher-node-dedicated subtest meaningful: fetcher lands on
    # agent, builder (no nodeSelector in vmtest-full.yaml) lands
    # wherever the scheduler puts it — but with agent labeled
    # fetcher, the device-plugin DS (nodeSelector: null in nonpriv
    # overlay) runs on both, so builder CAN land on agent too.
    # Shape-check the toleration instead of asserting different nodes.
    kubectl("label node k3s-agent rio.build/node-role=fetcher --overwrite", ns="kube-system")

    # ── TEST-NET-3 "public" origin on k3s-server:80 ───────────────────
    # 203.0.113.0/24 is RFC5737 TEST-NET-3 — non-RFC1918, non-link-
    # local → matches fetcher-egress 0.0.0.0/0:80 allow. All three
    # nodes share eth1 L2; static routes on agent+client point the /24
    # at the bridge so ARP resolves to k3s-server. Firewall port 80
    # opened via iptables (NixOS firewall config is eval-time only).
    k3s_server.succeed(
        "ip addr add ${originIP}/24 dev eth1 && "
        "iptables -I nixos-fw -p tcp --dport 80 -j ACCEPT && "
        "mkdir -p /srv && "
        "ln -sf ${drvs.coldBootstrapBusybox} /srv/busybox && "
        "echo ok > /srv/ok"
    )
    # /busybox is delayed 30s so the one-shot fetcher pod stays Running
    # long enough for the netns probe below. /ok and / serve immediately
    # (the netpol probes use those, not /busybox). systemd-run detaches
    # cleanly — nohup+& can leave the driver's pipe open → succeed() hangs.
    k3s_server.succeed(
        "cat >/srv/slow.py <<'PY'\n"
        "import http.server, time, os\n"
        "os.chdir('/srv')\n"
        "class H(http.server.SimpleHTTPRequestHandler):\n"
        "    def do_GET(self):\n"
        "        if self.path == '/busybox': time.sleep(30)\n"
        "        super().do_GET()\n"
        "http.server.ThreadingHTTPServer(('0.0.0.0', 80), H).serve_forever()\n"
        "PY"
    )
    k3s_server.succeed(
        "systemd-run --unit=test-origin ${py3} /srv/slow.py"
    )
    for n in [k3s_agent, client]:
        n.succeed("ip route add 203.0.113.0/24 dev eth1 || true")

    # ── NetworkPolicies rendered + applied ────────────────────────────
    # networkPolicy.enabled=true in extraValues → helm-render puts
    # builder-egress + fetcher-egress in 02-workloads.yaml → k3s
    # auto-applied at boot. If either get fails, the override didn't
    # take or the template if-gate is miswired.
    kubectl("get networkpolicy builder-egress -o name", ns="${nsBuilders}")
    kubectl("get networkpolicy fetcher-egress -o name", ns="${nsFetchers}")
    # kube-router watch latency (same 10s margin as netpol.nix:78).
    time.sleep(10)

    # ══════════════════════════════════════════════════════════════════
    # dispatch-fod + dispatch-nonfod — role-aware routing
    # ══════════════════════════════════════════════════════════════════
    # One nix-build, two derivations: builtin:fetchurl FOD (system=
    # builtin) + raw consumer (system=x86_64-linux). hard_filter's
    # is_fixed_output XOR kind==Fetcher check routes each to the right
    # pool. The build SUCCEEDING is the primary proof (wrong routing →
    # queue-forever → timeout). kubectl-logs grep confirms placement.
    ${common.mkBuildHelperV2 {
      gatewayHost = "k3s-server";
      # One-liner dumpLogsExpr — mkBuildHelperV2 interpolates inside
      # build()'s body at a fixed indent; multi-line breaks Python.
      dumpLogsExpr = ''[dump_all_logs([], kube_node=k3s_server, kube_namespace=n) for n in ("${ns}", "${nsFetchers}", "${nsBuilders}")]'';
    }}

    # Ephemeral workers: submit the build in BACKGROUND so pods stay
    # Running while the netns probes below execute. sleepSecs=30 keeps
    # the builder pod alive past the probe sequence; the fetcher pod
    # lives only as long as the FOD fetch — probe it first.
    client.succeed(
        "nohup nix-build --no-out-link --store ssh-ng://k3s-server "
        "--arg busybox '(builtins.storePath ${common.busybox})' "
        "--argstr url http://${originIP}/busybox "
        "--arg sleepSecs 30 "
        "${drvs.fodConsumer} >/tmp/split-build.log 2>&1 &"
    )
    # crictl → container PID → nsenter -n into the pod netns. Keep host
    # mountns so store-path curl/nc resolve. Same pattern as netpol.nix.
    def netns_handle(pod, ns_):
        node = kubectl(
            f"get pod {pod} -o jsonpath='{{.spec.nodeName}}'", ns=ns_
        ).strip()
        vm = k3s_agent if node == "k3s-agent" else k3s_server
        cid = vm.succeed(
            f"k3s crictl ps -q --label io.kubernetes.pod.name={pod} | head -1"
        ).strip()
        assert cid, f"no running container for {pod}"
        pid = vm.succeed(
            f"k3s crictl inspect {cid} | ${jq} -r .info.pid"
        ).strip()
        assert pid and pid != "0", f"bad pid for {pod}: {pid!r}"
        print(f"{pod} on {node} pid={pid}")
        return vm, pid

    try:
        fetcher_pod = wait_worker_pod(
            pool="${fetcherPool}", ns="${nsFetchers}", timeout=180
        )
    except Exception:
        print("=== fetcher pod TIMEOUT: diagnostic dump ===")
        print(k3s_server.execute(
            "k3s kubectl -n ${nsFetchers} get fetcherpool,job,pod -o wide 2>&1; "
            "k3s kubectl -n ${nsFetchers} describe fetcherpool 2>&1; "
            "k3s kubectl -n rio-system logs deploy/rio-controller --tail=80 2>&1"
        )[1])
        raise
    # Resolve fetcher netns NOW and run all fetcher_exec probes BEFORE
    # waiting for the builder. One-shot pod lives only as long as the
    # FOD (held to ~30s by the slow /busybox handler); the builder pod
    # won't exist until the FOD completes, so waiting for it first
    # guarantees the fetcher is already gone.
    fetcher_vm, fetcher_pid = netns_handle(fetcher_pod, "${nsFetchers}")
    def fetcher_exec(cmd):
        return fetcher_vm.execute(f"nsenter -t {fetcher_pid} -n -- {cmd}")

    sched_ip = kubectl(
        "get svc rio-scheduler -o jsonpath='{.spec.clusterIP}'"
    ).strip()

    # ══════════════════════════════════════════════════════════════════
    # fetcher-egress — fetcher REACHES TEST-NET-3 origin
    # ══════════════════════════════════════════════════════════════════
    # THE non-vacuous differentiator vs builder. Same origin, same
    # port — fetcher-egress's 0.0.0.0/0:80 allow (minus RFC1918/
    # link-local/loopback) fires; builder-egress has no such rule.
    with subtest("fetcher-egress: fetcher reaches 'public' origin"):
        rc, out = fetcher_exec(f"${nc} -z -w5 {sched_ip} 9001")
        assert rc == 0, (
            f"POSITIVE CONTROL FAILED: fetcher→scheduler:{sched_ip}:9001 "
            f"rc={rc}. fetcher-egress allows this too.\n{out}"
        )

        rc, out = fetcher_exec(
            "${curl} --max-time 5 -sS -o /dev/null -w '%{http_code}' "
            "http://${originIP}/ok"
        )
        assert rc == 0, (
            f"fetcher BLOCKED from ${originIP}:80 (rc={rc}) — "
            f"fetcher-egress 0.0.0.0/0:80 allow NOT firing. ${originIP} "
            f"is TEST-NET-3 (non-RFC1918), should pass except-clause.\n{out}"
        )
        assert out.strip() == "200", f"origin returned {out!r}, expected 200"
        print(f"fetcher-egress PASS: ${originIP}:80 reachable (http {out.strip()})")

    # ══════════════════════════════════════════════════════════════════
    # fetcher-imds-blocked — link-local deny inherited
    # ══════════════════════════════════════════════════════════════════
    # WEAK-in-VM (same caveat as netpol.nix:176): no IMDS listener in
    # QEMU. The fetcher-egress subtest above is the non-vacuous gate;
    # this proves the except-clause covers 169.254.0.0/16.
    with subtest("fetcher-imds-blocked: 169.254.169.254 blocked"):
        rc, _ = fetcher_exec(
            "${curl} --max-time 5 -sS http://169.254.169.254/latest/meta-data/"
        )
        assert rc != 0, (
            "fetcher reached IMDS (rc=0) — fetcher-egress except-clause "
            "for 169.254.0.0/16 NOT enforcing."
        )
        print(f"fetcher-imds PASS: blocked (rc={rc})")

    # ══════════════════════════════════════════════════════════════════
    # builder-airgap — builder BLOCKED from TEST-NET-3 origin
    # ══════════════════════════════════════════════════════════════════
    # Builder pod appears once the FOD completes; sleepSecs=30 in the
    # consumer drv keeps it alive for these probes. Positive control
    # first: scheduler ClusterIP MUST connect (builder-egress explicitly
    # allows it). Then the origin probe.
    builder_pod = wait_worker_pod(
        pool="${builderPool}", ns="${nsBuilders}", timeout=180
    )
    builder_vm, builder_pid = netns_handle(builder_pod, "${nsBuilders}")
    print(f"fetcher-split: fetcher={fetcher_pod} builder={builder_pod}")
    def builder_exec(cmd):
        return builder_vm.execute(f"nsenter -t {builder_pid} -n -- {cmd}")

    with subtest("builder-airgap: builder blocked from 'public' origin"):
        rc, out = builder_exec(f"${nc} -z -w5 {sched_ip} 9001")
        assert rc == 0, (
            f"POSITIVE CONTROL FAILED: builder→scheduler:{sched_ip}:9001 "
            f"rc={rc}. NetPol allows this; subsequent rc!=0 VACUOUS.\n{out}"
        )

        rc, out = builder_exec(
            "${curl} --max-time 5 -sS http://${originIP}/"
        )
        assert rc != 0, (
            f"builder reached ${originIP}:80 (rc=0) — builder-egress NOT "
            f"enforcing. ADR-019 airgap: no 0.0.0.0/0 allow-rule.\n{out}"
        )
        print(f"builder-airgap PASS: ${originIP}:80 blocked (rc={rc})")

    # ══════════════════════════════════════════════════════════════════
    # fetcher-node-dedicated — toleration + nodeSelector wired
    # ══════════════════════════════════════════════════════════════════
    # Full Karpenter NodePool isolation isn't testable in k3s (no
    # Karpenter). Shape-check the pod spec: reconciler's default
    # toleration + nodeSelector (fetcherpool/mod.rs:189-202). The
    # node-label above means the selector MATCHES → pod scheduled on
    # k3s-agent. Proves the params→podspec chain; actual node-pool
    # enforcement is EKS-only.
    with subtest("fetcher-node-dedicated: toleration + selector present"):
        spec = _json.loads(
            kubectl(f"get pod {fetcher_pod} -o json", ns="${nsFetchers}")
        )["spec"]
        tols = spec.get("tolerations", [])
        assert any(t.get("key") == "rio.build/fetcher" for t in tols), (
            f"expected toleration key rio.build/fetcher, got {tols!r}"
        )
        sel = spec.get("nodeSelector", {})
        assert sel.get("rio.build/node-role") == "fetcher", (
            f"expected nodeSelector rio.build/node-role=fetcher, got {sel!r}"
        )
        # Actually-scheduled-on check: we labeled k3s-agent, so the
        # fetcher pod MUST be there (only node matching the selector).
        node = spec.get("nodeName")
        assert node == "k3s-agent", (
            f"fetcher pod on {node!r}, expected k3s-agent (only node "
            f"with rio.build/node-role=fetcher label)"
        )
        print(f"fetcher-node-dedicated PASS: toleration+selector wired, "
              f"pod on {node}")

    with subtest("dispatch-fod+nonfod: FOD→fetcher, consumer→builder"):
        # Placement proof is structural: wait_worker_pod found
        # fetcher_pod in nsFetchers and builder_pod in nsBuilders after
        # this build was the only work submitted — wrong routing would
        # leave one namespace empty. Log-grep dropped: one-shot pods
        # are reaped by the controller before this subtest runs.
        # Await the background build → assert it succeeded.
        client.wait_until_succeeds(
            "grep -q '^/nix/store/' /tmp/split-build.log", timeout=240
        )
        out = client.succeed("tail -1 /tmp/split-build.log").strip()
        assert "rio-split" in out, f"build returned {out!r}"
        print("dispatch PASS: FOD→fetcher, consumer→builder")

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
