# ADR-019 builder/fetcher split end-to-end.
#
# FIRST test exercising both pool kinds in one fixture. Proves the full
# chain: FOD → kind=Fetcher pod, non-FOD → kind=Builder pod, builder airgap
# holds, fetcher egress open but IMDS-blocked.
#
# Fixture gotchas — fetcher pods have NO privileged escape hatch (hard-
# coded false at reconcilers/pool/mod.rs:237) AND a hard-coded
# Localhost seccomp (operator/rio-fetcher.json, :241). To get a RUNNING
# fetcher pod in the airgap VM:
#   - seccomp profile pre-installed on both nodes via systemd-tmpfiles
#     (k3sBase in fixtures/k3s-full.nix — same delivery as the NixOS AMI)
#   - nonpriv path enabled (vmtest-full-nonpriv.yaml overlay) — same as
#     vm-security-nonpriv-k3s; /dev/fuse via k3s containerd
#     base_runtime_spec (fixtures/k3s-full.nix containerdConfigTemplate)
#   - k3s-agent labeled rio.build/node-role=fetcher at runtime so the
#     reconciler's default nodeSelector matches (also exercises
#     fetcher.node.dedicated — fetcher lands on agent, builder on server)
#
# Egress-open proof — fetcher-egress allows toEntities:[world] on 80/443.
# In an airgap VM there's no real public IP. The scenario uses the
# fixture's `upstream-v4` node (v4-only http.server) reached from v6-only
# pods via DNS64+NAT64 — exactly the prod path. The 64:ff9b::/96
# synthesised address is outside any cluster identity → Cilium classifies
# it as `world`. Then:
#   builder netns → curl [64:ff9b::<v4>]:80 → rc≠0 (no world rule)
#   fetcher netns → curl [64:ff9b::<v4>]:80 → rc==0 (world:80 allow fires)
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

  # Ephemeral workers: pod names are Job-generated, not STS ordinals.
  # Resolved at runtime via wait_worker_pod() after the build is
  # queued. Pool name matches the extraValuesFiles overlay in
  # default.nix (vm-fetcher-split-k3s); builder pool is the
  # wait_worker_pod default ("x86-64").
  fetcherPool = "x86-64-fetcher";
