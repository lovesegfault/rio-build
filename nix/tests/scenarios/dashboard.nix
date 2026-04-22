# nginx → scheduler chain, curl-only (no Playwright).
#
# dashboard-gateway.nix proves the Envoy Gateway gRPC-Web translation
# works by curling the envoy data-plane Service directly. THIS fragment
# proves the FULL deployment chain: curl hits the nginx pod (the actual
# rio-dashboard Deployment that the browser talks to) which serves the
# SPA bundle and proxies /rio.* POSTs to rio-scheduler.
#
# Five assertions:
#   1. SPA served — index.html has the Svelte mount <div id="app">
#   2. SPA routing fallback — /builds/xyz returns index.html (try_files)
#   3. Unary gRPC-Web THROUGH nginx — 0x00 DATA frame prefix
#   4. Server-streaming THROUGH nginx — 0x80 trailer byte
#   5. method-gate via nginx — allow-list fail-closed
#
# (4) is the streaming-through-nginx proof. proxy_buffering-off itself
# is guarded by checks.dashboard-nginx-conf-guard (misc-checks.nix) — a
# short NotFound stream would still produce 0x80 even if buffered, so
# (4) alone can't distinguish.
#
# This file is a testScript FRAGMENT — interpolated into
# dashboard-gateway.nix when `withDashboardCurls = true` (i.e. the
# rio-dashboard image is in the airgap set; coverage mode skips it).
{ pkgs, ns }:
''
  # ── nginx Deployment Available ────────────────────────────────────
  # dashboard.yaml renders when dashboard.enabled=true (set by the
  # fixture's gatewayEnabled flag). The rio-dashboard:dev image
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
  # runAsNonRoot, no CAP_NET_BIND_SERVICE).
  pf_open("svc/rio-dashboard", 18081, 80, tag="pf-nginx")

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

  # ── (3) gRPC-Web unary THROUGH nginx ─────────────────────────────
  # curl → nginx:8080 → /rio.admin.AdminService/ClusterStatus matches
  # the `location ~ ^/rio\.(admin|scheduler)\./` block → proxy_pass
  # straight to rio-scheduler:9001 (D3: scheduler serves gRPC-Web
  # natively via tonic-web; no Gateway hop in-cluster).
  # The 0x00 byte is the gRPC-Web DATA frame compression flag — if
  # ANY hop mangles the binary framing (e.g., nginx gzip, envoy
  # buffering, wrong content-type pass-through), the first byte
  # won't be 0x00.
  #
  # Empty proto body = 5-byte header (1 byte flag + 4 bytes len=0).
  # wait_until_succeeds: the HTTPRoute backendRef load-balances
  # across scheduler replicas; non-leader returns Unavailable. ~5
  # retries covers the 50% hit rate.
  with subtest("gRPC-Web unary via nginx: ClusterStatus 0x00 prefix"):
      k3s_server.wait_until_succeeds(
          "printf '\\x00\\x00\\x00\\x00\\x00' | "
          "curl -sf -X POST http://localhost:18081/rio.admin.AdminService/ClusterStatus "
          "-H 'content-type: application/grpc-web+proto' "
          "-H 'x-grpc-web: 1' "
          "--data-binary @- "
          "| ${pkgs.xxd}/bin/xxd | head -1 | grep -q '^00000000: 00'",
          timeout=60,
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
  # off is hardcoded at docker.nix:357 and asserted by
  # checks.dashboard-nginx-conf-guard) is the practical gate.
  #
  # Request body: GetBuildLogsRequest{derivation_path:"nonexist"} =
  #   0x12 (field 2 wire-type 2) 0x08 (len 8) "nonexist" = 10 bytes
  # → prefixed with 5-byte header (0x00,0x00,0x00,0x00,0x0a).
  # Proto refactor at b643ab82 moved derivation_path to field 2
  # (field 1 is build_id) — same encoding as dashboard-gateway.nix.
  with subtest("gRPC-Web streaming via nginx: GetBuildLogs 0x80 trailer"):
      k3s_server.wait_until_succeeds(
          "printf '\\x00\\x00\\x00\\x00\\x0a\\x12\\x08nonexist' | "
          "curl -sf -X POST http://localhost:18081/rio.admin.AdminService/GetBuildLogs "
          "-H 'content-type: application/grpc-web+proto' "
          "-H 'x-grpc-web: 1' "
          "--data-binary @- "
          "| ${pkgs.xxd}/bin/xxd | grep -q ' 80'",
          timeout=60,
      )

  # ── (5) method-gate via nginx: allow-list fail-closed ────────────
  # nginx's catch-all /rio.* location (docker.nix dashboardNginxConf)
  # returns 404 for anything NOT in the readonly allow-list — proves
  # the browser-origin can't reach mutating methods even though the
  # upstream scheduler would accept them. Before the allow-list
  # conversion, nginx had a 4-method DENY-list that fail-OPENED ~10
  # mutating RPCs (ResetSlaModel, CancelBuild, …) — those reached the
  # scheduler and returned a gRPC error encoded as HTTP 200.
  with subtest("method-gate via nginx: allow-list fail-closed"):
      # Original deny-list entry — still blocked.
      k3s_server.succeed(
          "curl -s -o /dev/null -w '%{http_code}' -X POST "
          "http://localhost:18081/rio.admin.AdminService/ClearPoison -d x "
          "| grep -qx 404"
      )
      # Mutating admin RPC NOT in old deny-list → was 200 before fix.
      k3s_server.succeed(
          "curl -s -o /dev/null -w '%{http_code}' -X POST "
          "http://localhost:18081/rio.admin.AdminService/ResetSlaModel -d x "
          "| grep -qx 404"
      )
      # Mutating scheduler RPC → old deny-list never gated
      # SchedulerService at all; was 200 before fix.
      k3s_server.succeed(
          "curl -s -o /dev/null -w '%{http_code}' -X POST "
          "http://localhost:18081/rio.scheduler.SchedulerService/CancelBuild -d x "
          "| grep -qx 404"
      )
      # (Readonly methods reaching the scheduler is already proven by
      # the ClusterStatus 0x00 + GetBuildLogs 0x80 subtests above —
      # both use the proper grpc-web headers; a bare `-d x` curl here
      # would hit tonic-web's content-type check, not nginx.)

  k3s_server.execute("kill $(cat /tmp/pf-nginx.pid) 2>/dev/null || true")
''
