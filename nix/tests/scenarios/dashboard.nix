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
{
  pkgs,
  ns,
  # The k3s-full fixture — carries the deterministic vm-test HMAC key
  # the (4c) live-tail subtest signs its AppendLog assignment token
  # with, mirroring scenarios/log-service.nix.
  fixture,
  # The store's namespace (cross-namespace Service FQDN is the nginx
  # upstream under test).
  nsStore ? "rio-store",
}:
let
  protoset = import ../lib/protoset.nix { inherit pkgs; };
  grpcurl = "${pkgs.grpcurl}/bin/grpcurl";

  # ── (4c) live-tail identity constants ──────────────────────────────
  # Distinct from log-service.nix's identities (different scenario, no
  # cross-contamination if both ever share a fixture). Valid nixbase32.
  liveDrvHash32 = "1cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm";
  liveDrvBasename = "${liveDrvHash32}-vmtest-dash.drv";
  liveDrvFullPath = "/nix/store/${liveDrvBasename}";
  liveBuilderId = "vm-dash-live-builder";
  liveExecId = "01900000-0000-7000-8000-00000000bbbb";
  liveDerivationId = "01900000-0000-7000-8000-00000000eeee";

  # AssignmentClaims signed with the fixture's deterministic HMAC key —
  # the exact builder shape from scenarios/log-service.nix (see the
  # field-order/skip-serializing notes there).
  liveAssignmentToken =
    pkgs.runCommand "rio-dash-live-assignment-token"
      {
        nativeBuildInputs = [ pkgs.python3 ];
      }
      ''
        python3 - ${fixture.hmacKeys}/hmac.key > $out <<'EOF'
        import base64, hashlib, hmac, json, sys
        key = open(sys.argv[1], "rb").read()
        for suf in (b"\r\n", b"\n"):
            if key.endswith(suf):
                key = key[: -len(suf)]
                break
        claims = json.dumps(
            {
                "executor_id": "${liveBuilderId}",
                "drv_hash": "${liveDrvBasename}",
                "expected_outputs": [],
                "is_ca": False,
                "expiry_unix": 9999999999,
            },
            separators=(",", ":"),
        ).encode()
        sig = hmac.new(key, claims, hashlib.sha256).digest()
        b64 = lambda b: base64.urlsafe_b64encode(b).rstrip(b"=").decode()
        print(b64(claims) + "." + b64(sig))
        EOF
      '';

  # The gRPC-Web frame for TailLogRequest{derivation: liveDrvFullPath,
  # follow: true}: field 1 (string) + field 4 (bool varint), prefixed
  # with the 5-byte length header. Built at nix-build time so the
  # testScript curls a deterministic binary body.
  liveTailRequest =
    pkgs.runCommand "rio-dash-live-tail-request"
      {
        nativeBuildInputs = [ pkgs.python3 ];
      }
      ''
        python3 - > $out <<'EOF'
        import struct, sys
        drv = b"${liveDrvFullPath}"
        msg = b"\x0a" + bytes([len(drv)]) + drv + b"\x20\x01"
        sys.stdout.buffer.write(b"\x00" + struct.pack(">I", len(msg)) + msg)
        EOF
      '';
