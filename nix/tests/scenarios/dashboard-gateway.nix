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
in
pkgs.testers.runNixOSTest {
  name = "rio-dashboard-gateway";
  skipTypeCheck = true;

  # Bring-up ~4min + operator reconcile ~60s + certgen Job + envoy
  # data-plane pod schedule/start ~30s + curl <10s. 900s comfortable.
  globalTimeout = 900 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

    ${common.kvmPreopen}
    start_all()
    ${fixture.waitReady}
    ${fixture.kubectlHelpers}
    ${fixture.sshKeySetup}

    # ── certgen Job complete (flannel-race gate) ────────────────────────
    # k3s auto-applies manifests at startup (services.k3s.manifests),
    # INDEPENDENTLY of this testScript — the certgen Job is created as
    # soon as k3s's deploy controller processes 02-envoy-gateway.yaml.
    # Under KVM this happens BEFORE flannel has written
    # /run/flannel/subnet.env on the server node (waitReady only gates
    # on server-EXISTS, not server-Ready). Observed failure chain:
    #
    #   certgen pod: CreatePodSandboxError ... flannel failed (add):
    #     loadFlannelSubnetEnv failed: open /run/flannel/subnet.env:
    #     no such file or directory
    #   → Secret "envoy-gateway" never created
    #   → controller pod: MountVolume.SetUp failed ... secret
    #     "envoy-gateway" not found (exponential backoff 1s→8s)
    #   → controller starts late, xDS push delayed past config_dump poll
    #
    # kubelet retries sandbox creation; once flannel is up the certgen
    # pod schedules and the Job completes. Waiting for condition=complete
    # here means the Secret EXISTS before we wait on the controller
    # Deployment — the controller's first (or next-backoff) mount
    # attempt succeeds instead of compounding into an 8s backoff chain.
    # wait_until_succeeds retries the outer kubectl-wait in case the
    # Job object itself hasn't been applied yet.
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${egNs} wait --for=condition=complete "
        "job/envoy-gateway-gateway-helm-certgen --timeout=120s",
        timeout=150,
    )

    # ── Envoy Gateway operator Available ────────────────────────────────
    # certgen above guarantees the Secret exists; the Deployment mounts
    # it and comes Ready without the secret-not-found backoff. ~10-20s
    # on a warm KVM builder. Operator lives in its own namespace.
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${egNs} wait --for=condition=Available "
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
            "k3s kubectl -n ${ns} get grpcroute rio-scheduler-readonly "
            "-o jsonpath='{.status.parents[0].conditions[?(@.type==\"Accepted\")].status}' "
            "| grep -qx True",
            timeout=90,
        )
        # Mutating route MUST NOT exist — enableMutatingMethods defaults
        # false. The k3s fixture doesn't set it (fixture sets
        # dashboard.enabled=true only). Any accidental render =
        # fail-open on ClearPoison/DrainWorker/CreateTenant/TriggerGC.
        k3s_server.fail(
            "k3s kubectl -n ${ns} get grpcroute rio-scheduler-mutating"
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
        # NUMERIC port (19000), not named — k3s apiserver panics on
        # named-port resolution failure (same gotcha as waitReady's
        # leader-metrics scrape, k3s-full.nix:~500).
        # Field-selector phase=Running: a stale terminating pod can
        # linger; .items[0] without filter picks whichever sorts first.
        # Re-lookup each poll iteration (pod name changes on restart).
        # The certgen flannel-race is gated above (Job condition=
        # complete wait) so the controller starts without secret-mount
        # backoff — 60s here is a genuine xDS-push budget, not a
        # backoff-recovery budget. If grpc_web never appears, the
        # GRPCRoute wiring is broken.
        k3s_server.wait_until_succeeds(
            "pod=$(k3s kubectl -n ${egNs} get pod "
            "-l gateway.envoyproxy.io/owning-gateway-name=rio-dashboard "
            "--field-selector=status.phase=Running "
            "-o jsonpath='{.items[0].metadata.name}'); "
            "test -n \"$pod\" && "
            "k3s kubectl get --raw "
            "\"/api/v1/namespaces/${egNs}/pods/$pod:19000/proxy/config_dump\" "
            "2>&1 | grep -q grpc_web",
            timeout=60,
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
            timeout=90,
        )

    # ── R3 de-risk: server-streaming trailer-frame 0x80 ─────────────────
    # GetBuildLogs with a nonexistent derivation_path → server sends
    # zero log lines but MUST send the trailer frame (grpc-status:
    # NotFound or InvalidArgument) with flag 0x80.
    # Proto refactor at b643ab82 changed field 1 to build_id; field 2
    # is derivation_path. Request = GetBuildLogsRequest{derivation_
    # path:"nonexist"} = 0x12 (field 2, type 2) + 0x08 (len 8) +
    # "nonexist" = 10 bytes → header 0x00,0x00,0x00,0x00,0x0a.
    #
    # The 0x80 byte is the single most important assertion in Wave 0
    # — proves server-streaming through envoy-gateway BEFORE a single
    # line of TypeScript exists. If the grpc_web filter buffered, the
    # curl would either timeout or the trailer would be HTTP chunked
    # without the 0x80 frame marker.
    with subtest("gRPC-Web streaming: GetBuildLogs trailer 0x80 byte"):
        # Root cause of the 8+ earlier failures (sprint-save v10-v19):
        # tonic's `Err(Status)` return from a server-streaming handler
        # emits a Trailers-Only response — grpc-status in the HTTP
        # HEADERS, zero body. Envoy passes that through and curl sees
        # an empty body. Handler now returns Ok(stream-yielding-Err),
        # so tonic emits HEADERS→TRAILERS and Envoy encodes trailers
        # as a 0x80 body frame.
        #
        # wait_until_succeeds: GRPCRoute backendRef load-balances across
        # scheduler replicas; a non-leader hit yields Unavailable (also
        # a 0x80 trailer, but may race with leader election startup).
        k3s_server.wait_until_succeeds(
            "printf '\\x00\\x00\\x00\\x00\\x0a\\x12\\x08nonexist' | "
            "curl -sf -X POST http://localhost:18080/rio.admin.AdminService/GetBuildLogs "
            "-H 'content-type: application/grpc-web+proto' "
            "-H 'x-grpc-web: 1' "
            "--data-binary @- "
            "| ${pkgs.xxd}/bin/xxd | head -1 | grep -q '^00000000: 80'",
            timeout=90,
        )

    # ── r[dash.auth.method-gate] end-to-end: ClearPoison unrouted ───────
    # With enableMutatingMethods=false (the default; this fixture doesn't
    # override it), the mutating GRPCRoute is absent → envoy has no
    # route match for /rio.admin.AdminService/ClearPoison → 404.
    #
    # This is the LIVE proof that the helm-lint static assert (flake.nix
    # helm-lint "mutating GRPCRoute fail-closed") holds at runtime: the
    # operator actually reconciled a listener WITHOUT the mutating
    # matches. Static helm-template + yq can't prove the operator's xDS
    # translator respects the missing route; this curl can.
    #
    # NOT using -f here: we WANT to see the HTTP code. -f would make
    # curl exit nonzero on 404 and k3s_server.succeed would fail.
    # Instead, capture %{http_code} and assert on it.
    with subtest("method-gate: ClearPoison returns 404 (unrouted)"):
        code = k3s_server.succeed(
            "printf '\\x00\\x00\\x00\\x00\\x00' | "
            "curl -s -o /dev/null -w '%{http_code}' "
            "-X POST http://localhost:18080/rio.admin.AdminService/ClearPoison "
            "-H 'content-type: application/grpc-web+proto' "
            "-H 'x-grpc-web: 1' "
            "--data-binary @-"
        ).strip()
        print(f"ClearPoison http_code={code}")
        # Envoy Gateway returns 404 when no GRPCRoute matches (the
        # listener has no route for this path). If this starts
        # returning 200, either enableMutatingMethods got flipped or
        # the readonly route leaked a service-level match.
        assert code == "404", (
            f"ClearPoison was routed through the gateway (http_code={code}). "
            "The mutating GRPCRoute should NOT be rendered with default "
            "values — check dashboard.enableMutatingMethods."
        )

    # Positive control: the readonly ClusterStatus path DOES return 200
    # (already proven by the 0x00-prefix assert above, but make the
    # contrast explicit for the method-gate proof — same request shape,
    # only the URL path differs).
    with subtest("method-gate: ClusterStatus returns 200 (routed)"):
        code = k3s_server.succeed(
            "printf '\\x00\\x00\\x00\\x00\\x00' | "
            "curl -s -o /dev/null -w '%{http_code}' "
            "-X POST http://localhost:18080/rio.admin.AdminService/ClusterStatus "
            "-H 'content-type: application/grpc-web+proto' "
            "-H 'x-grpc-web: 1' "
            "--data-binary @-"
        ).strip()
        print(f"ClusterStatus http_code={code}")
        assert code == "200", (
            f"ClusterStatus unexpectedly unrouted (http_code={code}). "
            "The readonly GRPCRoute may have lost its match list."
        )

    k3s_server.execute("kill $(cat /tmp/pf-envoy.pid) 2>/dev/null || true")

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
