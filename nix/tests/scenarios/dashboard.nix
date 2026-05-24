# nginx → scheduler chain, curl-only (no Playwright).
#
# dashboard-gateway.nix proves the Envoy Gateway gRPC-Web translation
# works by curling the envoy data-plane Service directly. THIS fragment
# proves the FULL deployment chain: curl hits the nginx pod (the actual
# rio-dashboard Deployment that the browser talks to) which serves the
# SPA bundle and proxies /rio.* POSTs to rio-scheduler.
#
# Six assertions:
#   1. SPA served — index.html has the Svelte mount <div id="app">
#   2. SPA routing fallback — /builds/xyz returns index.html (try_files)
#   2b. rio-scheduler-leader EndpointSlice has exactly 1 ready endpoint
#       (the lease holder labeled its own pod — nginx's upstream)
#   3. Unary gRPC-Web THROUGH nginx — 0x00 DATA frame prefix
#   4. Server-streaming THROUGH nginx → scheduler — 0x80 trailer byte
#   4b. Server-streaming THROUGH nginx → rio-store TailLog — 0x80
#       trailer byte (the LogViewer's post-cutover log-read path; the
#       second upstream + cross-namespace Service FQDN)
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

  # ── (2b) leader-only Service has exactly one ready endpoint ──────
  # nginx's upstream is rio-scheduler-leader, whose selector includes
  # the rio.build/scheduler-role=leader label the lease holder stamps
  # on its own pod (rio-lease spawn_patch_leader_marks, level-triggered
  # off the election loop). Until that patch lands the Service has 0
  # endpoints and every proxied request fails with a connect error.
  # Waiting here turns "the leader never labeled itself" (RBAC, patch
  # bug, label typo between chart and Rust constant) into an immediate,
  # named failure instead of a 60s timeout in the gRPC-Web subtests
  # below. Exactly 1 (not >=1): 2 ready endpoints would mean the
  # standby is also labeled — the exact bug this Service exists to
  # prevent.
  with subtest("leader Service: rio-scheduler-leader has exactly 1 ready endpoint"):
      # The peer sweep (rio-lease sweep_peer_leader_marks) needs `list
      # pods` to strip a partitioned ex-leader's stale label. Mock tests
      # bypass RBAC and helm-lint only templates, so assert the live
      # grant here, where a forgotten verb becomes a loud named failure
      # instead of a silently never-running sweep.
      k3s_server.succeed(
          "k3s kubectl auth can-i list pods "
          "--as=system:serviceaccount:${ns}:rio-scheduler -n ${ns}"
      )
      k3s_server.wait_until_succeeds(
          "test \"$(k3s kubectl -n ${ns} get endpointslices "
          "-l kubernetes.io/service-name=rio-scheduler-leader "
          "-o jsonpath='{.items[*].endpoints[?(@.conditions.ready==true)].targetRef.name}' "
          "| wc -w)\" = 1",
          timeout=60,
      )

  # ── (3) gRPC-Web unary THROUGH nginx ─────────────────────────────
  # curl → nginx:8080 → /rio.admin.AdminService/ClusterStatus matches
  # the `location ~ ^/rio\.(admin|scheduler)\./` block → proxy_pass
  # straight to rio-scheduler-leader:9001 (D3: scheduler serves
  # gRPC-Web natively via tonic-web; no Gateway hop in-cluster).
  # The 0x00 byte is the gRPC-Web DATA frame compression flag — if
  # ANY hop mangles the binary framing (e.g., nginx gzip, envoy
  # buffering, wrong content-type pass-through), the first byte
  # won't be 0x00.
  #
  # Empty proto body = 5-byte header (1 byte flag + 4 bytes len=0).
  # wait_until_succeeds: nginx's upstream is the leader-only Service
  # (selector includes the leader label), so a Trailers-Only
  # Unavailable from the standby is structurally impossible — the
  # standby is never an endpoint. What CAN still fail transiently is
  # the window between subtest 2b and nginx re-resolving (rare —
  # ClusterIP is stable, only endpoints change), or a hung upstream
  # connection (nginx proxy_connect_timeout is 60s — one hang would
  # eat the whole budget without --max-time). --max-time 5 caps each
  # attempt so the budget always buys >=10 independent tries; the
  # except block captures the evidence needed to tell those modes
  # apart if it still fails.
  try:
      with subtest("gRPC-Web unary via nginx: ClusterStatus 0x00 prefix"):
          k3s_server.wait_until_succeeds(
              "printf '\\x00\\x00\\x00\\x00\\x00' | "
              "curl -sf --max-time 5 -X POST http://localhost:18081/rio.admin.AdminService/ClusterStatus "
              "-H 'content-type: application/grpc-web+proto' "
              "-H 'x-grpc-web: 1' "
              "--data-binary @- "
              "| ${pkgs.xxd}/bin/xxd | head -1 | grep -q '^00000000: 00'",
              timeout=60,
          )

  # ── (3b) service-token via nginx: njs HMAC verifies ──────────────
  # ListPoisoned is r[sched.sla.threat.read-path-auth]-gated: the
  # scheduler verifies the njs-minted x-rio-service-token (docker.nix
  # dashboardServiceTokenJs). The key fixture (lib/hmac-keys.nix)
  # appends a trailing LF, so any consumer that doesn't byte-trim it
  # (mirroring rio-auth load_key) computes a divergent HMAC and every
  # gated RPC returns PermissionDenied (Trailers-Only, first byte
  # 0x80 not 0x00). ClusterStatus/GetDerivationLogs above are NOT gated,
  # so they can't witness a bad token; this subtest is the tripwire.
      with subtest("service-token via nginx: njs HMAC verifies on gated RPC"):
          k3s_server.wait_until_succeeds(
              "printf '\\x00\\x00\\x00\\x00\\x00' | "
              "curl -sf --max-time 5 -X POST http://localhost:18081/rio.admin.AdminService/ListPoisoned "
              "-H 'content-type: application/grpc-web+proto' "
              "-H 'x-grpc-web: 1' "
              "--data-binary @- "
              "| ${pkgs.xxd}/bin/xxd | head -1 | grep -q '^00000000: 00'",
              timeout=60,
          )

  # ── (4) gRPC-Web server-streaming THROUGH nginx ──────────────────
  # THE proxy_buffering-off proof. GetDerivationLogs with a nonexistent
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
  # GetDerivationLogs) where nothing arrives until completion. We can't
  # easily probe that here without a real build; the 0x80-at-tail
  # grep combined with the nginx config assertion (proxy_buffering
  # off is hardcoded at docker.nix:357 and asserted by
  # checks.dashboard-nginx-conf-guard) is the practical gate.
  #
  # Request body: GetDerivationLogsRequest{derivation_path:"nonexist"}
  #   = 0x0a (field 1 wire-type 2) 0x08 (len 8) "nonexist" = 10 bytes
  # → prefixed with 5-byte header (0x00,0x00,0x00,0x00,0x0a).
  # derivation_path moved to field 1 (build_id removed; exec_id is
  # field 2) — same encoding as dashboard-gateway.nix.
      with subtest("gRPC-Web streaming via nginx: GetDerivationLogs 0x80 trailer"):
          k3s_server.wait_until_succeeds(
              "printf '\\x00\\x00\\x00\\x00\\x0a\\x0a\\x08nonexist' | "
              "curl -sf --max-time 5 -X POST http://localhost:18081/rio.admin.AdminService/GetDerivationLogs "
              "-H 'content-type: application/grpc-web+proto' "
              "-H 'x-grpc-web: 1' "
              "--data-binary @- "
              "| ${pkgs.xxd}/bin/xxd | grep -q ' 80'",
              timeout=60,
          )

  # ── (4b) gRPC-Web server-streaming THROUGH nginx → rio-store ─────
  # The dashboard's LogViewer now reads build logs from rio-store's
  # LogService.TailLog (the log data plane moved off the scheduler;
  # GetDerivationLogs above survives only until the deletion commit).
  # This is the proof that the SECOND nginx upstream (rio_store, a
  # cross-namespace Service FQDN) + the new location block actually
  # proxy gRPC-Web streaming: TailLogRequest{derivation:"nonexist"} →
  # the store yields an in-stream NotFound → tonic-web encodes the
  # trailer as a 0x80-flagged body frame. Same wire encoding as (4):
  # field 1 is a string in both request messages.
      with subtest("gRPC-Web streaming via nginx: store TailLog 0x80 trailer"):
          k3s_server.wait_until_succeeds(
              "printf '\\x00\\x00\\x00\\x00\\x0a\\x0a\\x08nonexist' | "
              "curl -sf --max-time 5 -X POST http://localhost:18081/rio.store.LogService/TailLog "
              "-H 'content-type: application/grpc-web+proto' "
              "-H 'x-grpc-web: 1' "
              "--data-binary @- "
              "| ${pkgs.xxd}/bin/xxd | grep -q ' 80'",
              timeout=60,
          )
  except Exception:
      # Discriminate "leader-only Service has no/extra endpoints" (the
      # label patch never landed, or the standby is also labeled) from
      # "connections to the leader hang" (504s / connect timeouts) from
      # "nginx still resolves the old ClusterIP" (200s with ~0 body
      # bytes would mean a standby answered — impossible unless the
      # selector matched it).
      print("== DIAGNOSTIC: nginx -> scheduler gRPC-Web proxy path ==")
      print(k3s_server.execute(
          "printf '\\x00\\x00\\x00\\x00\\x00' | "
          "curl -sv --max-time 5 -X POST http://localhost:18081/rio.admin.AdminService/ClusterStatus "
          "-H 'content-type: application/grpc-web+proto' "
          "-H 'x-grpc-web: 1' --data-binary @- 2>&1 | head -40"
      )[1])
      print(k3s_server.execute(
          "k3s kubectl -n ${ns} logs deploy/rio-dashboard --tail=80 2>&1"
      )[1])
      print(k3s_server.execute(
          "k3s kubectl -n ${ns} get endpointslices -l kubernetes.io/service-name=rio-scheduler-leader -o yaml 2>&1; "
          "k3s kubectl -n ${ns} get endpointslices -l kubernetes.io/service-name=rio-scheduler -o yaml 2>&1; "
          "k3s kubectl -n ${ns} get pods -o wide --show-labels 2>&1; "
          "k3s kubectl -n ${ns} get lease rio-scheduler-leader -o jsonpath='{.spec.holderIdentity}' 2>&1"
      )[1])
      raise

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
      # the ClusterStatus 0x00 + GetDerivationLogs 0x80 subtests above —
      # both use the proper grpc-web headers; a bare `-d x` curl here
      # would hit tonic-web's content-type check, not nginx.)

  k3s_server.execute("kill $(cat /tmp/pf-nginx.pid) 2>/dev/null || true")
''
