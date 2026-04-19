# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # health-shared — standby NOT_SERVING, leader SERVING on gRPC port
  # ══════════════════════════════════════════════════════════════════
  # Ports phase3b section T (phase3b.nix:342-360 + 506-520) onto the
  # k3s-full fixture. In phase3b the standby window was ARTIFICIAL
  # (scheduler in STANDBY because kubeconfig didn't exist yet). Here
  # the standby is REAL: scheduler.replicas=2, one pod holds the Lease,
  # the other is a live standby.
  #
  # Proves: the lease-gated set_not_serving on the NAMED service is
  # observable on the main gRPC port (9001). Pre-Cilium this probed a
  # separate plaintext health port 9101 (mTLS on 9001 rejected probes
  # without a client cert). With WireGuard transport encryption (D2)
  # 9001 is plaintext, the dedicated health port is gone, and the
  # single tonic-health server is what both k8s probes and the
  # client-side balancer hit.
  #
  # MUST run BEFORE recovery: recovery deletes the leader pod, which
  # disrupts the stable 2-replica state (the former standby becomes
  # leader, and the Deployment-spawned replacement pod may briefly
  # show SERVING-then-NOT_SERVING churn during its own startup).
  with subtest("health-shared: standby NOT_SERVING, leader SERVING (shared HealthReporter)"):
      # app.kubernetes.io/name=rio-scheduler is the Deployment's pod
      # selector (templates/_helpers.tpl rio.selectorLabels).
      all_sched = kubectl(
          "get pods -l app.kubernetes.io/name=rio-scheduler "
          "-o jsonpath='{.items[*].metadata.name}'"
      ).split()
      assert len(all_sched) == 2, (
          f"expected exactly 2 scheduler pods (replicas=2), got: {all_sched}"
      )
      leader = leader_pod()
      standby = next(p for p in all_sched if p != leader)
      print(f"health-shared: leader={leader}, standby={standby}")

      # ── STANDBY: NOT_SERVING on named service ─────────────────────
      # grpc-health-probe exits 1 for NOT_SERVING (phase3b.nix:348).
      # .fail() expects non-zero exit AND returns stdout+stderr.
      # Probe the NAMED service (rio.scheduler.SchedulerService), NOT
      # the empty-string default. scheduler/main.rs at
      # r[impl ctrl.probe.named-service]: set_not_serving only
      # affects the named service; empty-string stays SERVING forever
      # after the first set_serving. This proves the CLIENT-SIDE
      # BALANCER constraint (the K8s readinessProbe is tcpSocket —
      # it doesn't probe gRPC health). This probe with `-service ...`
      # AND the NOT_SERVING result together prove the named-service
      # gate.
      pf_open(standby, 19101, 9001, tag="pf-health")
      try:
          out = k3s_server.fail(
              "grpc-health-probe -addr localhost:19101 "
              "-service rio.scheduler.SchedulerService 2>&1"
          )
          assert "NOT_SERVING" in out, (
              f"standby gRPC health should report NOT_SERVING "
              f"(lease-gated named service), got: {out!r}"
          )
      finally:
          pf_close(tag="pf-health")

      # ── LEADER: SERVING on named service ──────────────────────────
      # Exit 0 = SERVING. Leader acquired lease during waitReady →
      # LeaderAcquired → recover_from_pg → recovery_complete=true →
      # set_serving on named service. DISTINCT local port (19102,
      # not 19101): the kill above leaves :19101 in TIME_WAIT for
      # ~60s, and port-forward without SO_REUSEADDR can't rebind —
      # it dies silently (stderr→/dev/null), probe gets conn-refused
      # → exit 2. sleep 2 doesn't help; TIME_WAIT outlasts it.
      pf_open(leader, 19102, 9001, tag="pf-health")
      try:
          k3s_server.succeed(
              "grpc-health-probe -addr localhost:19102 "
              "-service rio.scheduler.SchedulerService"
          )
      finally:
          pf_close(tag="pf-health")
      print("health-shared PASS: standby NOT_SERVING, leader SERVING "
            "(lease-gated named service on main gRPC port)")

  # ══════════════════════════════════════════════════════════════════
  # controller HTTP health: liveness bound BEFORE connect_forever,
  # readiness gated ON it (ctrl.health.ready-gates-connect)
  # ══════════════════════════════════════════════════════════════════
  # Regression: pre-fix the health server spawned AFTER
  # connect_forever, so a >~20s scheduler cold-start = 3× connection-
  # refused on livenessProbe = SIGTERM = CrashLoopBackOff. The chart
  # also pointed readinessProbe at /healthz (always-200), so readiness
  # never reflected scheduler reachability.
  #
  # Structural assertions (no scheduler restart needed — that would
  # disrupt the stable 2-replica state for downstream subtests):
  #   1. readinessProbe.path == /readyz  (was /healthz pre-fix)
  #   2. /healthz and /readyz both 200 in steady state
  #   3. controller Running (not CrashLoopBackOff) — proves the health
  #      server bound during the cold-start that already happened at
  #      waitReady
  with subtest("health-shared: controller /healthz live, /readyz gated on scheduler"):
      ctrl_pod = kubectl(
          "get pod -l app.kubernetes.io/name=rio-controller "
          "-o jsonpath='{.items[0].metadata.name}'"
      )
      ready_path = kubectl(
          "get pod " + ctrl_pod + " -o "
          "jsonpath='{.spec.containers[0].readinessProbe.httpGet.path}'"
      )
      assert ready_path == "/readyz", (
          f"readinessProbe must target /readyz (gated on scheduler "
          f"connect), got {ready_path!r}"
      )
      restarts = kubectl(
          "get pod " + ctrl_pod + " -o "
          "jsonpath='{.status.containerStatuses[0].restartCount}'"
      )
      assert restarts == "0", (
          f"controller restarted {restarts}× during cold-start "
          f"(livenessProbe killed it before health server bound?)"
      )
      # --retry-connrefused: pf_open's nc-probe proves the LOCAL bind
      # is up, but kubectl port-forward's upstream dial can lag a few
      # hundred ms (and cilium interface churn briefly drops it).
      # --fail: non-2xx → exit 22 → succeed() raises.
      pf_open(ctrl_pod, 19194, 9194, tag="pf-ctrl-health")
      try:
          for path in ("/healthz", "/readyz"):
              k3s_server.succeed(
                  "curl -sS --fail --retry 5 --retry-connrefused "
                  f"--retry-delay 1 http://localhost:19194{path}"
              )
      finally:
          pf_close(tag="pf-ctrl-health")
      print("health-shared PASS: controller readinessProbe=/readyz, "
            "0 restarts, /healthz+/readyz=200")
''
