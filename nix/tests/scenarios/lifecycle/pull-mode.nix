# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # pull-mode — PullAssignment/ReportOutcome end-to-end on the new path
  # ══════════════════════════════════════════════════════════════════
  # REQUIRES: no live builder pods (pull-mode pods never register, so a
  # live stream-mode worker would steal the dispatch). ephemeral-pool
  # already deleted the default x86-64 Pool and waited for its own pods
  # to be gone; the precondition below re-checks the pod-level fact
  # only. A lingering scheduler-side executor ENTRY (stream ghost whose
  # pod is gone) cannot steal work — a dispatch to it fails and the drv
  # requeues — so the precondition deliberately does NOT gate on the
  # workers_active gauge.
  #
  # The pool opts in via the `rio.build/dispatch-mode: pull` annotation
  # (the interim selector for the additive slice — the controller
  # injects RIO_DISPATCH_MODE=pull into its executor pods; the
  # first-class PoolSpec dispatchMode field lands at 1b).
  #
  # Open-attempt observability used below: the ledger view via psql
  # (the same join ListOpenAttempts serves, cheap enough to poll) plus
  # one ListOpenAttempts RPC call per arm for the service-token-gated
  # OA5 surface itself.
  #
  # Proves end-to-end (the 1a gate items):
  #   - a never-pulled pod death charges nothing: no attempt row exists
  #     for it and the same derivation later builds cleanly
  #     (sched.attempt.no-attempt-no-op);
  #   - the respawned pod pulls and builds the drv on the new path
  #     (sched.executor.pull-transaction), exactly one open attempt is
  #     visible in ListOpenAttempts while the build runs, and the
  #     report lands (the pod exits 0, the client gets its store path,
  #     the assignment closes, and no drv_attempts charge row exists —
  #     the ledger records failures, not successes);
  #   - a pod killed mid-build does NOT fabricate a completion or a
  #     premature classification: its attempt stays open (the
  #     establishment sweep — deadline + report slack, ~1 h with the
  #     vmtest probe deadline — is the only closer and is exercised at
  #     the unit level and in the 1b canary scenario, not here).
  with subtest("pull-mode: pull/report path builds a drv; no-attempt and killed-mid-build arms"):
      import time

      kubectl(
          "delete pool x86-64 --ignore-not-found --wait=true",
          ns="${nsBuilders}",
      )
      # Pod-level precondition only (see banner comment).
      k3s_server.wait_until_succeeds(
          "! k3s kubectl -n ${nsBuilders} get pods --no-headers 2>/dev/null | grep -q .",
          timeout=120,
      )

      # ── Pull-mode pool (annotation opt-in) ────────────────────────
      k3s_server.succeed(
          "k3s kubectl apply -f - <<'EOF'\n"
          "apiVersion: rio.build/v1alpha1\n"
          "kind: Pool\n"
          "metadata:\n"
          "  name: pull-pool\n"
          "  namespace: ${nsBuilders}\n"
          "  annotations:\n"
          "    rio.build/dispatch-mode: pull\n"
          "spec:\n"
          "  kind: Builder\n"
          "  maxConcurrent: 4\n"
          "  systems: [x86_64-linux]\n"
          "  image: rio-builder:dev\n"
          "  imagePullPolicy: Never\n"
          "  privileged: true\n"
          "  terminationGracePeriodSeconds: 60\n"
          # Arm 1 determinism: an unsatisfiable nodeSelector keeps the
          # first pod Pending (provably pre-pull) so the never-pulled
          # death is not a race against the pull; arm 2 patches it away.
          "  nodeSelector:\n"
          "    rio.build/never-schedule: \"true\"\n"
          "  tolerations: null\n"
          "EOF"
      )

      def open_pull_count(marker=""):
          """Open pull-mode attempts in the ledger (the view
          ListOpenAttempts serves): active assignment ⋈ execution with
          dispatch_mode='pull' and no terminal drv_attempts fill."""
          like = f"AND d.drv_path LIKE '%{marker}%' " if marker else ""
          return int(psql_k8s(k3s_server,
              "SELECT count(*) FROM assignments a "
              "JOIN drv_executions e ON e.exec_id = a.exec_id "
              "JOIN derivations d ON d.derivation_id = a.derivation_id "
              "WHERE a.status IN ('pending','acknowledged') "
              "AND e.dispatch_mode = 'pull' "
              f"{like}"
              "AND NOT EXISTS (SELECT 1 FROM drv_attempts t "
              " WHERE t.exec_id = a.exec_id "
              " AND t.termination_reason IS NOT NULL)"
          ).strip() or "0")

      def attempt_rows(marker):
          """All drv_attempts rows ever recorded for the marker drv."""
          return int(psql_k8s(k3s_server,
              "SELECT count(*) FROM drv_attempts a "
              "JOIN derivations d ON d.derivation_id = a.derivation_id "
              f"WHERE d.drv_path LIKE '%{marker}%'"
          ).strip() or "0")

      def wait_open_pull(marker, want, timeout=240, ctx=""):
          """Poll the ledger until the open pull-attempt count for
          marker equals want."""
          deadline = time.time() + timeout
          count = -1
          while time.time() < deadline:
              count = open_pull_count(marker)
              if count == want:
                  return
              time.sleep(3)
          raise AssertionError(
              f"pull-mode{ctx}: open pull attempts for {marker!r} stuck at "
              f"{count}, wanted {want} within {timeout}s"
          )

      def open_attempts_rpc():
          """ListOpenAttempts via the service-token-gated AdminService
          (the pull-filtered OA5 surface). Returns the attempts list."""
          out = sched_grpc("{}", "rio.admin.AdminService/ListOpenAttempts")
          msgs = grpcurl_json_stream(out)
          assert msgs, f"ListOpenAttempts returned no JSON: {out[:300]!r}"
          return msgs[0].get("attempts", [])

      # ── Arm 1: never-pulled pod death charges nothing ─────────────
      # Submit the build (background — it only completes in arm 2).
      # The pool's unsatisfiable nodeSelector keeps the spawned pod
      # Pending, so it provably never pulls; its death is then the
      # never-pulled case by construction, not by racing the pull.
      client.succeed(
          "nix-build --no-out-link --store 'ssh-ng://k3s-server' "
          "--arg busybox '(builtins.storePath ${common.busybox})' "
          "${pullDrv1} > /tmp/pull1.out 2>&1 & "
          "echo $! > /tmp/pull1.pid"
      )
      k3s_server.wait_until_succeeds(
          "test -n \"$(k3s kubectl -n ${nsBuilders} get pods "
          "-l rio.build/pool=pull-pool -o name)\"",
          timeout=120,
      )
      victim = k3s_server.succeed(
          "k3s kubectl -n ${nsBuilders} get pods "
          "-l rio.build/pool=pull-pool "
          "-o jsonpath='{.items[0].metadata.name}'"
      ).strip()
      victim_job = k3s_server.succeed(
          "k3s kubectl -n ${nsBuilders} get jobs "
          "-l rio.build/pool=pull-pool "
          "-o jsonpath='{.items[0].metadata.name}'"
      ).strip()
      # Provably pre-pull: the pod is Pending (unschedulable), so the
      # ledger has no open pull-mode attempt, no classification row,
      # and the pull-filtered RPC surface is empty. These checks are
      # deterministic HERE (no schedulable pull pod exists anywhere);
      # after the kill the controller respawns and the fresh pod may
      # pull within seconds, so post-kill emptiness is not asserted —
      # the never-pulled death's "charged nothing" is instead proven by
      # arm 2 completing the same drv with exactly one attempt row.
      phase = k3s_server.succeed(
          f"k3s kubectl -n ${nsBuilders} get pod {victim} "
          "-o jsonpath='{.status.phase}'"
      ).strip()
      assert phase == "Pending", (
          f"the unsatisfiable nodeSelector must keep the first pod Pending, got {phase!r}"
      )
      assert open_pull_count() == 0, "a Pending (never-pulled) pod must have no open attempt"
      assert attempt_rows("lifecycle-pull-mode-1") == 0, (
          "a Pending (never-pulled) pod must have no attempt row"
      )
      rpc_attempts = open_attempts_rpc()
      assert rpc_attempts == [], (
          f"ListOpenAttempts must be empty while the only pull pod is Pending, got: {rpc_attempts!r}"
      )
      # Make FUTURE pods schedulable before the kill so the controller's
      # respawn (which can land on its next tick, before arm 2 begins)
      # already uses the patched template; the existing pod's spec is
      # immutable so it stays Pending until killed.
      k3s_server.succeed(
          "k3s kubectl -n ${nsBuilders} patch pool pull-pool --type=merge "
          "-p '{\"spec\":{\"nodeSelector\":null}}'"
      )
      print(f"pull-mode arm 1: killing never-pulled (Pending) pod {victim} / job {victim_job}")
      k3s_server.succeed(
          f"k3s kubectl -n ${nsBuilders} delete pod {victim} "
          "--force --grace-period=0 --wait=false"
      )
      k3s_server.succeed(
          f"k3s kubectl -n ${nsBuilders} delete job {victim_job} --wait=false"
      )

      # ── Arm 2: the respawned pod pulls, builds, reports ───────────
      # The controller respawns the Job for the still-queued intent
      # with the patched (schedulable) template; that pod pulls the
      # drv and an open pull-mode attempt appears. The arm-1 death
      # contributed nothing: the attempt-row count asserted at the end
      # of this arm is exactly one (the successful worker report).
      wait_open_pull("lifecycle-pull-mode-1", 1, timeout=300, ctx=" arm 2")
      rpc_attempts = open_attempts_rpc()
      assert len(rpc_attempts) == 1, (
          f"exactly one open pull-mode attempt expected during the build, got: {rpc_attempts!r}"
      )
      assert "lifecycle-pull-mode-1" in rpc_attempts[0].get("derivation", ""), (
          f"the open attempt must be for the pull-mode drv, got: {rpc_attempts[0]!r}"
      )
      arm2_exec = rpc_attempts[0].get("execId", "")
      assert arm2_exec, f"open attempt carries an exec_id, got: {rpc_attempts[0]!r}"
      print(f"pull-mode arm 2: open attempt visible via ListOpenAttempts (exec_id {arm2_exec})")

      # The report lands: the client's nix-build exits with the store
      # path (the drv completed on the pull path) …
      client.wait_until_succeeds(
          "! kill -0 $(cat /tmp/pull1.pid) 2>/dev/null",
          timeout=300,
      )
      out1 = client.succeed("cat /tmp/pull1.out").strip()
      assert "/nix/store/" in out1, (
          f"pull-mode build should have produced a store path, got: {out1!r}"
      )
      # … the attempt closes …
      wait_open_pull("lifecycle-pull-mode-1", 0, timeout=120, ctx=" arm 2 close")
      # … the assignment minted by the pull is closed by the report …
      closed = int(psql_k8s(k3s_server,
          "SELECT count(*) FROM assignments a "
          "JOIN derivations d ON d.derivation_id = a.derivation_id "
          "WHERE d.drv_path LIKE '%lifecycle-pull-mode-1%' "
          "AND a.status NOT IN ('pending','acknowledged')"
      ).strip() or "0")
      assert closed == 1, (
          f"the pull-mode drv's assignment should be closed after the report, got: {closed}"
      )
      # … and the attempt ledger holds NO charge rows for the drv: the
      # never-pulled death was charge-free (the no-attempt rule) and a
      # successful pull build appends nothing to drv_attempts (the
      # ledger records failures/charges/resets, never successes).
      rows = attempt_rows("lifecycle-pull-mode-1")
      assert rows == 0, (
          f"no drv_attempts charge rows expected for a cleanly built pull-mode drv, got: {rows}"
      )
      print("pull-mode arm 2: build complete, report landed, assignment closed, no charges")

      # ── Arm 3: a pod killed mid-build fabricates nothing ──────────
      client.succeed(
          "nix-build --no-out-link --store 'ssh-ng://k3s-server' "
          "--arg busybox '(builtins.storePath ${common.busybox})' "
          "${pullDrv2} > /tmp/pull2.out 2>&1 & "
          "echo $! > /tmp/pull2.pid"
      )
      wait_open_pull("lifecycle-pull-mode-2", 1, timeout=300, ctx=" arm 3")
      rpc_attempts = open_attempts_rpc()
      assert len(rpc_attempts) == 1 and "lifecycle-pull-mode-2" in rpc_attempts[0].get(
          "derivation", ""
      ), f"expected the pull-mode-2 attempt mid-build, got: {rpc_attempts!r}"
      arm3_exec = rpc_attempts[0].get("execId", "")
      builder = k3s_server.succeed(
          "k3s kubectl -n ${nsBuilders} get pods "
          "-l rio.build/pool=pull-pool "
          "--field-selector=status.phase=Running "
          "-o jsonpath='{.items[*].metadata.name}' 2>/dev/null || true"
      ).split()
      assert builder, "a Running pull-pool pod expected mid-build"
      k3s_server.succeed(
          f"k3s kubectl -n ${nsBuilders} delete pod {builder[0]} "
          "--force --grace-period=0 --wait=false"
      )
      # The kill must not fabricate a completion or classification: the
      # attempt stays open with the same exec_id (the establishment
      # sweep, after deadline + slack, is the only closer — outside
      # this subtest's budget) and no attempt row appears.
      time.sleep(15)
      rpc_attempts = open_attempts_rpc()
      assert len(rpc_attempts) == 1 and rpc_attempts[0].get("execId", "") == arm3_exec, (
          f"the killed-mid-build attempt must stay open with the same exec_id "
          f"({arm3_exec}), got: {rpc_attempts!r}"
      )
      assert attempt_rows("lifecycle-pull-mode-2") == 0, (
          "no classification may exist right after the mid-build kill"
      )
      print("pull-mode arm 3: killed mid-build, attempt stays open, nothing fabricated")

      # ── Cleanup ───────────────────────────────────────────────────
      # The arm-3 client is still waiting on a build that will only
      # resolve via the establishment sweep — kill it; the drv's open
      # attempt intentionally remains (documented above). Delete the
      # pool; ownerRef GC removes its Jobs/pods.
      client.succeed("kill $(cat /tmp/pull2.pid) 2>/dev/null || true")
      kubectl("delete pool pull-pool --wait=false", ns="${nsBuilders}")
      k3s_server.wait_until_succeeds(
          "! k3s kubectl -n ${nsBuilders} get pool pull-pool 2>/dev/null",
          timeout=30,
      )
      k3s_server.wait_until_succeeds(
          "! k3s kubectl -n ${nsBuilders} get pods "
          "-l rio.build/pool=pull-pool "
          "--no-headers 2>/dev/null | grep -q .",
          timeout=120,
      )
      print("pull-mode PASS: no-attempt death charge-free, pull build + report "
            "end-to-end, killed-mid-build attempt stays open")
''
