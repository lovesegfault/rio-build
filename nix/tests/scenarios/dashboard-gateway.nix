# Envoy Gateway gRPC-Web → gRPC+mTLS end-to-end.
#
# Wave 0a of the dashboard track: proves the grpc_web filter works
# through operator-managed Envoy BEFORE a single line of Svelte exists.
# Two curl assertions (r[verify dash.envoy.grpc-web-translate] at
# default.nix:vm-dashboard-gateway-k3s):
#
#   1. Unary ClusterStatus: DATA frame starts 0x00 (compression flag 0)
#   2. Server-streaming GetBuildLogs: trailer frame has 0x80 byte
#
# The 0x80 byte is the load-bearing assertion — it proves the grpc_web
# filter doesn't buffer the stream (Envoy's filter emits trailers as a
# separate LENGTH-PREFIXED-MESSAGE with flag=0x80, unlike HTTP/2 which
# puts grpc-status in actual trailers the browser fetch API can't
# read). If the stream buffered, the test would timeout or the trailer
# wouldn't have the 0x80 marker at the end.
#
# k3s-full fixture with envoyGatewayEnabled=true: preloads
# envoyproxy/gateway + envoyproxy/envoy images, applies gateway-helm
# CRDs+operator as k3s manifests, renders the rio chart's dashboard-
# gateway*.yaml CRs. ~5-6min (bring-up ~4min + ~60s operator
# reconcile + curls).
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) ns;

  # envoy data-plane (Deployment + Service) lives in the OPERATOR's
  # namespace by default (ControllerNamespaceMode, the upstream
  # default). Not ${ns} — the Gateway/GRPCRoute/EnvoyProxy CRs live
  # in rio-system but the infra they produce lands here. The
  # backendTLS.clientCertificateRef Secret lives in rio-system (same
  # ns as the EnvoyProxy CR) and the operator syncs it cross-ns.
  egNs = "envoy-gateway-system";

  covTimeoutHeadroom = if common.coverage then 300 else 0;
