# scheduling subtest fragment — composed by scenarios/scheduling.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # cgroup — memory.peak + cpu.stat land in build_history
  # ══════════════════════════════════════════════════════════════════
  # Chain: BuildCgroup.memory_peak() → ExecutionResult →
  # CompletionReport → scheduler handle_completion (filters 0 → None)
  # → db.update_build_history COALESCE blend (first sample → just $new).
  #
  # cgroup memory.peak captures the WHOLE TREE (daemon + builder +
  # sleep subprocess). Per-PID VmHWM would only measure nix-daemon's
  # own ~10MB RSS — the builder is a fork()ed child whose footprint
  # never appears there. Even a trivial busybox+sleep build has
  # ~3-10MB tree RSS.
  with subtest("cgroup: memory.peak + cpu.stat → build_history"):
      build("${cgroupDrv}")

      # psql -qtA: NULL → empty line. grep matches ≥7 digits = ≥1MB.
      # wait_until_succeeds: small window between client-sees-built
      # and scheduler-actor DB commit. 10s is overkill but costs
      # nothing when the happy path commits in <100ms.
      ${gatewayHost}.wait_until_succeeds(
          "sudo -u postgres psql rio -qtAc "
          "\"SELECT ema_peak_memory_bytes::bigint FROM build_history "
          "WHERE pname = 'rio-sched-cgroup'\" | "
          "grep -qE '^[0-9]{7,}$'",
          timeout=10,
      )

      # CPU: the drv sleeps 3s specifically so the 1Hz poll fires
      # at least once. sleep uses negligible CPU but the poll
      # baseline captures daemon overhead; value > 0 proves the
      # poll → peak_cpu_cores → CompletionReport → COALESCE blend
      # → ema_peak_cpu_cores chain ran. Non-null is the real signal
      # (completion.rs filters 0 → None).
      ${gatewayHost}.wait_until_succeeds(
          "sudo -u postgres psql rio -qtAc "
          "\"SELECT ema_peak_cpu_cores FROM build_history "
          "WHERE pname = 'rio-sched-cgroup' "
          "AND ema_peak_cpu_cores IS NOT NULL "
          "AND ema_peak_cpu_cores > 0\" | "
          "grep -qE '^[0-9]'",
          timeout=10,
      )
''
