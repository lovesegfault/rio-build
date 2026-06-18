# Standby-burst scenario — round-9 live_055(e) constituent (W9-AI).
#
# live_055 observed scheduler standby FLAPPING under the KEDA 2→8
# store scale-out burst (EKS, scheduler replicas=2). This pins the
# inverse, structurally: across a store scale-out burst plus a
# parallel build burst, scheduler leadership never moves —
# leaseTransitions delta == 0 AND holderIdentity unchanged — and the
# standby stays exactly in its documented posture the whole time:
# responsive-but-Unavailable, answered as gRPC Trailers-Only (the
# ci-failure-patterns envoy/nginx family's root shape).
#
# At VM scale there is no KEDA operator (airgapped image set — see the
# substitute-scale builder note in default.nix); the burst drives the
# same ACTUATION KEDA performs: kubectl scale of deploy/rio-store,
# up and back down, while builds churn the control plane. The EKS-
# scale CPU-contention face does not reproduce in a 2-node VM — the
# asserts here are the REGRESSION PIN for the lease-stability law
# (the disclosed-strawman form recorded in the landing commit).
#
# Trailers-Only assert mechanism (the dashboard-gateway.nix D3
# port-forward pattern): the scheduler serves gRPC-Web natively on
# :9001 (tonic-web, accept_http1). A SERVED unary answers a DATA frame
# (body starts 0x00; status arrives in a later trailer frame). A
# REFUSED unary is Trailers-Only — per the ci-failure-patterns standby
# family, the grpc-status rides the HTTP response HEADERS and the body
# is EMPTY (verified live by this scenario's first run). Port-forward
# to a SPECIFIC pod (svc/ would pick an arbitrary replica).
#
# Verify markers live at the default.nix subtests wiring entries (the
# house rule — a marker here would claim coverage without wiring):
#   standby-shape: exactly-one-serving — leader answers DATA-framed,
#     standby answers Trailers-Only Unavailable, fast (no hang).
#   burst-stability: leaseTransitions delta == 0 + holder unchanged
#     across the burst; standby posture re-verified after.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) ns nsStore;
  drvs = import ../lib/derivations.nix { inherit pkgs; };

  # Four distinct markers → four distinct drv hashes (a reused drv
  # would cache-hit and complete instantly — no burst). Short sleeps:
  # the load is Job-spawn/registration churn, not build wall time.
  burstDrvs = map (
    i:
    drvs.mkTrivial {
      marker = "standby-burst-${toString i}";
      sleepSecs = 5;
    }
  ) (pkgs.lib.range 1 4);

  # Mint an x-rio-service-token (rio_auth::hmac::ServiceClaims) for the
  # AdminService/ClusterStatus probe — c66 gated all AdminService reads
  # on ensure_service_caller (UNGATED_PUBLIC→[]). Mirrors lifecycle.nix
  # signServiceToken: key from the live rio-service-hmac Secret on
  # stdin (base64), trimmed per rio_auth::hmac::load_key.
  signServiceToken = pkgs.writeScript "sign-service-token-standby-burst" ''
    #!${pkgs.python3}/bin/python3
    import base64, hashlib, hmac, json, sys, time
    key = base64.b64decode(sys.stdin.read().strip())
    for suf in (b"\r\n", b"\n"):
        if key.endswith(suf):
            key = key[: -len(suf)]
            break
    claims = json.dumps(
        {"caller": "rio-cli", "expiry_unix": int(time.time()) + 3600},
        separators=(",", ":"),
    ).encode()
    tag = hmac.new(key, claims, hashlib.sha256).digest()
    b64 = lambda b: base64.urlsafe_b64encode(b).rstrip(b"=").decode()
    print(f"{b64(claims)}.{b64(tag)}")
  '';

  prelude = ''
    ${common.mkBootstrap {
      inherit fixture;
      withSsh = false;
    }}

    # AdminService/ClusterStatus is service-token-gated (c66
    # ensure_service_caller, allowlist rio-controller/rio-cli/
    # rio-dashboard) and the gate runs BEFORE ensure_leader — without
    # a valid token both replicas answer PermissionDenied, breaking
    # the leader DATA-frame / standby Unavailable assertions. Mint
    # once; valid for the test duration.
    service_token = k3s_server.succeed(
        "k3s kubectl -n ${ns} get secret rio-service-hmac "
        "-o jsonpath='{.data.service-hmac\\.key}' | ${signServiceToken}"
    ).strip()

    def lease_transitions():
        raw = kubectl(
            "get lease rio-scheduler-leader "
            "-o jsonpath='{.spec.leaseTransitions}'"
        ).strip()
        return int(raw or "0")

    def scheduler_pods():
        # status.phase=Running includes Terminating pods; callers here
        # only run in steady 2/2 states (waits precede every use).
        return kubectl(
            "get pods -l app.kubernetes.io/name=rio-scheduler "
            "--field-selector=status.phase=Running "
            "-o jsonpath='{.items[*].metadata.name}'"
        ).split()

    def standby_pod():
        leader = leader_pod()
        pods = scheduler_pods()
        others = [p for p in pods if p != leader]
        assert len(others) == 1, (
            f"expected exactly 1 standby (pods={pods!r}, leader={leader!r}); "
            f"scheduler.replicas != 2, or a pod is mid-restart?"
        )
        return others[0]

    def wait_sched_steady():
        # 2/2 Ready (readyReplicas: Terminating/Pending do not count)
        # AND a leader elected.
        k3s_server.wait_until_succeeds(
            "test \"$(k3s kubectl -n ${ns} get deploy rio-scheduler "
            "-o jsonpath='{.status.readyReplicas}')\" = 2",
            timeout=120,
        )
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${ns} get lease rio-scheduler-leader "
            "-o jsonpath='{.spec.holderIdentity}' | grep -q rio-scheduler",
            timeout=90,
        )

    def grpcweb_probe(pod, port):
        """Port-forward to ONE scheduler pod, POST an empty gRPC-Web
        ClusterStatus, return (body_size, first_frame_hex, headers).
        A SERVED unary carries a DATA frame (body starts 0x00, status
        in a later 0x80 trailer frame); a REFUSED unary is
        Trailers-Only — grpc-status rides the HTTP response HEADERS
        and the body is EMPTY (the ci-failure-patterns documented
        shape). Raw-file extraction (no xxd-gutter greps — xxd wraps
        at 16 bytes/line and splits tokens)."""
        body = f"/tmp/gw-{port}.bin"
        hdrs = f"/tmp/gw-{port}.hdr"
        k3s_server.succeed(
            f"k3s kubectl -n ${ns} port-forward pod/{pod} {port}:9001 "
            f">/tmp/pf-{port}.log 2>&1 & echo $! > /tmp/pf-{port}.pid"
        )
        k3s_server.wait_until_succeeds(
            f"${pkgs.netcat}/bin/nc -z localhost {port}", timeout=10
        )
        try:
            # --max-time 10: the standby must ANSWER (refusal is the
            # leader-guard, sched.grpc.leader-guard), never hang. -s
            # without -f: refusals still answer HTTP 200 — curl must
            # not treat them as transport failures.
            k3s_server.succeed(
                "printf '\\x00\\x00\\x00\\x00\\x00' | "
                "curl -s --max-time 10 -X POST "
                f"http://localhost:{port}/rio.admin.AdminService/ClusterStatus "
                "-H 'content-type: application/grpc-web+proto' "
                "-H 'x-grpc-web: 1' "
                f"-H 'x-rio-service-token: {service_token}' "
                f"--data-binary @- -D {hdrs} -o {body}"
            )
        finally:
            k3s_server.execute(
                f"kill $(cat /tmp/pf-{port}.pid) 2>/dev/null; rm -f /tmp/pf-{port}.pid"
            )
        size = int(k3s_server.succeed(f"stat -c %s {body}").strip())
        frame = k3s_server.succeed(
            f"${pkgs.xxd}/bin/xxd -l 1 -p {body} || true"
        ).strip()
        headers = k3s_server.succeed(f"cat {hdrs}")
        return size, frame, headers

    def assert_standby_shape(tag):
        leader = leader_pod()
        standby = standby_pod()
        lsize, lframe, lhdrs = grpcweb_probe(leader, 19001)
        assert lsize > 0 and lframe == "00", (
            f"[{tag}] leader {leader} did not serve ClusterStatus as a "
            f"DATA frame (body {lsize}B, first byte {lframe!r}); "
            f"headers:\n{lhdrs}"
        )
        ssize, sframe, shdrs = grpcweb_probe(standby, 19002)
        shdrs_l = shdrs.lower()
        # The documented Trailers-Only shape (sched.grpc.leader-guard
        # + the ci-failure-patterns standby family): grpc-status in
        # the HTTP HEADERS, EMPTY body. Unavailable = 14.
        assert ssize == 0, (
            f"[{tag}] standby {standby} answered with a {ssize}B body "
            f"(first byte {sframe!r}) — a DATA frame means a SECOND "
            f"replica is serving; the exactly-one-leader law is broken. "
            f"headers:\n{shdrs}"
        )
        assert "grpc-status: 14" in shdrs_l or "grpc-status:14" in shdrs_l, (
            f"[{tag}] standby refusal does not carry Unavailable in the "
            f"response headers (Trailers-Only):\n{shdrs}"
        )
        print(f"[{tag}] standby-shape OK: leader={leader} serves DATA, "
              f"standby={standby} Trailers-Only Unavailable (headers)")
  '';

  fragments = {
    standby-shape = ''
      # ══════════════════════════════════════════════════════════════════
      # standby-shape — exactly one replica serves; the other refuses fast
      # ══════════════════════════════════════════════════════════════════
      with subtest("standby-shape: leader serves, standby Trailers-Only Unavailable"):
          wait_sched_steady()
          assert_standby_shape("pre-burst")
    '';

    burst-stability = ''
      # ── prep: SSH + busybox for the client-driven build burst ─────────
      ${fixture.sshKeySetup}
      ${common.seedBusybox "k3s-server"}

      import threading

      # ══════════════════════════════════════════════════════════════════
      # burst-stability — leadership rock-solid through the scale-out burst
      # ══════════════════════════════════════════════════════════════════
      with subtest("burst-stability: zero lease transitions across the burst"):
          wait_sched_steady()
          tx_before = lease_transitions()
          holder_before = leader_pod()
          store_replicas_before = kubectl(
              "get deploy rio-store -o jsonpath='{.spec.replicas}'",
              ns="${nsStore}",
          ).strip()
          print(f"burst-stability: holder={holder_before} tx={tx_before} "
                f"store_replicas={store_replicas_before}")

          # Build burst in the background — ONE thread, CLIENT machine
          # only (Machine.succeed is not thread-safe per machine; the
          # main thread below drives k3s_server exclusively — the same
          # split leader-election's build-during-failover uses). One
          # nix-build invocation carries all four drvs.
          bg = {}
          def _bg():
              try:
                  bg["out"] = client.succeed(
                      "nix-build --no-out-link "
                      "--store 'ssh-ng://k3s-server' "
                      "--arg busybox '(builtins.storePath ${common.busybox})' "
                      "${toString burstDrvs}"
                  ).strip()
              except Exception as e:
                  bg["err"] = e
          bg_thread = threading.Thread(target=_bg, daemon=True)
          bg_thread.start()

          # The KEDA-analog actuation: scale the store fleet out. At VM
          # scale some replicas may sit Pending (2-node capacity) — the
          # burst load (apiserver churn, store registration, scheduler
          # store-probe traffic) happens regardless; do NOT gate on all
          # four becoming Ready.
          kubectl("scale deploy rio-store --replicas=4", ns="${nsStore}")
          k3s_server.wait_until_succeeds(
              "test \"$(k3s kubectl -n ${nsStore} get deploy rio-store "
              "-o jsonpath='{.spec.replicas}')\" = 4",
              timeout=30,
          )

          # Observation window: long enough for one full flap cycle of
          # the live_055(e) shape (the leader-election scenario derives
          # ~35s/cycle; 60s covers it) while the builds + scale-out
          # churn run.
          k3s_server.sleep(60)
          running_stores = k3s_server.succeed(
              "k3s kubectl -n ${nsStore} get pods -l app.kubernetes.io/name=rio-store "
              "--field-selector=status.phase=Running -o name | wc -l"
          ).strip()
          print(f"burst-stability: store pods Running mid-burst: {running_stores}")

          # Builds complete (the burst premise: real control-plane work
          # happened; generous join — Job spawn dominates).
          bg_thread.join(timeout=300)
          assert not bg_thread.is_alive(), (
              "build burst did not finish within 300s — scheduler wedged "
              "during the scale-out burst?"
          )
          if "err" in bg:
              dump_all_logs([], kube_node=k3s_server, kube_namespace="${ns}")
              raise bg["err"]
          paths = bg.get("out", "").split()
          assert len(paths) == 4 and all(
              p.startswith("/nix/store/") for p in paths
          ), f"expected 4 store paths from the burst builds, got {paths!r}"

          # Scale back down (the burst's trailing edge — live_055(e)
          # flapped on BOTH edges of the KEDA cycle).
          kubectl("scale deploy rio-store --replicas={}".format(store_replicas_before),
                  ns="${nsStore}")
          k3s_server.sleep(15)

          # THE law (W9-AI): zero leadership movement across the whole
          # burst. EXACT equality — any transition is the flap.
          tx_after = lease_transitions()
          holder_after = leader_pod()
          assert tx_after == tx_before, (
              f"leaseTransitions moved during the burst: {tx_before} → "
              f"{tx_after} (delta={tx_after - tx_before}). This is the "
              f"live_055(e) standby flap under scale-out burst."
          )
          assert holder_after == holder_before, (
              f"holderIdentity moved without a transitions bump: "
              f"{holder_before} → {holder_after}?"
          )

          # The standby posture survived the burst too.
          assert_standby_shape("post-burst")
          print(f"burst-stability PASS: holder={holder_after} stable, "
                f"tx={tx_after} (delta 0), builds={len(paths)}")
    '';
  };

  mkTest = common.mkFragmentTest {
    scenario = "standby-burst";
    inherit prelude fragments fixture;
    defaultTimeout = 900;
    chains = [
      {
        before = "standby-shape";
        after = "burst-stability";
        msg = "burst-stability re-asserts the standby shape post-burst; running the cheap pre-burst shape check first localizes a broken-at-rest posture before the expensive burst runs";
      }
    ];
  };
in
{
  inherit fragments mkTest;
}
