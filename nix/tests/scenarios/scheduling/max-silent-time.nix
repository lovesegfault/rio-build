# scheduling subtest fragment — composed by scenarios/scheduling.nix mkTest.
scope: with scope; ''
  import time as _time

  # ══════════════════════════════════════════════════════════════════
  # max-silent-time — silence arm kills at ~10s, NOT 60s wall-clock
  # ══════════════════════════════════════════════════════════════════
  # silenceDrv echoes once then sleeps 60s. wlarge has worker-config
  # RIO_MAX_SILENT_TIME_SECS=10 (default.nix fixture). The stderr-loop
  # silence select! arm fires ~10s after the echo → BuildStatus::TimedOut
  # → cgroup.kill() reaps the sleep. Wall-clock elapsed MUST be <<60s;
  # if it's ~60s, the silence arm never fired (sleep ran to completion).
  #
  # WHY WORKER-SIDE CONFIG: the Nix ssh-ng client (protocol 1.38) does
  # NOT send wopSetOptions to the gateway. Client-side --max-silent-time
  # cannot propagate. Verified empirically: the gateway's info-level
  # wopSetOptions log never fires during a nix-build --store ssh-ng://
  # session. Worker config is the operator's fleet default until a
  # gateway-side propagation path lands (follow-up).
  #
  # ROUTING: seed build_history with 120s EMA for pname rio-sched-silence
  # → classify() picks "large" → dispatches to wlarge (the only worker
  # with the silence config). Same pattern as sizeclass/bigthing.
  with subtest("max-silent-time: silence arm kills at ~10s, not 60s wall-clock"):
      # Seed + wait-for-refresh (same pattern as sizeclass).
      # TWO refreshes not one: a refresh could fire between
      # baseline-capture and INSERT (12s cadence vs sub-ms INSERT).
      refresh_baseline = int(${gatewayHost}.succeed(
          "curl -sf http://localhost:9091/metrics | "
          "grep -E '^rio_scheduler_estimator_refresh_total ' | "
          "awk '{print $2}' || echo 0"
      ).strip() or "0")
      psql(${gatewayHost},
          "INSERT INTO build_history "
          "(pname, system, ema_duration_secs, sample_count, last_updated) "
          "VALUES ('rio-sched-silence', 'x86_64-linux', 120.0, 1, now())")
      target = refresh_baseline + 2
      ${gatewayHost}.wait_until_succeeds(
          "test \"$(curl -sf http://localhost:9091/metrics | "
          "grep -E '^rio_scheduler_estimator_refresh_total ' | "
          f"awk '{{print $2}}' || echo 0)\" -ge {target}",
          timeout=40,
      )

      # expect_fail=True: TimedOut is a build FAILURE (nix-build
      # exits nonzero). The timing wrap stays at the callsite —
      # measures end-to-end including ssh setup, not just build.
      t0 = _time.monotonic()
      out = build("${silenceDrv}", expect_fail=True)
      elapsed = _time.monotonic() - t0

      # I-200: TimedOut now resets to Ready and retries up to
      # RetryPolicy.max_timeout_retries (default 4) before going
      # terminal. The drv runs 1+max_timeout_retries times; each
      # echoes start-silence-marker. Per-attempt timing proves the
      # silence arm fired EVERY time — if any attempt ran the full
      # 60s sleep, elapsed/attempts >> 25.
      attempts = out.count("start-silence-marker")
      assert attempts >= 1, (
          f"no start-silence-marker in output — drv never ran (eval "
          f"error or wrong-worker routing).\nBuild output:\n{out}"
      )
      per_attempt = elapsed / attempts
      # Timing proof. 10s silence + daemon-setup + QEMU/SSH overhead
      # → expect ~12-25s/attempt. 30s/attempt upper bound is <<60s
      # (the key constraint: silence arm fired, sleep didn't run to
      # completion). Lower bound 8s: the silence arm can't fire
      # before the 10s deadline; if per_attempt<8s the failure was
      # something else (immediate daemon crash, wrong status code).
      assert 8 < per_attempt < 30, (
          f"expected silence kill at ~10s/attempt (per-attempt "
          f"~12-25s), got {per_attempt:.1f}s over {attempts} attempts "
          f"(total {elapsed:.1f}s). If ~60s/attempt: silence arm "
          f"never fired, sleep ran to completion (routed to a worker "
          f"without RIO_MAX_SILENT_TIME_SECS?). If <8s: failed "
          f"before silence deadline.\nBuild output:\n{out}"
      )
      # I-200 retry loop sanity: with default max_timeout_retries=4
      # this is 5. Asserting >=2 (not ==5) keeps the test green if
      # the fixture overrides max_timeout_retries while still
      # verifying the retry path fired at least once.
      assert attempts >= 2, (
          f"expected TimedOut→retry (I-200) but only {attempts} "
          f"attempt(s). max_timeout_retries=0 in fixture?\n"
          f"Build output:\n{out}"
      )

      # wlarge must have logged the silence warn. journalctl grep is
      # the end-to-end proof that RIO (not the local nix-daemon) fired.
      # If this is 0 but elapsed is in-range, the nix-daemon enforced
      # it instead — rio-side backstop didn't fire (impl bug).
      warn_count = wlarge.succeed(
          "journalctl -u rio-builder --no-pager | "
          "grep -c 'silent for maxSilentTime' || true"
      ).strip()
      assert int(warn_count or "0") >= 1, (
          f"wlarge did not log 'silent for maxSilentTime'. Either "
          f"(a) routed to wrong worker (check classify), or (b) local "
          f"nix-daemon enforced maxSilentTime before rio-side fired.\n"
          f"Build output:\n{out}"
      )

      # Confirm NOT routed to small workers (proves build_history seed
      # → classify → wlarge worked). If silenceDrv landed on a small
      # worker, it would have run 60s and succeeded (no silence config).
      for w in small_workers:
          w.fail(
              "journalctl -u rio-builder --no-pager | "
              "grep 'rio-sched-silence'"
          )

      print(f"max-silent-time PASS: {attempts} attempts, {per_attempt:.1f}s/attempt (drv sleep was 60s)")
''