in
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
  # 0x80 not 0x00). ClusterStatus above is NOT gated,
  # so it can't witness a bad token; this subtest is the tripwire.
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

  # ── (4) gRPC-Web server-streaming THROUGH nginx → rio-store ──────
  # THE proxy_buffering-off proof. The dashboard's LogViewer reads
  # build logs from rio-store's LogService.TailLog; this proves the
  # rio_store nginx upstream (a cross-namespace Service FQDN) + its
  # location block actually proxy gRPC-Web streaming.
  # TailLogRequest{derivation:"nonexist"} → the store yields an
  # in-stream NotFound → tonic-web encodes the trailer as a
  # length-prefixed body frame with flag 0x80 (distinct from HTTP/2
  # trailers, which browsers can't read).
  #
  # If nginx's proxy_buffering were on (the default), nginx would
  # buffer the entire upstream response before flushing to the
  # client. For a short NotFound stream this would STILL eventually
  # produce 0x80 (the stream is tiny) — but the pipe's shape would
  # be different: no incremental frames, one blob at close. The
  # real victim is a LONG-running stream (WatchBuild, a multi-minute
  # TailLog) where nothing arrives until completion. We can't
  # easily probe that here without a real build; the 0x80-at-tail
  # grep combined with the nginx config assertion (proxy_buffering
  # off is hardcoded at docker.nix:357 and asserted by
  # checks.dashboard-nginx-conf-guard) is the practical gate.
  #
  # Request body: TailLogRequest{derivation:"nonexist"}
  #   = 0x0a (field 1 wire-type 2) 0x08 (len 8) "nonexist" = 10 bytes
  # → prefixed with 5-byte header (0x00,0x00,0x00,0x00,0x0a) — the
  # same encoding as dashboard-gateway.nix's request.
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

  # ── (4c) LIVE follow-tail via nginx: post-open lines reach the open
  # stream. THE incremental-delivery proof the (4) comment concedes it
  # cannot give: a short NotFound stream still yields 0x80 under
  # buffering, but a line ingested AFTER the stream opened only reaches
  # the open connection if nginx (proxy_buffering off, the rio_store
  # upstream) and the store's follow path actually flush incrementally
  # — the data plane the dashboard's follow:true LogViewer (B3) rides.
  #
  # Choreography (the follow contract: a stream opened with NO live
  # ingest session ends immediately and the CLIENT re-opens — so the
  # subtest holds ONE AppendLog session open via a FIFO and attaches
  # the follow stream mid-session):
  #   1. seed a running execution (the log-service scenario's rows)
  #   2. start a long-lived AppendLog session: grpcurl reads a FIFO; a
  #      backgrounded writer sends header+batch1 then parks on a flag
  #      file, holding the session open. While parked it emits an empty
  #      keepalive batch every ~5s: the store aborts a session whose
  #      buffer is empty (the 60s periodic cut empties it) with no
  #      inbound for INBOUND_IDLE_BOUND (60s), so a mute parked writer
  #      gives the whole choreography a hidden ~60-75s deadline from
  #      batch 1 to flag-touch — under full-gate host load the gates
  #      below burned that budget and the lone session lost its lease
  #      before batch 2 was sent (3 recorded gate strikes; the
  #      post-open grep then times out with nothing left to deliver).
  #      accept() skips the numbering checks for empty batches — the
  #      protocol's keepalive shape, explicitly non-cut-masking.
  #   3. open TailLog{follow:true} THROUGH nginx into a capture file —
  #      the snapshot serves batch 1 (proves attach while live)
  #   4. touch the flag → the writer sends batch 2 on the SAME session
  #      → the live fan-out delivers it to the ALREADY-OPEN stream
  #      (the load-bearing post-open assertion)
  # Both lease gates below poll log_ingest_sessions freshness — the
  # exact predicate the read path's lookup_live routes on (heartbeat
  # within SESSION_STALE_AFTER = 30s): structural DB state, not logs.
  with subtest("live tail via nginx: post-open lines reach the open stream"):
      psql_k8s(k3s_server,
          "INSERT INTO derivations "
          "(derivation_id, drv_hash, drv_path, system, status) VALUES "
          "('${liveDerivationId}', '${liveDrvBasename}', "
          " '${liveDrvFullPath}', 'x86_64-linux', 'running')")
      psql_k8s(k3s_server,
          "INSERT INTO assignments "
          "(derivation_id, builder_id, generation, status, exec_id) VALUES "
          "('${liveDerivationId}', '${liveBuilderId}', 1, 'acknowledged', "
          " '${liveExecId}')")
      psql_k8s(k3s_server,
          "INSERT INTO drv_executions "
          "(exec_id, drv_hash, executor_id, started_at) VALUES "
          "('${liveExecId}', '${liveDrvHash32}', '${liveBuilderId}', now())")

      # Request-message files: header + batch 1 (lines 0-1), batch 2
      # (lines 2-3). Contents are b64 of dash-live-NNNNN.
      import base64 as _b64
      def _line(t):
          return _b64.b64encode(t.encode()).decode()
      k3s_server.succeed(
          "printf '%s\n%s\n' "
          + "'{\"header\":{\"derivationPath\":\"${liveDrvFullPath}\","
          + "\"execId\":\"${liveExecId}\"}}' "
          + "'{\"batch\":{\"lines\":[\"" + _line("dash-live-00000")
          + "\",\"" + _line("dash-live-00001") + "\"],"
          + "\"firstLineNumber\":\"0\"}}' > /tmp/dash-b1.json"
      )
      k3s_server.succeed(
          "printf '%s\n' "
          + "'{\"batch\":{\"lines\":[\"" + _line("dash-live-00002")
          + "\",\"" + _line("dash-live-00003") + "\"],"
          + "\"firstLineNumber\":\"2\"}}' > /tmp/dash-b2.json"
      )

      # bug_309: the client wall-clock caps are DERIVED from the summed
      # downstream gate budgets + slack, in ONE binding — a cap below
      # the sum kills a healthy-but-slow run that stays inside every
      # gate's own budget (the exact load regime those budgets were
      # sized for), striking as the same unnamed timeout this
      # choreography was restructured to eliminate (pre-fix:
      # grpcurl -max-time 300 vs 480s of summed pre-batch-2 gates;
      # curl --max-time 180 vs 240s of post-open gates). The finally
      # block kills both processes, so generosity is free: the caps
      # are leak insurance, never the pacing control. The SESSION's
      # liveness is governed separately by the writer's ~5s keepalive
      # cadence vs the store's 60s inbound-idle bound (see the
      # choreography comment above) — wall-clock caps must only
      # outlive the gates. Adding a gate to the choreography extends
      # the caps automatically through these bindings.
      lease_gate_1 = 180  # ingest chain to batch-1 lease (recorded >150s tail)
      data_gate    = 150  # one-shot serve: cut interval + slack (see below)
      open_grep    = 90   # snapshot phase serves batch 1
      lease_gate_2 = 60   # two staleness windows of psql/exec slack
      post_grep    = 90   # live fan-out delivers batch 2
      cap_slack    = 120  # spawn/teardown + load-dilated sleep loops
      appendlog_cap = lease_gate_1 + data_gate + open_grep + lease_gate_2 + post_grep + cap_slack
      follow_cap    = open_grep + lease_gate_2 + post_grep + cap_slack

      # Long-lived store port-forward for the AppendLog session.
      pf_open("svc/rio-store", 19510, 9002, ns="${nsStore}", tag="pf-store-live")
      token = k3s_server.succeed("cat ${liveAssignmentToken}").strip()
      # The session: grpcurl reads the FIFO; the writer holds it open.
      k3s_server.succeed(
          "mkfifo /tmp/dash-live.fifo 2>/dev/null || true; "
          "rm -f /tmp/dash-live.go2; "
          # Every backgrounded process FULLY redirects its fds — an
          # inherited test-driver backdoor descriptor breaks the
          # driver's channel ([Errno 9] Bad file descriptor).
          "(${grpcurl} -plaintext -max-time " + str(appendlog_cap) + " "
          "-protoset ${protoset}/rio.protoset "
          "-H 'x-rio-assignment-token: " + token + "' "
          "-d @ localhost:19510 rio.store.LogService/AppendLog "
          "< /tmp/dash-live.fifo > /tmp/dash-live-acks.log 2>&1) "
          "& echo $! > /tmp/dash-grpcurl.pid; "
          # The parked loop's empty keepalive batches (see the
          # choreography comment): ~5s nominal cadence vs the 60s
          # inbound-idle bound — an order of magnitude of headroom for
          # load-dilated sleep loops.
          "(exec 3>/tmp/dash-live.fifo; cat /tmp/dash-b1.json >&3; "
          "i=0; until [ -f /tmp/dash-live.go2 ]; do "
          "sleep 0.5; i=$((i+1)); "
          "if [ $((i % 10)) -eq 0 ]; then echo '{\"batch\":{}}' >&3; fi; "
          "done; "
          "cat /tmp/dash-b2.json >&3; exec 3>&-) "
          "</dev/null >/tmp/dash-writer.log 2>&1 "
          "& echo $! > /tmp/dash-writer.pid"
      )

      # Structural gate: the ingest-session lease row, fresh. This is
      # the signal the read path itself routes on (lookup_live serves
      # the live view only for heartbeat_at within 30s) — until it
      # holds, no reader anywhere can see the live session. 180s: the
      # recorded strike showed the ingest chain (port-forward + grpcurl
      # + store accept) taking >150s to land batch 1 under full-gate
      # load — budget for that tail, not the ~1s typical.
      lease_fresh = (
          "k3s kubectl -n ${ns} exec rio-postgresql-0 -- "
          "env PGPASSWORD=rio psql -h 127.0.0.1 -U rio rio -qtAc "
          "\"SELECT 1 FROM log_ingest_sessions "
          "WHERE exec_id='${liveExecId}' "
          "AND heartbeat_at > now() - interval '30 seconds'\" "
          "| grep -qx 1"
      )
      k3s_server.wait_until_succeeds(lease_fresh, timeout=lease_gate_1)

      # Data gate, after the lease gate: a one-shot TailLog (direct,
      # the long-lived store port-forward) must serve batch 1 first. A
      # follow stream opened before the session exists ends immediately
      # by contract (the client re-opens) — the gate removes that race
      # rather than looping the curl.
      b64_line1 = "ZGFzaC1saXZlLTAwMDAx"  # b64("dash-live-00001")
      # timeout budget: the one-shot rides a port-forward that can land
      # on the NON-owning replica; the cross-replica tail proxy degrades
      # to the history-only view on relay failure (by design —
      # rio_store_log_tail_proxy_failures_total), and history-only means
      # batch 1 is visible only after the periodic chunk cut
      # (DEFAULT_CUT_INTERVAL = 60s). A 60s wait sat exactly ON that
      # period (observed timeouts 60.67s/60.91s — pure phase luck);
      # 150s = one full cut interval + grpc/scrape slack + builder-load
      # headroom. The fast path (owning replica / healthy proxy) still
      # exits in ~1s.
      k3s_server.wait_until_succeeds(
          "${grpcurl} -plaintext -max-time 10 "
          "-protoset ${protoset}/rio.protoset "
          "-d '{\"derivation\":\"${liveDrvFullPath}\",\"follow\":false}' "
          "localhost:19510 rio.store.LogService/TailLog "
          "| grep -q " + b64_line1, timeout=data_gate,
      )

      try:
          # Open the follow stream THROUGH nginx while the session is
          # live. -N disables curl buffering; the capture accumulates
          # raw gRPC-Web frames (line bytes appear verbatim inside DATA
          # frames).
          k3s_server.succeed(
              "curl -sN --max-time " + str(follow_cap) + " -X POST "
              "http://localhost:18081/rio.store.LogService/TailLog "
              "-H 'content-type: application/grpc-web+proto' "
              "-H 'x-grpc-web: 1' "
              "--data-binary @${liveTailRequest} "
              "</dev/null > /tmp/livetail.bin 2>/dev/null "
              "& echo $! > /tmp/livetail.pid"
          )
          # The snapshot phase serves batch 1 (already in the live
          # buffer): proves the stream attached to the live session.
          k3s_server.wait_until_succeeds(
              "grep -aq dash-live-00001 /tmp/livetail.bin", timeout=open_grep
          )
          # The post-open assertion's precondition, made structural:
          # the session that must carry batch 2 is STILL leased. Under
          # full-gate host load the lone ingest session used to lose
          # its lease here (inbound-idle abort once the 60s cut emptied
          # the buffer) and the grep below struck three gates as an
          # unnamed 90s timeout. With the writer's keepalives the lease
          # only goes stale if the chain (writer → FIFO → grpcurl →
          # port-forward → store driver) actually died — fail loud and
          # named here instead. 60s = two staleness windows of psql/
          # exec slack under load.
          k3s_server.wait_until_succeeds(lease_fresh, timeout=lease_gate_2)
          # Batch 2, sent on the SAME open session AFTER the stream
          # opened: the live fan-out must deliver it to the open
          # connection — the incremental flush through nginx that
          # proxy_buffering off exists to allow.
          k3s_server.succeed("touch /tmp/dash-live.go2")
          k3s_server.wait_until_succeeds(
              "grep -aq dash-live-00003 /tmp/livetail.bin", timeout=post_grep
          )
      except Exception:
          print("== DIAGNOSTIC: live-tail subtest ==")
          print(k3s_server.execute(
              "echo '-- acks:'; cat /tmp/dash-live-acks.log 2>&1; "
              "echo '-- writer:'; cat /tmp/dash-writer.log 2>&1; "
              "echo '-- capture:'; ${pkgs.xxd}/bin/xxd /tmp/livetail.bin 2>&1 | head -20; "
              "echo '-- store logs:'; "
              "k3s kubectl -n ${nsStore} logs deploy/rio-store --tail=40 2>&1"
          )[1])
          raise
      finally:
          # Teardown: the writer exits after batch 2 (closing the FIFO
          # ends the AppendLog session); kill the follow curl + pf.
          k3s_server.execute(
              "kill $(cat /tmp/livetail.pid) 2>/dev/null || true; "
              "kill $(cat /tmp/dash-grpcurl.pid) 2>/dev/null || true"
          )
          pf_close("pf-store-live")

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
      # (Readonly methods reaching the upstreams is already proven by
      # the ClusterStatus 0x00 + TailLog 0x80 subtests above —
      # both use the proper grpc-web headers; a bare `-d x` curl here
      # would hit tonic-web's content-type check, not nginx.)

  k3s_server.execute("kill $(cat /tmp/pf-nginx.pid) 2>/dev/null || true")
''
