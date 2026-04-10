# scheduling subtest fragment — composed by scenarios/scheduling.nix mkTest.
scope: with scope; ''
  import threading
  import time as _time

  # ══════════════════════════════════════════════════════════════════
  # reassign — SIGKILL worker mid-build, build completes on another
  # ══════════════════════════════════════════════════════════════════
  # NEW coverage — no phase test exercises the smoke-test step 7
  # pattern. Proves: disconnect detection → DAG node back to Ready
  # → redispatch → completes on a different worker.
  with subtest("reassign: SIGKILL mid-build, build completes elsewhere"):
      # Gate: prior subtests (setoptions-unreachable, cancel-timing)
      # leave one-shot small workers in their RestartSec=1s + reconnect
      # window. If BOTH smalls are unregistered at submit time the
      # scheduler dispatches to wlarge immediately and the small_workers
      # poll below never matches. workers_active==3 ⇒ wsmall1 + wsmall2
      # + wlarge all registered. 30s budget covers RestartSec=1s +
      # HEARTBEAT_INTERVAL=10s with slack.
      for _ in range(30):
          _wa = metric_value(
              scrape_metrics(${gatewayHost}, 9091),
              "rio_scheduler_workers_active",
          ) or 0.0
          if _wa >= 3:
              break
          _time.sleep(1)
      else:
          raise AssertionError(
              f"workers_active never reached 3 within 30s (last={_wa})"
          )

      disc_before = scrape_metrics(${gatewayHost}, 9091)
      promo_before = metric_value(disc_before,
          "rio_scheduler_size_class_promotions_total",
          '{kind="builder",from="small",to="large"}') or 0.0

      # Start the slow build in a background thread. Thread-safe:
      # the NixOS test driver's Machine.succeed() can overlap across
      # threads (each is a separate SSH exec).
      bg = {}
      def _bg():
          try:
              bg["out"] = build("${reassignDrv}", capture_stderr=False).strip()
          except Exception as e:
              bg["err"] = e
      bg_thread = threading.Thread(target=_bg, daemon=True)
      bg_thread.start()

      # Find which SMALL worker got the FIRST assignment. No-pname
      # drv → estimator default → "small" class, floor=None. With
      # 2 small workers idle and 0 builds in flight, it MUST go to
      # wsmall1 or wsmall2. 60s: one-shot builders may both be in
      # the RestartSec=1s gap when the build lands; worst case is
      # one restart cycle + heartbeat + dispatch tick.
      assigned = None
      for _ in range(60):
          for w in small_workers:
              c = w.succeed(
                  "journalctl -u rio-builder --no-pager | "
                  "grep -c 'rio-test-sched-reassign' || true"
              ).strip()
              if int(c or "0") > 0:
                  assigned = w
                  break
          if assigned:
              break
          _time.sleep(1)
      assert assigned is not None, (
          "no small worker picked up rio-test-sched-reassign within 60s"
      )
      print(f"reassign: assigned to {assigned.name}, killing")

      # SIGKILL the assigned worker. systemd restarts it (Restart=
      # always) but the scheduler sees the gRPC stream drop
      # immediately → increments disconnects → requeues the build.
      assigned.succeed("systemctl kill -s KILL rio-builder.service")

      # Wait for the background build to complete. Worst case: the
      # first attempt ran ~20s before kill, then a fresh 25s run on
      # another worker, plus scheduler tick latency. 120s is generous.
      bg_thread.join(timeout=120)
      assert not bg_thread.is_alive(), (
          "reassign build thread did not finish within 120s "
          "(hung? scheduler didn't requeue?)"
      )
      if "err" in bg:
          dump_all_logs([${gatewayHost}] + all_workers)
          raise bg["err"]
      assert "out" in bg and bg["out"].startswith("/nix/store/"), (
          f"reassign build returned {bg.get('out')!r}"
      )

      # Disconnect counter incremented ≥1. Not exact: the killed
      # worker may restart and reconnect during the test window;
      # if the scheduler briefly sees it then drops it again during
      # registration churn, count could be >1.
      disc_after = scrape_metrics(${gatewayHost}, 9091)
      d_before = metric_value(disc_before,
          "rio_scheduler_worker_disconnects_total") or 0.0
      d_after = metric_value(disc_after,
          "rio_scheduler_worker_disconnects_total") or 0.0
      assert d_after >= d_before + 1, (
          f"SIGKILL should increment disconnects by >=1; "
          f"before={d_before}, after={d_after}"
      )

      # I-177-reversed: bare disconnect/SIGKILL must NOT promote
      # size_class_floor — only controller-reported OOMKilled or
      # EvictedDiskPressure does (ReportExecutorTermination RPC). The
      # standalone fixture has no rio-controller pod-watch, so no
      # termination report is sent; promotion stays at 0. The retry
      # re-queues at the SAME class (small), not wlarge.
      promo_after = metric_value(disc_after,
          "rio_scheduler_size_class_promotions_total",
          '{kind="builder",from="small",to="large"}') or 0.0
      assert promo_after == promo_before, (
          f"SIGKILL alone must NOT promote size_class_floor "
          f"(only OOMKilled/DiskPressure does); "
          f"before={promo_before}, after={promo_after}"
      )

      # Wait for the killed worker to come back before collectCoverage
      # (otherwise its profraw from pre-kill is all we get, and the
      # workers_active count is wrong for any later scenario runs).
      assigned.wait_for_unit("rio-builder.service")
''
