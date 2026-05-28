# scheduling subtest fragment — composed by scenarios/scheduling.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # sigint-graceful — Ctrl+C path: main() returns, Drop/atexit run
  # ══════════════════════════════════════════════════════════════════
  # Before remediation 15: worker main.rs watched SIGTERM only.
  # SIGINT hit the default handler → immediate termination → no
  # Drop, no atexit → profraw never flushed (local dev Ctrl+C =
  # zero coverage) and any in-flight per-build state leaked.
  #
  # Two-layered assertion:
  #   1. ExecMainCode=1 (CLD_EXITED) + ExecMainStatus=0 → main()
  #      RETURNED Ok(()) via the shutdown.cancelled() select! arm.
  #      PRIMARY — SIGINT default handler would give Code=2
  #      (CLD_KILLED) Status=2 (signal number). Code=1 Status=0
  #      proves the shutdown arm fired and the stack unwound (the
  #      castore session and overlay are per-build and torn down at
  #      build end, so process exit has no mount to clean up).
  #   2. [coverage mode only] profraw count increased → atexit
  #      fired → LLVM flush ran. Guards .#coverage: a main.rs
  #      refactor breaking the cancellation arm would silently
  #      zero worker VM coverage.
  #
  # Uses worker2: disposable in the disrupt split (no cache-state
  # coupling with other fragments).
  #
  # Standalone fixture only (k3s worker pods are distroless, no
  # shell, no systemctl). This is the only place we can deliver
  # SIGINT to a worker PID and inspect aftermath from the host.
  with subtest("sigint-graceful: SIGINT → main() returns → atexit runs"):
      # Coverage-mode baseline. COUNT before/after, not existence:
      # prior fragments (reassign SIGKILL, or systemd
      # Restart=on-failure churn) may have left stale profraws. A
      # strict "file exists" check would pass for the wrong reason.
      # shopt nullglob: glob-no-match expands to empty (not literal);
      # printf '%s\n' on empty → one blank line → wc -l = 1, so use
      # a for-loop counter instead. Plain ls fails under pipefail;
      # find fails if dir doesn't exist. This form is pipefail-safe.
      profraw_before = int(worker2.succeed(
          "shopt -s nullglob; "
          "n=0; for f in /var/lib/rio/cov/*.profraw; do n=$((n+1)); done; "
          "echo $n"
      ).strip())

      # Prior subtests (reassign) may have landed a sleepSecs=25 build
      # on worker2. SIGINT-drain waits for in-flight builds; without
      # waiting for idle first, the 30s timeout can't cover 25s+drain.
      #
      # Positive gate, two conditions: the metrics exporter answers
      # (a builder PROCESS is up — the one-shot unit exits after every
      # build and Restart=always brings it back ~1s later) AND the
      # builds_active gauge, if registered, is 0. The earlier inverted
      # form ("absent gauge = idle") also passed during that ~1s
      # auto-restart gap right after load-50drv's last build: SIGINT
      # then went to a unit with no MainPID, the already-queued restart
      # job ignored the Restart=no drop-in, and the fresh builder never
      # got the signal — the inactive-wait below burned its full 30s.
      # A fresh-started builder has no gauge until its first build, so
      # require gauge-not-≥1 rather than gauge-equals-0.
      worker2.wait_until_succeeds(
          "m=$(curl -sf localhost:9093/metrics) && ! echo \"$m\" | "
          "grep -E '^rio_builder_builds_active\\{role=\"builder\"\\} [1-9]' >/dev/null",
          timeout=60,
      )

      # SIGINT, not SIGTERM. systemctl kill delivers to MainPID.
      # `systemctl stop` would send SIGTERM (KillSignal default) —
      # that path already works (rio-common::signal::shutdown_signal
      # watched SIGTERM from day one). SIGINT tests the NEW code
      # at main.rs:503 (r[impl builder.shutdown.sigint]).
      #
      # The unit has Restart=always (one-shot builder), so the
      # post-exit state is a ~1s blip — ExecMainCode reads 0
      # (running) by the time we check. Temporarily disable
      # restart via a runtime drop-in, observe the exit, then
      # restore.
      worker2.succeed(
          "mkdir -p /run/systemd/system/rio-builder.service.d && "
          "printf '[Service]\\nRestart=no\\n' "
          "  > /run/systemd/system/rio-builder.service.d/norestart.conf && "
          "systemctl daemon-reload"
      )
      worker2.succeed("systemctl kill -s INT rio-builder.service")
      # inactive OR failed: if SIGINT ever regresses to death-by-signal
      # the unit lands in "failed" — let the ExecMainCode assert below
      # report that precisely instead of burning this timeout.
      worker2.wait_until_succeeds(
          "systemctl show rio-builder.service -p ActiveState "
          "| grep -qxE 'ActiveState=(inactive|failed)'",
          timeout=30,
      )

      # PRIMARY: exit code. main() returning Ok(()) → CLD_EXITED
      # (Code=1) + Status=0. SIGINT default handler →
      # CLD_KILLED (Code=2) + Status=2.
      exit_info = worker2.succeed(
          "systemctl show rio-builder.service "
          "-p ExecMainCode -p ExecMainStatus"
      )
      assert "ExecMainCode=1" in exit_info, (
          f"worker should exit via return-from-main (CLD_EXITED=1), "
          f"not death-by-signal. Got: {exit_info!r}. "
          f"SIGINT handler not installed? Check rio-common::signal."
      )
      assert "ExecMainStatus=0" in exit_info, (
          f"worker main() should return Ok(()) on SIGINT drain. "
          f"Got: {exit_info!r}"
      )
      print(f"sigint-graceful: {exit_info.strip()} (CLD_EXITED, "
            f"status 0 — main() returned)")

      # SECONDARY [coverage mode only]: fresh profraw appeared.
      # LLVM registers __llvm_profile_write_file in atexit —
      # fires iff main() returns (not on signal death).
      #
      # Nix interpolates a single Python-boolean token, not a
      # block: nested indented-string block-interpolation breaks
      # Python indentation (the inner block strips its OWN common
      # leading whitespace, so the content lands at col-0 inside
      # a col-4 `with subtest` context → mypy `Unexpected indent`
      # on the line after. Observed: test-driver type-check fail
      # at nixos-test-driver-rio-scheduling-disrupt).
      _cov_mode = ${if common.coverage then "True" else "False"}
      if _cov_mode:
          profraw_after = int(worker2.succeed(
              "shopt -s nullglob; "
              "n=0; for f in /var/lib/rio/cov/*.profraw; do n=$((n+1)); done; "
              "echo $n"
          ).strip())
          assert profraw_after > profraw_before, (
              f"graceful SIGINT should flush a fresh profraw via "
              f"atexit; before={profraw_before} after={profraw_after}. "
              f"main() returned (ExecMainCode=1 above) but atexit "
              f"didn't fire? LLVM_PROFILE_FILE unset in unit env?"
          )
          print(f"sigint-graceful: profraw {profraw_before} → "
                f"{profraw_after} (atexit fired)")
      else:
          _ = profraw_before  # silence unused in non-coverage

      # Restore Restart=always (drop the runtime override) and
      # bring the service back.
      worker2.succeed(
          "rm -f /run/systemd/system/rio-builder.service.d/norestart.conf && "
          "systemctl daemon-reload && "
          "systemctl start rio-builder.service"
      )
      worker2.wait_for_unit("rio-builder.service")
      # Wait for scheduler re-registration. Worker heartbeats every
      # HEARTBEAT_INTERVAL_SECS=10 (rio-common/src/limits.rs:51).
      # Without this, any fragment inserted after sigint-graceful
      # sees 2 slots (worker1 only) until worker2's first heartbeat.
      # Timeout 30s: 1 heartbeat interval + TCG slop.
      ${gatewayHost}.wait_until_succeeds(
          "curl -sf http://localhost:9091/metrics | "
          "grep '^rio_scheduler_workers_active ' | "
          "awk '{exit !($2 >= 3)}'",
          timeout=30,
      )
''