in
pkgs.testers.runNixOSTest {
  name = "rio-fetcher-split";
  skipTypeCheck = true;

  # k3s bring-up ~4min + fetcher pod ~30s + build ~60s + 6 probe
  # subtests ~30s + fod-dir/fod-fail ~90s.
  globalTimeout = 1100 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.mkBootstrap {
      inherit fixture;
      withSeed = true;
    }}

    import time
    import json as _json

    # ══════════════════════════════════════════════════════════════════
    # FIXTURE PREP — node labels + "public" origin
    # ══════════════════════════════════════════════════════════════════

    # ── Seccomp profiles already on both nodes via tmpfiles ───────────
    # Fetcher reconciler hard-codes Localhost operator/rio-fetcher.json
    # (reconcilers/pool/mod.rs). k3sBase (fixtures/k3s-full.nix) writes both
    # profiles via systemd-tmpfiles before k3s starts — same delivery
    # as the NixOS AMI. Assert presence so a fixture regression surfaces
    # here and not as a CreateContainerError 60s downstream.
    for n in [k3s_server, k3s_agent]:
        n.succeed(
            "test -s /var/lib/kubelet/seccomp/operator/rio-fetcher.json && "
            "test -s /var/lib/kubelet/seccomp/operator/rio-builder.json"
        )

    # ── Label k3s-agent as the dedicated fetcher node ─────────────────
    # Reconciler defaults nodeSelector rio.build/node-role=fetcher
    # (reconcilers/pool/mod.rs Fetcher arm). Without a matching node, the pod
    # stays Pending forever. Labeling ONE node also makes the
    # fetcher-node-dedicated subtest meaningful: fetcher lands on
    # agent, builder (no nodeSelector in vmtest-full.yaml) lands
    # wherever the scheduler puts it — builder CAN land on agent
    # too. Shape-check the toleration instead of asserting different
    # nodes.
    kubectl("label node k3s-agent rio.build/node-role=fetcher --overwrite", ns="kube-system")

    # ── "public" origin on upstream-v4:80 ─────────────────────────────
    # The v6-only fixture's upstream-v4 node is the prod-path egress
    # target: pod resolves `upstream-v4` via CoreDNS dns64 →
    # 64:ff9b::<v4> AAAA → host's 64:ff9b::/96 route → edge Jool →
    # v4. The synthesised prefix is outside any Cilium cluster
    # identity → classified `world` → matches fetcher-egress's
    # world:80 allow. mkUpstreamNode runs http.server on :8080 only;
    # fetcher-egress allows world:{80,443} only — replace it with the
    # slow server on :80 here. Firewall port 80 opened at runtime
    # (NixOS firewall config is eval-time only); upstream-v4 is
    # v4-only so iptables (not ip6tables) is correct — Jool delivers
    # post-translation v4 packets.
    upstream_v4.succeed(
        "systemctl stop upstream-http.service && "
        "iptables -I nixos-fw -p tcp --dport 80 -j ACCEPT && "
        "mkdir -p /srv/sha256 && "
        "ln -sf ${drvs.coldBootstrapBusybox} /srv/busybox && "
        "echo ok > /srv/ok && "
        "printf 'rio-hashed-mirror-probe\\n' > /srv/sha256/${drvs.hashedMirrorProbeHex}"
    )
    # /busybox is delayed 30s so the one-shot fetcher pod stays Running
    # long enough for the netns probe below. /ok and / serve immediately
    # (the netpol probes use those, not /busybox). systemd-run detaches
    # cleanly — nohup+& can leave the driver's pipe open → succeed() hangs.
    # Bind on the empty host (all families); upstream-v4's eth1 is v4-only.
    upstream_v4.succeed(
        "cat >/srv/slow.py <<'PY'\n"
        "import http.server, time, os\n"
        "os.chdir('/srv')\n"
        "class H(http.server.SimpleHTTPRequestHandler):\n"
        "    def do_GET(self):\n"
        "        if self.path == '/busybox': time.sleep(30)\n"
        "        super().do_GET()\n"
        "http.server.ThreadingHTTPServer((''', 80), H).serve_forever()\n"
        "PY"
    )
    upstream_v4.succeed(
        "systemd-run --unit=test-origin ${py3} /srv/slow.py"
    )
    upstream_v4.wait_for_open_port(80)

    # nsenter -n keeps host mountns → host /etc/resolv.conf → no CoreDNS
    # dns64. Compute the RFC 6052 v4-embedded literal here so the netns
    # probes below stay raw-IP (matches the existing nsenter pattern;
    # same approach as ingress-v4v6.nix). The FOD URLs use the hostname
    # — those run inside the pod with pod DNS.
    upstream_v4_ip = upstream_v4.succeed(
        "ip -4 -o addr show dev eth1 | "
        "${pkgs.gawk}/bin/awk '{split($4,a,\"/\"); print a[1]}'"
    ).strip()
    origin_v6 = f"64:ff9b::{upstream_v4_ip}"
    print(f"upstream-v4 = {upstream_v4_ip}, NAT64 literal = {origin_v6}")

    # ── NetworkPolicies rendered + applied ────────────────────────────
    # networkPolicy.enabled=true in extraValues → helm-render puts
    # builder-egress + fetcher-egress in 02-workloads.yaml → k3s
    # auto-applied at boot. D3a: cluster-scoped CCNP (not namespaced
    # CNP) so a Pool{kind=Builder} in ANY namespace is selected. If
    # either get fails, the override didn't take or the template
    # if-gate is miswired.
    k3s_server.succeed("k3s kubectl get ccnp builder-egress -o name")
    k3s_server.succeed("k3s kubectl get ccnp fetcher-egress -o name")
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
        "--argstr url http://upstream-v4/busybox "
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
            "k3s kubectl -n ${nsFetchers} get pool,job,pod -o wide 2>&1; "
            "k3s kubectl -n ${nsFetchers} describe pool 2>&1; "
            "k3s kubectl -n rio-system logs deploy/rio-controller --tail=80 2>&1"
        )[1])
        raise
    # Resolve fetcher netns NOW and run all fetcher_exec probes BEFORE
    # waiting for the builder. One-shot pod lives only as long as the
    # FOD (held to ~30s by the slow /busybox handler); the builder pod
    # won't exist until the FOD completes, so waiting for it first
    # guarantees the fetcher is already gone.
    fetcher_vm, fetcher_pid = netns_handle(fetcher_pod, "${nsFetchers}")
    # Snapshot pod spec NOW for fetcher-node-dedicated below — the
    # one-shot pod is reaped (ttlSecondsAfterFinished) before that
    # subtest runs.
    fetcher_pod_spec = _json.loads(
        kubectl(f"get pod {fetcher_pod} -o json", ns="${nsFetchers}")
    )["spec"]
    def fetcher_exec(cmd):
        return fetcher_vm.execute(f"nsenter -t {fetcher_pid} -n -- {cmd}")

    sched_ip = kubectl(
        "get svc rio-scheduler -o jsonpath='{.spec.clusterIP}'"
    ).strip()

    # ══════════════════════════════════════════════════════════════════
    # fetcher-egress — fetcher REACHES upstream-v4 via NAT64
    # ══════════════════════════════════════════════════════════════════
    # THE non-vacuous differentiator vs builder. Same origin, same
    # port — fetcher-egress's toEntities:[world]:80 allow fires (the
    # 64:ff9b::/96 prefix is outside any cluster identity → world);
    # builder-egress has no world rule.
    with subtest("fetcher-egress: fetcher reaches 'public' origin"):
        rc, out = fetcher_exec(f"${nc} -z -w5 {sched_ip} 9001")
        assert rc == 0, (
            f"POSITIVE CONTROL FAILED: fetcher→scheduler:{sched_ip}:9001 "
            f"rc={rc}. fetcher-egress allows this too.\n{out}"
        )

        rc, out = fetcher_exec(
            "${curl} --max-time 5 -sS -o /dev/null -w '%{http_code}' "
            f"'http://[{origin_v6}]/ok'"
        )
        assert rc == 0, (
            f"fetcher BLOCKED from [{origin_v6}]:80 (rc={rc}) — "
            f"fetcher-egress toEntities:[world]:80 allow NOT firing. "
            f"64:ff9b::/96 is outside cluster identities → world.\n{out}"
        )
        assert out.strip() == "200", f"origin returned {out!r}, expected 200"
        print(f"fetcher-egress PASS: [{origin_v6}]:80 reachable (http {out.strip()})")

    # ══════════════════════════════════════════════════════════════════
    # fetcher-imds-blocked — host entity not in world
    # ══════════════════════════════════════════════════════════════════
    # WEAK-in-VM (same caveat as netpol.nix): no IMDS listener in
    # QEMU. The fetcher-egress subtest above is the non-vacuous gate;
    # this proves the v6 IMDS endpoint (Cilium `host` entity, NOT
    # `world`) is denied.
    with subtest("fetcher-imds-blocked: fd00:ec2::254 blocked"):
        rc, _ = fetcher_exec(
            "${curl} --max-time 5 -sS 'http://[fd00:ec2::254]/latest/meta-data/'"
        )
        assert rc != 0, (
            "fetcher reached IMDS (rc=0) — fetcher-egress world rule "
            "should NOT match the host entity (fd00:ec2::254)."
        )
        print(f"fetcher-imds PASS: blocked (rc={rc})")

    # ══════════════════════════════════════════════════════════════════
    # builder-airgap — builder BLOCKED from upstream-v4 via NAT64
    # ══════════════════════════════════════════════════════════════════
    # Builder pod appears once the FOD completes; sleepSecs=30 in the
    # consumer drv keeps it alive for these probes. Positive control
    # first: scheduler ClusterIP MUST connect (builder-egress explicitly
    # allows it). Then the origin probe.
    builder_pod = wait_worker_pod()
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
            f"${curl} --max-time 5 -sS 'http://[{origin_v6}]/'"
        )
        assert rc != 0, (
            f"builder reached [{origin_v6}]:80 (rc=0) — builder-egress NOT "
            f"enforcing. ADR-019 airgap: no world allow-rule.\n{out}"
        )
        print(f"builder-airgap PASS: [{origin_v6}]:80 blocked (rc={rc})")

    # ══════════════════════════════════════════════════════════════════
    # fetcher-node-dedicated — toleration + nodeSelector wired
    # ══════════════════════════════════════════════════════════════════
    # Full Karpenter NodePool isolation isn't testable in k3s (no
    # Karpenter). Shape-check the pod spec: reconciler's default
    # toleration + nodeSelector (reconcilers/pool/mod.rs Fetcher arm). The
    # node-label above means the selector MATCHES → pod scheduled on
    # k3s-agent. Proves the params→podspec chain; actual node-pool
    # enforcement is EKS-only.
    with subtest("fetcher-node-dedicated: toleration + selector present"):
        spec = fetcher_pod_spec
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

    # ══════════════════════════════════════════════════════════════════
    # fod-dead-origin — hashed-mirrors fallback for flat-hash FODs
    # ══════════════════════════════════════════════════════════════════
    # Origin URL is a 404 path on upstream-v4; the ONLY way this build
    # succeeds is via {mirror}/sha256/{hex}. CppNix builtin:fetchurl
    # tries hashed-mirrors first for FileIngestionMethod::Flat, then
    # falls back to mainUrl. nixConf.hashedMirrors = http://upstream-v4/
    # via extraValues (default.nix) → rio-nix-conf ConfigMap → fetcher
    # pod's nix.conf. A regression (typo'd setting, ConfigMap not
    # mounted, wrong URL format) → mirror not tried → origin 404 →
    # build fails here.
    with subtest("fod-dead-origin: flat FOD succeeds via hashed-mirrors"):
        rc, out = client.execute(
            "timeout 180 nix-build --no-out-link --store ssh-ng://k3s-server "
            "${drvs.fodDeadOrigin} 2>&1"
        )
        if rc != 0:
            print(upstream_v4.execute(
                "journalctl -u test-origin --no-pager -n 40 2>&1"
            )[1])
            raise AssertionError(
                f"fod-dead-origin build failed (rc={rc}); origin URL is a "
                f"deliberate 404 — success requires hashed-mirrors lookup "
                f"at /sha256/${drvs.hashedMirrorProbeHex}.\n{out}"
            )
        assert "rio-mirror-probe" in out, f"unexpected output {out!r}"
        print(f"fod-dead-origin PASS: {out.strip().splitlines()[-1]}")

    # ══════════════════════════════════════════════════════════════════
    # fod-dir — recursive-hash FOD with directory output
    # ══════════════════════════════════════════════════════════════════
    # The build runs `mkdir $out`. busybox FOD is already cached (the
    # dispatch subtest above built it), so the only fresh build is the
    # dir-output FOD itself — routes to a fetcher pod.
    with subtest("fod-dir: recursive-hash FOD with directory output"):
        rc, out = client.execute(
            "timeout 180 nix-build --no-out-link --store ssh-ng://k3s-server "
            "${drvs.fodDir} 2>&1"
        )
        if rc != 0:
            raise AssertionError(
                f"fod-dir build failed (rc={rc}). 'Input/output error' on "
                f"mkdir means a whiteout was placed at the output path "
                f"(prepare_sandbox regression).\n{out}"
            )
        assert "rio-fod-dir" in out, f"unexpected output {out!r}"
        print(f"fod-dir PASS: {out.strip().splitlines()[-1]}")

    # ══════════════════════════════════════════════════════════════════
    # fod-fail — failing FOD propagates without FUSE-lookup hang (P0308)
    # ══════════════════════════════════════════════════════════════════
    # Origin 404s, hashed-mirror has no entry → builtin:fetchurl exits
    # nonzero with $out absent. nix-daemon's post-build deletePath($out)
    # stat falls through to FUSE lower; JitClass::NotInput → ENOENT
    # without store contact. Asserting elapsed bounds the hang the
    # whiteout once papered over: a regression here means lookup() fell
    # through to gRPC and blocked.
    with subtest("fod-fail: failing FOD propagates without hang"):
        t0 = time.monotonic()
        rc, out = client.execute(
            "timeout 90 nix-build --no-out-link --store ssh-ng://k3s-server "
            "${drvs.fodFail} 2>&1"
        )
        elapsed = time.monotonic() - t0
        assert rc != 0, (
            f"fod-fail unexpectedly SUCCEEDED — origin /nonexistent 404s "
            f"and hashed-mirror has no entry for its hash:\n{out}"
        )
        assert elapsed < 60, (
            f"fod-fail took {elapsed:.1f}s (>60s) — daemon post-fail "
            f"stat($out) likely blocked in FUSE lookup (JitClass::NotInput "
            f"fast-path not firing).\n{out}"
        )
        print(f"fod-fail PASS: rc={rc} in {elapsed:.1f}s")

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
