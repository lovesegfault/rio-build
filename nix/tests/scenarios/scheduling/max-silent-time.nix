# scheduling subtest fragment — composed by scenarios/scheduling.nix mkTest.
scope: with scope; ''
  import time as _time

  # ══════════════════════════════════════════════════════════════════
  # max-silent-time — silence arm kills at ~10s, NOT 60s wall-clock
  # ══════════════════════════════════════════════════════════════════
  # silenceDrv echoes once then sleeps 60s. ALL scheduling workers have
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
  # Decoupled from classify(): every worker has the silence config, so
  # the test passes regardless of which class silenceDrv routes to.
  # Previously this relied on cutoff_secs routing to wlarge — broke when
  # the cutoff moved 30→60, and classify() is being deleted anyway.
  with subtest("max-silent-time: silence arm kills at ~10s, not 60s wall-clock"):
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
          f"never fired, sleep ran to completion. If <8s: failed "
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

      # SOME worker must have logged the silence warn. journalctl grep
      # is the end-to-end proof that RIO (not the local nix-daemon)
      # fired. If this is 0 but elapsed is in-range, the nix-daemon
      # enforced it instead — rio-side backstop didn't fire (impl bug).
      # Sum across all workers: I-200 retries may land on different
      # workers each attempt.
      warn_total = sum(
          int(w.succeed(
              "journalctl -u rio-builder --no-pager | "
              "grep -c 'silent for maxSilentTime' || true"
          ).strip() or "0")
          for w in all_workers
      )
      assert warn_total >= 1, (
          f"no worker logged 'silent for maxSilentTime'. The local "
          f"nix-daemon may have enforced maxSilentTime before the "
          f"rio-side backstop fired.\nBuild output:\n{out}"
      )

      print(f"max-silent-time PASS: {attempts} attempts, {per_attempt:.1f}s/attempt (drv sleep was 60s)")
''
