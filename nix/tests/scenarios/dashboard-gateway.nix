# Cilium Gateway API → rio-scheduler (tonic-web) end-to-end.
#
# Proves the dashboard's gRPC-Web path works through Cilium's per-
# Gateway envoy. gRPC-Web translation is in-process at rio-scheduler
# (tonic-web layer, D3) — the Gateway is plain HTTP/2 routing. Two
# curl assertions (r[verify dash.envoy.grpc-web-translate] at
# default.nix:vm-dashboard-gateway-k3s):
#
#   1. Unary ClusterStatus: DATA frame starts 0x00 (compression flag 0)
#   2. Server-streaming GetBuildLogs: trailer frame has 0x80 byte
#
# The 0x80 byte is the load-bearing assertion — proves tonic-web emits
# trailers as a separate LENGTH-PREFIXED-MESSAGE with flag=0x80 per the
# gRPC-Web spec (browsers can't read HTTP/2 trailers).
#
# k3s-full fixture with gatewayEnabled=true: vendors Gateway API CRDs,
# enables cilium-envoy DaemonSet + gatewayAPI in cilium-render.nix,
# preloads quay.io/cilium/cilium-envoy. ~5min (bring-up ~4min + Cilium
# Gateway reconcile + curls).
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) ns;
in
pkgs.testers.runNixOSTest {
  name = "rio-dashboard-gateway";
  skipTypeCheck = true;

  # Bring-up ~4min + Cilium Gateway reconcile ~30s + curl <10s.
  globalTimeout = 900 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.mkBootstrap { inherit fixture; }}

    # ── cilium-envoy DaemonSet rollout ──────────────────────────────────
    # gatewayAPI.enabled=true → cilium-envoy DS deploys alongside
    # cilium-agent. Must be Ready before any Gateway reconciles.
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n kube-system rollout status ds/cilium-envoy "
        "--timeout=120s",
        timeout=150,
    )

    # ── Gateway Programmed ──────────────────────────────────────────────
    # cilium-operator reconciles GatewayClass → Gateway → per-Gateway
    # envoy Deployment + Service. Gateway.status.conditions[type=
    # Programmed] goes True when the envoy is serving. The Service is
    # `cilium-gateway-rio-dashboard` in ${ns}.
    with subtest("Gateway Programmed: cilium per-gateway envoy reconciled"):
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${ns} get gateway rio-dashboard "
            "-o jsonpath='{.status.conditions[?(@.type==\"Programmed\")].status}' "
            "| grep -qx True",
            timeout=120,
        )
        # HTTPRoute Accepted (proves parentRef resolved, backendRef valid).
        # HTTPRoute (not GRPCRoute): grpc-web content-type doesn't match
        # GRPCRoute; the scheduler does grpc-web translation in-process,
        # so the Gateway routes plain HTTP by exact path.
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${ns} get httproute rio-scheduler-readonly "
            "-o jsonpath='{.status.parents[0].conditions[?(@.type==\"Accepted\")].status}' "
            "| grep -qx True",
            timeout=90,
        )
        # Mutating route MUST NOT exist — enableMutatingMethods defaults
        # false. Any accidental render = fail-open on ClearPoison/
        # DrainExecutor/CreateTenant/TriggerGC.
        k3s_server.fail(
            "k3s kubectl -n ${ns} get httproute rio-scheduler-mutating"
        )

    # ── CiliumEnvoyConfig generated ─────────────────────────────────────
    # cilium-operator translates Gateway+GRPCRoute → a CiliumEnvoyConfig
    # CR (the xDS source for cilium-envoy). Its existence proves the
    # full reconcile chain: GatewayClass accepted → Gateway programmed
    # → GRPCRoute attached → CEC pushed to envoy.
    with subtest("CiliumEnvoyConfig generated for gateway"):
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${ns} get ciliumenvoyconfig "
            "-l gateway.networking.k8s.io/gateway-name=rio-dashboard "
            "-o name | grep -q .",
            timeout=60,
        )

    # ── D3: gRPC-Web direct to scheduler (no Gateway) ───────────────────
    # Proves the scheduler serves gRPC-Web NATIVELY (tonic-web layer),
    # independent of any proxy. Scheduler is plaintext on 9001 — plain
    # port-forward + http://localhost curl. accept_http1(true) means
    # the h1 gRPC-Web POST works alongside native h2 gRPC on one port.
    # Runs FIRST: if tonic-web is broken, the Gateway tests can't pass
    # either — fail here with the simpler reproduction.
    with subtest("gRPC-Web direct: scheduler tonic-web ClusterStatus"):
        k3s_server.succeed(
            "k3s kubectl -n ${ns} port-forward svc/rio-scheduler 19001:9001 "
            ">/tmp/pf-sched.log 2>&1 & echo $! > /tmp/pf-sched.pid"
        )
        k3s_server.wait_until_succeeds(
            "${pkgs.netcat}/bin/nc -z localhost 19001", timeout=10
        )
        try:
            k3s_server.wait_until_succeeds(
                "printf '\\x00\\x00\\x00\\x00\\x00' | "
                "curl -sf -X POST http://localhost:19001/rio.admin.AdminService/ClusterStatus "
                "-H 'content-type: application/grpc-web+proto' "
                "-H 'x-grpc-web: 1' --data-binary @- "
                "| ${pkgs.xxd}/bin/xxd | head -1 | grep -q '^00000000: 00'",
                timeout=60,
            )
        except Exception:
            print("── DIAGNOSTIC: scheduler tonic-web direct ──")
            print(k3s_server.succeed(
                "curl -sv -X POST http://localhost:19001/rio.admin.AdminService/ClusterStatus "
                "-H 'content-type: application/grpc-web+proto' -H 'x-grpc-web: 1' "
                "--data-raw "" 2>&1 | head -30; "
                "k3s kubectl -n ${ns} logs deploy/rio-scheduler --tail=40 2>&1"
            ))
            raise
        finally:
            k3s_server.execute("kill $(cat /tmp/pf-sched.pid) 2>/dev/null || true")

    # ── gRPC-Web through Cilium Gateway (from-pod) ──────────────────────
    # Cilium's per-Gateway Service is selectorless — the eBPF L7
    # redirect to cilium-envoy fires for pod-netns traffic only, not
    # host-netns. So wget from inside the rio-dashboard nginx pod
    # (alpine, has busybox wget+od). Same-ns → short-name DNS works.
    # busybox wget needs --post-file (no --data-binary); write the
    # 5-byte grpc-web header to a tmpfile first.
    gw_url = "http://cilium-gateway-rio-dashboard:8080"

    def pod_wget(script):
        # Heredoc-via-stdin avoids the nested-quote escaping of
        # `sh -c '...'` inside Python inside Nix.
        return (
            "k3s kubectl -n ${ns} exec -i deploy/rio-dashboard -c nginx -- sh "
            f"<<'PODEOF'\n{script}\nPODEOF"
        )

    with subtest("gRPC-Web unary via Gateway: ClusterStatus DATA frame 0x00"):
        # Empty proto body = 5-byte header (compression flag + 4-byte
        # length, all zero). Response DATA frame starts with same 0x00.
        # wait_until_succeeds: HTTPRoute backendRef load-balances across
        # scheduler replicas; a non-leader hit yields Unavailable.
        try:
            k3s_server.wait_until_succeeds(
                pod_wget(
                    "printf '\\000\\000\\000\\000\\000' >/tmp/b && "
                    "wget -qO- --header='content-type: application/grpc-web+proto' "
                    "--header='x-grpc-web: 1' --post-file=/tmp/b "
                    f"{gw_url}/rio.admin.AdminService/ClusterStatus "
                    "| od -An -tx1 -N1 | grep -qw 00"
                ),
                timeout=90,
            )
        except Exception:
            print("── DIAGNOSTIC: from-pod wget via Gateway ──")
            print(k3s_server.succeed(pod_wget(
                "wget -SO- --header='content-type: application/grpc-web+proto' "
                "--header='x-grpc-web: 1' --post-data=x "
                f"{gw_url}/rio.admin.AdminService/ClusterStatus 2>&1 | head -30 || true"
            )))
            print(k3s_server.succeed(
                "k3s kubectl -n ${ns} get svc cilium-gateway-rio-dashboard -o wide 2>&1; "
                "k3s kubectl -n ${ns} get httproute rio-scheduler-readonly -o yaml 2>&1 | tail -30"
            ))
            raise

    with subtest("gRPC-Web streaming via Gateway: GetBuildLogs trailer 0x80"):
        # GetBuildLogs{derivation_path:"nonexist"} (field 2, type 2,
        # len 8) → handler returns Ok(stream-yielding-Err) so tonic-web
        # emits the trailer as a 0x80-flagged length-prefixed-message.
        k3s_server.wait_until_succeeds(
            pod_wget(
                "printf '\\000\\000\\000\\000\\012\\022\\010nonexist' >/tmp/b && "
                "wget -qO- --header='content-type: application/grpc-web+proto' "
                "--header='x-grpc-web: 1' --post-file=/tmp/b "
                f"{gw_url}/rio.admin.AdminService/GetBuildLogs "
                "| od -An -tx1 -N1 | grep -qw 80"
            ),
            timeout=90,
        )

    # ── r[dash.auth.method-gate] runtime: ClearPoison unrouted ──────────
    # enableMutatingMethods=false → mutating HTTPRoute absent → Cilium
    # Gateway has no route for /ClearPoison → 404. Live proof the
    # method-gate holds at the data plane (helm-lint proves it
    # statically; this proves the operator's xDS respects it).
    # busybox wget exits nonzero on 4xx → wrap in `! wget` for the 404.
    with subtest("method-gate via Gateway: ClearPoison 404"):
        k3s_server.succeed(pod_wget(
            "! wget -qO- --post-data=x "
            f"{gw_url}/rio.admin.AdminService/ClearPoison 2>&1 "
            "| grep -q ."
        ))
  '';
}
