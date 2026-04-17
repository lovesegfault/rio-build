# scheduling subtest fragment — composed by scenarios/scheduling.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # sigint-graceful — Ctrl+C path: main() returns, FUSE unmounts
  # ══════════════════════════════════════════════════════════════════
  # Before remediation 15: worker main.rs watched SIGTERM only.
  # SIGINT hit the default handler → immediate termination → no
  # Drop, no atexit. FUSE mount leaked (next start EBUSY), profraw
  # never flushed (local dev Ctrl+C = zero coverage).
  #
  # Three-layered assertion:
  #   1. ExecMainCode=1 (CLD_EXITED) + ExecMainStatus=0 → main()
  #      RETURNED Ok(()) via the shutdown.cancelled() select! arm.
  #      PRIMARY — SIGINT default handler would give Code=2
  #      (CLD_KILLED) Status=2 (signal number). Code=1 Status=0
  #      proves main.rs:504 shutdown arm fired, stack unwound.
  #   2. FUSE mount gone. Belt-and-suspenders: AutoUnmount means
  #      the kernel unmounts on fd close regardless, so this alone
  #      doesn't distinguish graceful from crash. But with Code=1
  #      proven above, this confirms Mount::drop ran in the normal
  #      unwind (what a human debugging EBUSY would check first).
  #   3. [coverage mode only] profraw count increased → atexit
  #      fired → LLVM flush ran. Guards .#coverage-full: a main.rs
  #      refactor breaking the cancellation arm would silently
  #      zero worker VM coverage.
  #
  # Uses worker2: worker1 holds FUSE cache state for fuse-direct
  # (core-test cache-chain coupling). worker2 is disposable here —
  # disrupt split only.
  #
  # Standalone fixture only (k3s worker pods are distroless, no
  # shell, no systemctl). This is the only place we can deliver
  # SIGINT to a worker PID and inspect aftermath from the host.
  with subtest("sigint-graceful: SIGINT → main() returns → FUSE unmounts"):
      # Baseline: mount IS present (worker running, FUSE alive).
      # One-shot builder restarts between builds — wait for the
      # next instance to be up and mounted (RestartSec=1s).
      worker2.wait_until_succeeds(
          "mountpoint -q /var/rio/fuse-store", timeout=15
      )

      # Coverage-mode baseline. `ls | wc -l` prints 0 on no-match
      # (wc counts lines from ls's empty stdout); `|| echo 0`
      # swallows ls's non-zero exit on glob-no-match. COUNT before/
      # after, not existence: prior fragments (reassign SIGKILL,
      # or systemd Restart=on-failure churn) may have left stale
      # profraws. A strict "file exists" check would pass for the
      # wrong reason.
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
      # One-shot: a fresh-restarted builder hasn't registered the
      # gauge yet, so "absent" = idle. Invert: fail only if gauge
      # is present AND ≥1.
      worker2.wait_until_succeeds(
          "! curl -sf localhost:9093/metrics 2>/dev/null | "
          "grep -qE '^rio_builder_builds_active\\{role=\"builder\"\\} [1-9]'",
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
      worker2.wait_until_succeeds(
          "systemctl show rio-builder.service -p ActiveState "
          "| grep -qx ActiveState=inactive",
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

      # SECONDARY: FUSE mount gone. Observable now (Restart=no
      # drop-in in effect).
      worker2.succeed("! mountpoint -q /var/rio/fuse-store")

      # TERTIARY [coverage mode only]: fresh profraw appeared.
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
      # Wait for FUSE remount so subsequent fragments (none
      # currently, but collectCoverage + future additions) see
      # a consistent state. FUSE mount happens early in main().
      worker2.wait_until_succeeds(
          "mountpoint -q /var/rio/fuse-store", timeout=30
      )
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
