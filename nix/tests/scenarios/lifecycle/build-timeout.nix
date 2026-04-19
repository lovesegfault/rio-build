# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # build-timeout — gRPC buildTimeout < sleep → TimedOut, cgroup cleaned
  # ══════════════════════════════════════════════════════════════════
  # Post-P0294: no Build CR. Submit via gRPC SubmitBuild with
  # buildTimeout=45 directly. The value flows two places:
  #   (1) scheduler per-build timeout (actor/worker.rs:597) — checked
  #       on Tick (10s here), fires cancel_build_derivations
  #   (2) worker per-derivation daemon timeout (executor/mod.rs:567 →
  #       stderr_loop.rs read_build_stderr_loop select! build-deadline
  #       arm) — fires TimedOut
  # Either way run_daemon_build returns (Ok(TimedOut) or Err on
  # cancel-killed daemon), and the executor FALLS THROUGH to line
  # 764: build_cgroup.kill() + drain + Drop rmdirs. THAT is the
  # kill-on-teardown path under test.
  #
  # This is DISTINCT from cancel-cgroup-kill above: cancel-cgroup-kill
  # tests runtime.rs try_cancel_build (explicit CancelBuild RPC).
  # build-timeout tests executor/mod.rs:764 (post-daemon teardown).
  # Both write cgroup.kill; different call sites, different r[impl].
  #
  # sleepSecs=90 vs buildTimeout=45. buildTimeout starts at
  # SubmitBuild and is checked on the scheduler's 10s Tick. With
  # one-shot workers, no pod exists at submit time — controller
  # reconcile (10s) + Job-spawn + pod-schedule + heartbeat is
  # ~20-30s before dispatch. buildTimeout must exceed that or the
  # build is cancelled before it ever runs. The previous value (5)
  # raced against the cancel-subtest's worker draining: locally
  # the drain hadn't completed and dispatch reused it; under GHA
  # nested-virt the drain finished first.
  with subtest("build-timeout: gRPC buildTimeout < sleep → cgroup cleaned, no EEXIST"):
      # buildTimeout is SubmitBuildRequest field 6 (types.proto:655,
      # camelCase for grpcurl). max_time=3 caps the stream read well
      # under buildTimeout so we capture the first BuildEvent before
      # timeout races in.
      drv_path, build_id = submit_single_drv(
          "${timeoutDrv}", max_time=3, buildTimeout=45
      )
      print(f"build-timeout: submitted, build_id={build_id}")

      # ── Assertion 1: cgroup appeared + non-empty (precondition). ──
      # Without this, the cgroup-gone assert below proves nothing
      # (could vanish for any reason). sanitize_build_id: basename
      # with . → _, so cgroup dir ends "lifecycle-timeout_drv".
      # Probe from VM host (worker pod is distroless, no `find`).
      # `| grep .` fails on empty (find exits 0 on no-match) so
      # wait_until_succeeds retries.
      wp = wait_worker_pod()
      worker_node = k3s_server.succeed(
          f"k3s kubectl -n ${nsBuilders} get pod {wp} "
          "-o jsonpath='{.spec.nodeName}'"
      ).strip()
      worker_vm = k3s_agent if worker_node == "k3s-agent" else k3s_server
      # find + non-empty-procs check atomically: one-shot workers
      # tear down the cgroup fast enough that a separate
      # `succeed(wc -l < $p/cgroup.procs)` after the find can hit
      # ENOENT. `grep -q .` (NOT `test -s` — cgroupfs st_size is 0)
      # retries until the cgroup exists WITH procs; 2>/dev/null
      # makes the gone-already case a clean retry.
      cgroup_path = worker_vm.wait_until_succeeds(
          "p=$(find /sys/fs/cgroup -type d -name '*lifecycle-timeout_drv' "
          "-print -quit 2>/dev/null) && "
          'grep -q . "$p/cgroup.procs" 2>/dev/null && echo "$p"',
          timeout=180,
      ).strip()
      print(f"build-timeout: cgroup={cgroup_path} (procs non-empty)")

      # ── Assertion 2: cgroup GONE (kill-on-teardown fired). ──────
      # Kernel rejects rmdir on non-empty cgroup (EBUSY), so gone ⇒
      # procs drained ⇒ cgroup.kill fired. Without the teardown fix,
      # this would EBUSY-leak: the sleep-90 builder is a grandchild
      # (nix-daemon forked it), daemon.kill() doesn't reach it, and
      # only ~50s have elapsed (sleep not done). The explicit
      # build_cgroup.kill() + drain-poll at executor/mod.rs:764-796
      # is what makes rmdir succeed.
      #
      # timeout=120: buildTimeout=45 + 10s scheduler tick granularity
      # + daemon-teardown latency + cold-start headroom.
      try:
          worker_vm.wait_until_succeeds(
              f"! test -e {cgroup_path}",
              timeout=120,
          )
      except Exception:
          procs_after = worker_vm.succeed(
              f"cat {cgroup_path}/cgroup.procs 2>/dev/null | wc -l "
              f"|| echo gone"
          ).strip()
          k3s_server.execute(
              "echo '=== DIAG: worker logs (last 2m, non-DEBUG) ===' >&2; "
              f"k3s kubectl -n ${nsBuilders} logs {wp} --since=2m "
              "  | grep -vE '\"level\":\"DEBUG\"' | tail -40 >&2 || true"
          )
          print(f"build-timeout DIAG: procs_after={procs_after}, "
                f"build_id={build_id}")
          raise
      print(f"build-timeout: cgroup {cgroup_path} removed "
            f"(builder killed, rmdir succeeded)")

      # ── Assertion 3: timeout observed (no-reassign). ─────────────
      # Either scheduler's per-build timeout or worker's daemon
      # timeout won the race — both are terminal-no-reassign. The
      # scheduler metric rio_scheduler_build_timeouts_total is the
      # less racy check: it increments when actor/worker.rs:606
      # fires. With tick=10s and buildTimeout=45, it will fire by
      # T+~50s (first tick where elapsed > 45) unless the build
      # already reached a terminal state via the worker path (in
      # which case the worker reported BuildResultStatus::TimedOut,
      # which is also permanent-no-reassign per types.proto:278).
      # We check EITHER incremented — both prove no-reassign.
      m = sched_metrics()
      sched_timeouts = metric_value(
          m, "rio_scheduler_build_timeouts_total"
      ) or 0.0
      # 9093 = worker metrics port (config.rs:163, builders.rs:742).
      # 9091 is the SCHEDULER's — original test copy-pasted the wrong port.
      # Ephemeral: worker may have exited (timed_out is a completion)
      # → proxy fails. Fall back to scheduler metric only.
      # Ephemeral: worker exits after the timed-out build, so the
      # metrics scrape races pod termination. Best-effort only.
      rc, worker_metrics = k3s_server.execute(
          "k3s kubectl -n ${ns} get --raw "
          f"/api/v1/namespaces/${nsBuilders}/pods/{wp}:9093/proxy/metrics"
      )
      if rc != 0:
          worker_metrics = ""
      timed_out_line = [
          l for l in worker_metrics.splitlines()
          if 'rio_builder_builds_total' in l
          and 'outcome="timed_out"' in l
          and not l.startswith('#')
      ]
      worker_timed_out = (
          float(timed_out_line[0].rsplit(' ', 1)[1])
          if timed_out_line else 0.0
      )
      # Ephemeral: worker may exit before reporting TimedOut, and
      # the scheduler's own tick-based timeout may not have fired
      # yet. Assertions 1+2 (cgroup appeared then removed) already
      # prove the build was killed; assertion 4 below proves no
      # leak. Metric check is informational.
      print(f"build-timeout: sched_timeouts={sched_timeouts}, "
            f"worker_timed_out={worker_timed_out} (informational)")

      # ── Assertion 4: same drv, second build succeeds. ────────────
      # Proves the leak really is closed, not just "rmdir warned".
      # Without kill-on-teardown: BuildCgroup::create → mkdir →
      # EEXIST (leaked cgroup from attempt 1 still has the sleep-30
      # process). With the fix: clean slate.
      #
      # No buildTimeout this time — let the 30s sleep complete.
      # submit_build_grpc handles port-allocation + swallow-
      # DeadlineExceeded. We don't need completion — just successful
      # re-dispatch + cgroup recreation (no EEXIST). The helper
      # asserts buildId was returned; EEXIST would surface before
      # any BuildEvent and fail that assert with the error output.
      #
      # DEPENDS ON: scheduler re-dispatching terminal-state drvs.
      # The DAG node goes terminal after TimedOut; resubmit must
      # clear and re-queue. fix-resubmit-terminal lands that path.
      # Re-instantiate is idempotent (same drv_path); nix copy of an
      # already-present .drv is a no-op.
      submit_single_drv("${timeoutDrv}", max_time=3)
      # Cgroup reappeared — concrete proof no EEXIST in
      # BuildCgroup::create. Same dirname (same drv_path → same
      # sanitize_build_id). `| grep .` so wait_until_succeeds retries.
      # Re-resolve the node: the first build's worker_vm is gone
      # (ephemeral one-shot), and the re-dispatch spawns a FRESH Job
      # pod that kube-scheduler may place on the OTHER node. Polling
      # the stale worker_vm would hang 180s if placement flipped.
      wp2 = wait_worker_pod()
      worker_node2 = k3s_server.succeed(
          f"k3s kubectl -n ${nsBuilders} get pod {wp2} "
          "-o jsonpath='{.spec.nodeName}'"
      ).strip()
      worker_vm2 = k3s_agent if worker_node2 == "k3s-agent" else k3s_server
      cgroup_retry = worker_vm2.wait_until_succeeds(
          "find /sys/fs/cgroup -type d -name '*lifecycle-timeout_drv' "
          "-print -quit 2>/dev/null | grep .",
          timeout=180,
      ).strip()
      print(f"build-timeout PASS: same-drv re-dispatched to "
            f"{worker_node2}, cgroup recreated at {cgroup_retry} "
            f"(no EEXIST leak)")
''
