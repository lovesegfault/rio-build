# scheduling subtest fragment — composed by scenarios/scheduling.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # sizeclass — psql-seeded EMA routes bigthing to large
  # ══════════════════════════════════════════════════════════════════
  with subtest("sizeclass: pre-seeded 120s EMA routes to large worker"):
      # Seed build_history: "rio-2c-bigthing" has 120s EMA. With
      # small cutoff=30s, classify() picks "large" → only wlarge gets
      # it. pname MUST match sizeclass.nix's env.pname exactly.
      #
      # Estimator refreshes every 6 ticks = 12s (tickInterval=2s).
      # Insert the seed, wait for AT LEAST TWO more refreshes — two
      # not one: a refresh could fire between baseline-capture and
      # INSERT (12s cadence vs sub-second INSERT latency). The refresh
      # AFTER that one is guaranteed to see the seed.
      refresh_baseline = int(${gatewayHost}.succeed(
          "curl -sf http://localhost:9091/metrics | "
          "grep -E '^rio_scheduler_estimator_refresh_total ' | "
          "awk '{print $2}' || echo 0"
      ).strip() or "0")

      psql(${gatewayHost},
          "INSERT INTO build_history "
          "(pname, system, ema_duration_secs, sample_count, last_updated) "
          "VALUES ('rio-2c-bigthing', 'x86_64-linux', 120.0, 1, now())")

      target = refresh_baseline + 2
      ${gatewayHost}.wait_until_succeeds(
          "test \"$(curl -sf http://localhost:9091/metrics | "
          "grep -E '^rio_scheduler_estimator_refresh_total ' | "
          f"awk '{{print $2}}' || echo 0)\" -ge {target}",
          timeout=30,
      )

      # Baseline BEFORE bigthing build — a simple post-build ≥1 check
      # could false-pass if an earlier fanout leaf somehow routed to
      # large. The delta proves THIS build specifically went large.
      sched_before = scrape_metrics(${gatewayHost}, 9091)
      wl_before = journal_builds_succeeded(wlarge)

      build("${drvs.sizeclass}", attr="bigthing")

      sched_after = scrape_metrics(${gatewayHost}, 9091)
      wl_after = journal_builds_succeeded(wlarge)

      # size_class_assignments_total{class="large"} incremented ≥1.
      # bigthing is single-node so expected delta is exactly 1, but
      # ≥1 tolerates any extra dispatch noise.
      large_before = metric_value(sched_before,
          "rio_scheduler_size_class_assignments_total", '{class="large"}') or 0.0
      large_after = metric_value(sched_after,
          "rio_scheduler_size_class_assignments_total", '{class="large"}') or 0.0
      assert large_after >= large_before + 1, (
          f"bigthing should increment large assignments by >=1; "
          f"before={large_before}, after={large_after} "
          f"(delta={large_after - large_before})"
      )

      # wlarge's journald build count incremented (proves DISPATCH,
      # not just classification). Per-process Prometheus counter
      # resets on one-shot restart, so journald is the signal.
      assert wl_after >= wl_before + 1, (
          f"wlarge should have built bigthing; "
          f"journald before={wl_before}, after={wl_after}"
      )

      # Small workers: NEITHER got it. journalctl grep is noisier
      # than metrics but proves end-to-end — the derivation name
      # never appeared in their logs at all.
      wlarge.succeed(
          "journalctl -u rio-builder --no-pager | grep 'rio-2c-bigthing'"
      )
      for w in small_workers:
          w.fail(
              "journalctl -u rio-builder --no-pager | grep 'rio-2c-bigthing'"
          )
''
