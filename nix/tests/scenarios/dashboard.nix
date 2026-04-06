# nginx → Envoy Gateway → scheduler chain, curl-only (no Playwright).
#
# dashboard-gateway.nix proves the Envoy Gateway gRPC-Web translation
# works by curling the envoy data-plane Service directly. This scenario
# proves the FULL deployment chain: curl hits the nginx pod (the actual
# rio-dashboard Deployment that the browser talks to) which serves the
# SPA bundle and proxies /rio.* POSTs to the Envoy Gateway Service.
#
# Four assertions:
#   1. SPA served — index.html has the Svelte mount <div id="app">
#   2. SPA routing fallback — /builds/xyz returns index.html (try_files)
#   3. Unary gRPC-Web THROUGH nginx — 0x00 DATA frame prefix
#   4. Server-streaming THROUGH nginx — 0x80 trailer byte
#
# (4) is the load-bearing proxy_buffering-off proof: if nginx buffered
# the stream (the default), curl would receive nothing until the stream
# closed — no incremental 0x80 frame at the tail. dashboard-gateway.nix
# can't prove this; it port-forwards directly to envoy, bypassing nginx.
#
# Same k3s fixture as vm-dashboard-gateway-k3s (envoyGatewayEnabled=true
# preloads the rio-dashboard image AND the envoy-gateway operator). ~6min
# — ~4min bring-up + ~60s operator reconcile + ~30s nginx schedule + curls.
#
# r[verify dash.journey.build-to-logs] lives at the wiring point
# (default.nix:vm-dashboard-k3s) per the P0341 markers-at-subtests-entry
# convention — the scenario header explains WHAT the four curls prove.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) ns;
  egNs = "envoy-gateway-system";