in
pkgs.testers.runNixOSTest {
  name = "rio-dashboard-gateway";

  # Bring-up ~4min + operator reconcile ~60s + certgen Job + envoy
  # data-plane pod schedule/start ~30s + curl <10s. 900s comfortable.
  globalTimeout = 900 + covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.kvmCheck}
    ${common.assertions}

    start_all()
    ${fixture.waitReady}
    ${fixture.kubectlHelpers}
    ${fixture.sshKeySetup}

    # ── Envoy Gateway operator Available ────────────────────────────────
    # The certgen Job runs first (generates the operator's xDS mTLS
    # certs into a Secret); the Deployment mounts that Secret and
    # won't be Ready until certgen completes. ~20-30s on a warm KVM
    # builder. The operator lives in its own namespace (not rio-system).
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n envoy-gateway-system wait --for=condition=Available "
        "deploy/envoy-gateway --timeout=120s",
        timeout=150,
    )

    # ── Gateway Programmed ──────────────────────────────────────────────
    # Operator reconciles GatewayClass → Gateway → envoy Deployment +
    # Service. Gateway.status.conditions[type=Programmed] goes True
    # when the envoy data-plane pods are serving xDS. EnvoyProxy.spec.
    # provider.kubernetes.envoyService.name pins the Service name so
    # we don't have to label-select.
    with subtest("Gateway Programmed: envoy data-plane reconciled"):
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${ns} get gateway rio-dashboard "
            "-o jsonpath='{.status.conditions[?(@.type==\"Programmed\")].status}' "
            "| grep -qx True",
            timeout=120,
        )
        # GRPCRoute Accepted (proves parentRef resolved, backendRef
        # valid). BackendTLSPolicy + SecurityPolicy don't have a
        # simple top-level condition — status surfaces via
        # ancestorStatus; the curl below is the real proof.
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${ns} get grpcroute rio-scheduler "
            "-o jsonpath='{.status.parents[0].conditions[?(@.type==\"Accepted\")].status}' "
            "| grep -qx True",
            timeout=30,
        )

    # ── envoy Service endpoints ready ───────────────────────────────────
    # Service name is stable (EnvoyProxy.spec.provider.kubernetes.
    # envoyService.name = rio-dashboard-envoy). The operator creates
    # it in ${egNs} (ControllerNamespaceMode — the upstream default)
    # NOT in ${ns}. Wait for at least one endpoint before curling —
    # envoy takes a few seconds after Programmed to be serving.
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${egNs} get endpoints rio-dashboard-envoy "
        "-o jsonpath='{.subsets[0].addresses[0].ip}' | grep -q .",
        timeout=60,
    )

    # ── Filter-chain verify ─────────────────────────────────────────────
    # egctl isn't in the VM (would need another image); hit the envoy
    # admin /config_dump via apiserver pods/proxy (distroless envoy
    # has no shell → can't kubectl exec curl). Port 19000 is the
    # envoy admin port (bootstrap.go EnvoyAdminPort constant).
    # The grpc_web filter name is "envoy.filters.http.grpc_web" —
    # present iff the GRPCRoute's IsHTTP2 trigger fired
    # (listener.go:424-425).
    with subtest("grpc_web filter present: auto-inject via GRPCRoute"):
        pod = k3s_server.succeed(
            "k3s kubectl -n ${egNs} get pod "
            "-l gateway.envoyproxy.io/owning-gateway-name=rio-dashboard "
            "-o jsonpath='{.items[0].metadata.name}'"
        ).strip()
        # NUMERIC port (19000), not named — k3s apiserver panics on
        # named-port resolution failure (same gotcha as waitReady's
        # leader-metrics scrape, k3s-full.nix:~500).
        k3s_server.succeed(
            "k3s kubectl get --raw "
            f"'/api/v1/namespaces/${egNs}/pods/{pod}:19000/proxy/config_dump' "
            "| grep -q envoy.filters.http.grpc_web"
        )

    # ── curl gate: unary + trailer-frame ────────────────────────────────
    # Port-forward to the envoy Service (cluster DNS isn't resolvable
    # from the host netns — the test script runs on k3s_server which
    # is OUTSIDE the pod network). Same pattern as cli.nix's scheduler
    # port-forward.
    k3s_server.succeed(
        "k3s kubectl -n ${egNs} port-forward svc/rio-dashboard-envoy 18080:8080 "
        ">/tmp/pf-envoy.log 2>&1 & echo $! > /tmp/pf-envoy.pid"
    )
    k3s_server.wait_until_succeeds(
        "${pkgs.netcat}/bin/nc -z localhost 18080", timeout=10
    )

    # ── R1 de-risk: unary ClusterStatus via gRPC-Web ────────────────────
    # Empty proto body = 5-byte header (1 byte compression flag + 4
    # bytes big-endian length = 0x00,0x00,0x00,0x00,0x00). Response
    # starts with the same 5-byte DATA frame header (compression=0).
    #
    # AdminService is served on the leader scheduler; the GRPCRoute
    # backendRef points at the rio-scheduler ClusterIP Service, which
    # load-balances across replicas. A non-leader hit returns
    # Unavailable — wrap in wait_until_succeeds with a short timeout
    # (~5 retries covers the 50% hit rate).
    with subtest("gRPC-Web unary: ClusterStatus DATA frame 0x00 prefix"):
        k3s_server.wait_until_succeeds(
            "printf '\\x00\\x00\\x00\\x00\\x00' | "
            "curl -sf -X POST http://localhost:18080/rio.admin.AdminService/ClusterStatus "
            "-H 'content-type: application/grpc-web+proto' "
            "-H 'x-grpc-web: 1' "
            "--data-binary @- "
            "| ${pkgs.xxd}/bin/xxd | head -1 | grep -q '^00000000: 00'",
            timeout=30,
        )

    # ── R3 de-risk: server-streaming trailer-frame 0x80 ─────────────────
    # GetBuildLogs with a nonexistent drv_path → server sends zero
    # log lines but MUST send the trailer frame (grpc-status: 5
    # NotFound) as a length-prefixed message with flag 0x80.
    # Request body = GetBuildLogsRequest{drv_path:"nonexist"} =
    # 0x0a (field 1, type 2) + 0x08 (len 8) + "nonexist" = 10 bytes
    # message → 5-byte header 0x00,0x00,0x00,0x00,0x0a + 10 bytes.
    #
    # The 0x80 byte is the single most important assertion in Wave 0
    # — proves server-streaming through envoy-gateway BEFORE a single
    # line of TypeScript exists. If the grpc_web filter buffered, the
    # curl would either timeout or the trailer would be HTTP chunked
    # without the 0x80 frame marker.
    with subtest("gRPC-Web streaming: GetBuildLogs trailer 0x80 byte"):
        k3s_server.wait_until_succeeds(
            "printf '\\x00\\x00\\x00\\x00\\x0a\\x0a\\x08nonexist' | "
            "curl -sf -X POST http://localhost:18080/rio.admin.AdminService/GetBuildLogs "
            "-H 'content-type: application/grpc-web+proto' "
            "-H 'x-grpc-web: 1' "
            "--data-binary @- "
            "| ${pkgs.xxd}/bin/xxd | grep -q ' 80'",
            timeout=30,
        )

    k3s_server.execute("kill $(cat /tmp/pf-envoy.pid) 2>/dev/null || true")

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
