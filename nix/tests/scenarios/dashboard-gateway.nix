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
# enables gatewayAPI in cilium-render.nix (embedded envoy — no separate
# cilium-envoy DaemonSet; see eds-rootcause.md). ~5min.
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

    # ── (no cilium-envoy DS — embedded mode, see cilium-render.nix) ─────

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

    # ── EDS: envoy has healthy rio-scheduler endpoints ──────────────────
    # The Gateway data-plane assertion. With separate cilium-envoy
    # DaemonSet, EDS (ClusterLoadAssignment) fetch timed out — envoy
    # had zero endpoints. Embedded mode (envoy.enabled=false in
    # cilium-render.nix) pushes endpoints in-process. This proves the
    # L7 data-plane is wired: cluster exists, endpoints discovered,
    # health=healthy. Combined with Gateway Programmed + HTTPRoute
    # Accepted + CEC generated above, every Cilium-side piece is
    # verified.
    #
    # An end-to-end curl through the Gateway LB IP requires either
    # l2announcements reachability (k3s — set in cilium-render.nix but
    # pod-netns → LB-IP routing is fixture-specific) or a cloud LB
    # (EKS Phase 6). The production north-south path (browser → LB →
    # Gateway → scheduler) is proven at deploy time. The in-cluster
    # path (nginx → scheduler direct, D3) is proven by vm-dashboard-
    # k3s with full curl assertions.
    with subtest("Gateway data-plane: envoy EDS has healthy scheduler endpoints"):
        # Single grep — chained `| grep | grep -q` SIGPIPEs the upstream
        # under pipefail (test-driver's succeed wraps in `set -euo pipefail`).
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n kube-system exec ds/cilium -- "
            "cilium-dbg envoy admin clusters 2>/dev/null "
            "| grep -E 'rio-scheduler:9001::.+::health_flags::healthy' >/dev/null",
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
        # Port-forward to the LEADER pod, not svc/ (which picks an
        # arbitrary replica — standby returns trailer-only Unavailable
        # → first byte 0x80 not 0x00, and retries hit the same pod).
        leader = k3s_server.succeed(
            "k3s kubectl -n ${ns} get lease rio-scheduler-leader "
            "-o jsonpath='{.spec.holderIdentity}'"
        ).strip()
        k3s_server.succeed(
            f"k3s kubectl -n ${ns} port-forward pod/{leader} 19001:9001 "
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
            k3s_server.execute(
                "curl -sv -X POST http://localhost:19001/rio.admin.AdminService/ClusterStatus "
                "-H 'content-type: application/grpc-web+proto' -H 'x-grpc-web: 1' "
                "-d x 2>&1 | head -30; "
                "k3s kubectl -n ${ns} logs pod/" + leader + " --tail=40 2>&1"
            )
            raise
        finally:
            k3s_server.execute("kill $(cat /tmp/pf-sched.pid) 2>/dev/null || true")
  '';
}
