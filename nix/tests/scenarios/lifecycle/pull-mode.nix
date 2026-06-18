# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # pull-mode — PullAssignment/ReportOutcome end-to-end on the new path
  # ══════════════════════════════════════════════════════════════════
  # REQUIRES: no live builder pods. ephemeral-pool already deleted the
  # default x86-64 Pool and waited for its own pods to be gone; the
  # precondition below re-checks the pod-level fact only.
  #
  # Pull is the only dispatch protocol (the dispatchMode knob is
  # retired): the controller renders the AD5 abort grace on every
  # executor pod, and the pool needs no opt-in field.
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
  #   - a pod killed mid-build is not lost and is not charged (AD5: a
  #     platform termination of still-wanted work is an abort, not a
  #     worker fault): the builder's SIGTERM-abort report closes the
  #     attempt promptly with exactly one uncharged terminal row
  #     (disconnected/worker_abort) instead of waiting for the
  #     establishment sweep, the derivation requeues at that fold, a
  #     fresh attempt (new exec_id) rebuilds it, and the same client
  #     build still gets its store path. The successful re-run appends
  #     nothing to the ledger; the establishment sweep (deadline +
  #     report slack) remains the out-of-budget backstop — and the
  #     only charging path — for attempts no observer reports.
  with subtest("pull-mode: pull/report path builds a drv; no-attempt and killed-mid-build arms"):
      import time

      kubectl(
          "delete pool x86-64 --ignore-not-found --wait=true",
          ns="${nsBuilders}",
      )
      # Pod-level precondition only (see banner comment). Filter on
      # the rio.build/pool label: ADR-022 lands the rio-mountd
      # DaemonSet in this namespace, so an unfiltered wait never sees
      # an empty namespace.
      k3s_server.wait_until_succeeds(
          "! k3s kubectl -n ${nsBuilders} get pods -l rio.build/pool "
          "--no-headers 2>/dev/null | grep -q .",
          timeout=120,
      )

      # ── Pull pool ──────────────────────────────────────────────────
      k3s_server.succeed(
          "k3s kubectl apply -f - <<'EOF'\n"
          "apiVersion: rio.build/v1alpha1\n"
          "kind: Pool\n"
          "metadata:\n"
          "  name: pull-pool\n"
          "  namespace: ${nsBuilders}\n"
          "spec:\n"
          "  kind: Builder\n"
          "  maxConcurrent: 4\n"
          "  systems: [x86_64-linux]\n"
          "  image: rio-builder:dev\n"
          "  imagePullPolicy: Never\n"
          "  privileged: true\n"
          # Arm 1 determinism: an unsatisfiable nodeSelector keeps the
          # first pod Pending (provably pre-pull) so the never-pulled
          # death is not a race against the pull; arm 2 patches it away.
          "  nodeSelector:\n"
          "    rio.build/never-schedule: \"true\"\n"
          "  tolerations: null\n"
          "EOF"
      )

      def open_pull_count(marker=""):
          """Open attempts in the ledger (the view ListOpenAttempts
          serves): active assignment ⋈ execution with no terminal
          drv_attempts fill."""
          like = f"AND d.drv_path LIKE '%{marker}%' " if marker else ""
          return int(psql_k8s(k3s_server,
              "SELECT count(*) FROM assignments a "
              "JOIN drv_executions e ON e.exec_id = a.exec_id "
              "JOIN derivations d ON d.derivation_id = a.derivation_id "
              "WHERE a.status IN ('pending','acknowledged') "
              f"{like}"
              "AND NOT EXISTS (SELECT 1 FROM drv_attempts t "
              " WHERE t.exec_id = a.exec_id "
              " AND t.termination_reason IS NOT NULL)"
          ).strip() or "0")

      def open_pull_exec(marker):
          """exec_id of the currently-open pull attempt for marker
          (empty string when none) — the same view open_pull_count
          reads, narrowed to its exec_id column."""
          return psql_k8s(k3s_server,
              "SELECT a.exec_id FROM assignments a "
              "JOIN drv_executions e ON e.exec_id = a.exec_id "
              "JOIN derivations d ON d.derivation_id = a.derivation_id "
              "WHERE a.status IN ('pending','acknowledged') "
              f"AND d.drv_path LIKE '%{marker}%' "
              "AND NOT EXISTS (SELECT 1 FROM drv_attempts t "
              " WHERE t.exec_id = a.exec_id "
              " AND t.termination_reason IS NOT NULL)"
          ).strip()

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

      # ── Arm 3: a pod killed mid-build is closed once + requeued ──
      # Wave 1a wrote this arm against the interim state (nothing
      # reported pod-terminal outcomes for pull-mode pods, so a killed
      # attempt stayed open under the same exec_id until the
      # establishment sweep). The pull-hardening batch aligned the
      # in-budget closer with AD5: the builder's SIGTERM-abort report
      # of still-wanted work (fired as the force-kill lands in this
      # fixture) closes the attempt promptly and CHARGE-FREE — exactly
      # one uncharged terminal row (disconnected/worker_abort), never
      # an infrastructure-failure charge — and the derivation requeues
      # at that fold. The establishment sweep remains the out-of-budget
      # backstop (and the only path that charges) when no abort report
      # lands. This arm asserts the strongest in-budget form of that
      # contract: original exec closed with exactly one uncharged
      # terminal row, never a fabricated success, a fresh attempt
      # appears, and the SAME client build still completes with a store
      # path (one extra 45 s rebuild, well inside the group budget).
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
      assert arm3_exec, f"open attempt carries an exec_id, got: {rpc_attempts[0]!r}"
      builder = k3s_server.succeed(
          "k3s kubectl -n ${nsBuilders} get pods "
          "-l rio.build/pool=pull-pool "
          "--field-selector=status.phase=Running "
          "-o jsonpath='{.items[*].metadata.name}' 2>/dev/null || true"
      ).split()
      assert builder, "a Running pull-pool pod expected mid-build"
      print(f"pull-mode arm 3: force-killing mid-build pod {builder[0]} (exec_id {arm3_exec})")
      k3s_server.succeed(
          f"k3s kubectl -n ${nsBuilders} delete pod {builder[0]} "
          "--force --grace-period=0 --wait=false"
      )

      # The killed attempt must be superseded: the original exec_id
      # leaves the open view and a FRESH attempt (different exec_id)
      # appears once the requeued drv is re-pulled. Poll structurally
      # (no fixed sleep): the close-to-re-pull window is seconds on
      # KVM, budgeted generously for TCG. The open view holds at most
      # one attempt per derivation, so a fresh exec_id here also
      # proves the killed one is no longer open.
      deadline = time.time() + 120
      arm3_exec2 = ""
      while time.time() < deadline:
          arm3_exec2 = open_pull_exec("lifecycle-pull-mode-2")
          if arm3_exec2 and arm3_exec2 != arm3_exec:
              break
          time.sleep(3)
      assert arm3_exec2 and arm3_exec2 != arm3_exec, (
          f"killed-mid-build attempt {arm3_exec} should be closed and the "
          f"requeued drv re-pulled under a fresh exec_id, open view shows "
          f"{arm3_exec2!r}"
      )
      rpc_attempts = open_attempts_rpc()
      assert len(rpc_attempts) == 1 and rpc_attempts[0].get("execId", "") == arm3_exec2, (
          f"ListOpenAttempts must show exactly the fresh attempt ({arm3_exec2}), "
          f"got: {rpc_attempts!r}"
      )
      # The killed exec is closed with exactly one terminal row and the
      # row is the UNCHARGED abort class (the ledger has no success
      # class at all, so a row here can never launder the kill into a
      # completion). The in-budget closer is the worker SIGTERM-abort
      # report of still-wanted work, which AD5 resolves charge-free as
      # disconnected/worker_abort — never an infra charge; the
      # establishment sweep's executor_crash spelling only appears when
      # no abort report lands, which is structurally impossible inside
      # this arm's 120 s poll budget (the window is deadline + 120 s
      # slack).
      orig_rows = int(psql_k8s(k3s_server,
          f"SELECT count(*) FROM drv_attempts WHERE exec_id = '{arm3_exec}'"
      ).strip() or "0")
      orig_class = psql_k8s(k3s_server,
          f"SELECT outcome_class FROM drv_attempts WHERE exec_id = '{arm3_exec}'"
      ).strip()
      orig_reason = psql_k8s(k3s_server,
          "SELECT coalesce(termination_reason, '<none>') FROM drv_attempts "
          f"WHERE exec_id = '{arm3_exec}'"
      ).strip()
      assert orig_rows == 1 and orig_class == "disconnected", (
          f"the killed attempt must be closed exactly once with the uncharged "
          f"abort class, got {orig_rows} row(s), class {orig_class!r}"
      )
      assert orig_reason == "worker_abort", (
          f"the in-budget closer is the AD5 SIGTERM-abort report, "
          f"got termination_reason {orig_reason!r}"
      )
      # Never a fabricated success: a legitimate completion needs the
      # fresh attempt to run its full 45 s build, so the client must
      # still be waiting at this point.
      still_running = client.execute("kill -0 $(cat /tmp/pull2.pid)")[0]
      assert still_running == 0, (
          f"the arm-3 client build finished suspiciously early (kill -0 rc "
          f"{still_running}); only a fabricated completion could land before "
          f"the re-attempt has built"
      )
      print(f"pull-mode arm 3: killed exec closed uncharged ({orig_class}/{orig_reason}), "
            f"fresh attempt {arm3_exec2}")

      # The derivation is not lost: the fresh attempt builds it, the
      # report lands, and the same client nix-build exits with the
      # store path.
      client.wait_until_succeeds(
          "! kill -0 $(cat /tmp/pull2.pid) 2>/dev/null",
          timeout=300,
      )
      out2 = client.succeed("cat /tmp/pull2.out").strip()
      assert "/nix/store/" in out2, (
          f"the requeued pull-mode drv should still produce a store path, got: {out2!r}"
      )
      # … the fresh attempt closes on the successful report …
      wait_open_pull("lifecycle-pull-mode-2", 0, timeout=120, ctx=" arm 3 close")
      active2 = int(psql_k8s(k3s_server,
          "SELECT count(*) FROM assignments a "
          "JOIN derivations d ON d.derivation_id = a.derivation_id "
          "WHERE d.drv_path LIKE '%lifecycle-pull-mode-2%' "
          "AND a.status IN ('pending','acknowledged')"
      ).strip() or "0")
      assert active2 == 0, (
          f"no active assignment may remain for the rebuilt drv, got: {active2}"
      )
      # … and at quiescence the ledger holds exactly the one uncharged
      # abort row: the kill closed the killed exec once (charge-free)
      # and the successful re-attempt appended nothing (the ledger
      # records failures and closures, never successes).
      rows2 = attempt_rows("lifecycle-pull-mode-2")
      new_rows = int(psql_k8s(k3s_server,
          f"SELECT count(*) FROM drv_attempts WHERE exec_id = '{arm3_exec2}'"
      ).strip() or "0")
      assert rows2 == 1 and new_rows == 0, (
          f"expected exactly one terminal row (the killed attempt's abort close) "
          f"and none for the successful re-attempt, got: drv-wide {rows2}, "
          f"fresh-exec {new_rows}"
      )
      print("pull-mode arm 3: requeued drv rebuilt under the fresh exec_id, "
            "store path delivered, single uncharged abort row in the ledger")

      # ── Cleanup ───────────────────────────────────────────────────
      # Both clients have exited and no open attempt remains. Delete
      # the pool; ownerRef GC removes its Jobs/pods.
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
            "end-to-end, killed-mid-build closed uncharged and rebuilt to success")
''