in
pkgs.testers.runNixOSTest {
  name = "rio-dashboard-curl";
  skipTypeCheck = true;

  # Bring-up ~4min + operator reconcile ~60s + nginx pod schedule ~30s +
  # curls <15s. 900s matches vm-dashboard-gateway-k3s.
  globalTimeout = 900 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.kvmPreopen}
    ${common.assertions}

    start_all()
    ${fixture.waitReady}
    ${fixture.kubectlHelpers}
    ${fixture.sshKeySetup}

    # ── Envoy Gateway operator Available ──────────────────────────────
    # Same gate as dashboard-gateway.nix — nginx proxies to the envoy
    # Service, so we need the operator reconciled before the curls hit
    # the /rio.* location. SPA curls (1+2) don't need this; placed
    # here so the grpc_web gate has time to settle while we test SPA.
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n envoy-gateway-system wait --for=condition=Available "
        "deploy/envoy-gateway --timeout=120s",
        timeout=150,
    )

    # ── nginx Deployment Available ────────────────────────────────────
    # dashboard.yaml renders when dashboard.enabled=true (set by the
    # fixture's envoyGatewayEnabled flag). The rio-dashboard:dev image
    # is preloaded (k3s-full.nix rioImages — guarded by dockerImages ?
    # dashboard so coverage-mode skips the scenario entirely rather
    # than deadlock on ImagePullBackOff here).
    with subtest("nginx Available: SPA Deployment rolled out"):
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${ns} wait --for=condition=Available "
            "deploy/rio-dashboard --timeout=120s",
            timeout=150,
        )

    # ── Port-forward to the nginx Service ─────────────────────────────
    # Service maps :80 → targetPort 8080 (nginx listens on 8080 —
    # runAsNonRoot, no CAP_NET_BIND_SERVICE). Same pattern as the envoy
    # port-forward in dashboard-gateway.nix.
    k3s_server.succeed(
        "k3s kubectl -n ${ns} port-forward svc/rio-dashboard 18081:80 "
        ">/tmp/pf-nginx.log 2>&1 & echo $! > /tmp/pf-nginx.pid"
    )
    k3s_server.wait_until_succeeds(
        "${pkgs.netcat}/bin/nc -z localhost 18081", timeout=10
    )

    # ── (1) SPA served: index.html has Svelte mount point ─────────────
    # rio-dashboard/index.html:9 — <div id="app"></div>. vite build
    # preserves this (it's the mount target for main.ts's `new App({
    # target: ...})`). grep -F for literal match — no regex metachars
    # in id="app" but -F is future-proof against HTML quoting changes.
    with subtest("SPA served: index.html has id=app mount point"):
        k3s_server.succeed(
            "curl -sf http://localhost:18081/ | grep -qF 'id=\"app\"'"
        )

    # ── (2) SPA routing fallback: unknown path → index.html ──────────
    # nginx.conf try_files $uri /index.html — the client-side router
    # handles the path. /builds/<id> is a real dashboard route (Builds
    # page → DAG view); /builds/nonexistent proves the fallback fires
    # (no such static file, no such proxy target — try_files catches
    # it). Without try_files: 404 → grep fails → test red.
    with subtest("SPA routing fallback: /builds/<id> returns index.html"):
        k3s_server.succeed(
            "curl -sf http://localhost:18081/builds/nonexistent "
            "| grep -qF 'id=\"app\"'"
        )

    # ── Envoy Gateway Programmed (before gRPC-Web curls) ──────────────
    # nginx's upstream is `rio-dashboard-envoy.envoy-gateway-system.
    # svc.cluster.local:8080` (baked into the image, docker.nix
    # dashboardNginxConf). That Service exists only after the operator
    # reconciles the Gateway CR → envoy Deployment + Service. The
    # operator is Available (above) but reconcile is async — wait for
    # Gateway Programmed like dashboard-gateway.nix does.
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${ns} get gateway rio-dashboard "
        "-o jsonpath='{.status.conditions[?(@.type==\"Programmed\")].status}' "
        "| grep -qx True",
        timeout=120,
    )
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${egNs} get endpoints rio-dashboard-envoy "
        "-o jsonpath='{.subsets[0].addresses[0].ip}' | grep -q .",
        timeout=60,
    )

    # ── (3) gRPC-Web unary THROUGH nginx ─────────────────────────────
    # curl → nginx:8080 → /rio.admin.AdminService/ClusterStatus matches
    # the `location ~ ^/rio\.(admin|scheduler)\./` block → proxy_pass
    # to envoy-gateway Service → grpc_web filter → mTLS to scheduler.
    # The 0x00 byte is the gRPC-Web DATA frame compression flag — if
    # ANY hop mangles the binary framing (e.g., nginx gzip, envoy
    # buffering, wrong content-type pass-through), the first byte
    # won't be 0x00.
    #
    # Empty proto body = 5-byte header (1 byte flag + 4 bytes len=0).
    # wait_until_succeeds: the GRPCRoute backendRef load-balances
    # across scheduler replicas; non-leader returns Unavailable. ~5
    # retries covers the 50% hit rate.
    with subtest("gRPC-Web unary via nginx: ClusterStatus 0x00 prefix"):
        k3s_server.wait_until_succeeds(
            "printf '\\x00\\x00\\x00\\x00\\x00' | "
            "curl -sf -X POST http://localhost:18081/rio.admin.AdminService/ClusterStatus "
            "-H 'content-type: application/grpc-web+proto' "
            "-H 'x-grpc-web: 1' "
            "--data-binary @- "
            "| ${pkgs.xxd}/bin/xxd -l 16 | grep -q '^00000000: 00'",
            timeout=30,
        )

    # ── (4) gRPC-Web server-streaming THROUGH nginx ──────────────────
    # THE proxy_buffering-off proof. GetBuildLogs with a nonexistent
    # drv_path → scheduler sends zero log lines + trailer frame
    # (grpc-status: 5 NotFound). Envoy's grpc_web filter encodes the
    # trailer as a length-prefixed message with flag 0x80 (distinct
    # from HTTP/2 trailers which browsers can't read).
    #
    # If nginx's proxy_buffering were on (the default), nginx would
    # buffer the entire upstream response before flushing to the
    # client. For a short NotFound stream this would STILL eventually
    # produce 0x80 (the stream is tiny) — but the pipe's shape would
    # be different: no incremental frames, one blob at close. The
    # real victim is a LONG-running stream (WatchBuild, a multi-minute
    # GetBuildLogs) where nothing arrives until completion. We can't
    # easily probe that here without a real build; the 0x80-at-tail
    # grep combined with the nginx config assertion (proxy_buffering
    # off is hardcoded at docker.nix:234 and asserted by helm-lint)
    # is the practical gate.
    #
    # Request body: GetBuildLogsRequest{drv_path:"nonexist"} =
    #   0x0a (field 1 wire-type 2) 0x08 (len 8) "nonexist" = 10 bytes
    # → prefixed with 5-byte header (0x00,0x00,0x00,0x00,0x0a).
    with subtest("gRPC-Web streaming via nginx: GetBuildLogs 0x80 trailer"):
        k3s_server.wait_until_succeeds(
            "printf '\\x00\\x00\\x00\\x00\\x0a\\x0a\\x08nonexist' | "
            "curl -sf -X POST http://localhost:18081/rio.admin.AdminService/GetBuildLogs "
            "-H 'content-type: application/grpc-web+proto' "
            "-H 'x-grpc-web: 1' "
            "--data-binary @- "
            "| ${pkgs.xxd}/bin/xxd | grep -q ' 80'",
            timeout=30,
        )

    k3s_server.execute("kill $(cat /tmp/pf-nginx.pid) 2>/dev/null || true")

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
