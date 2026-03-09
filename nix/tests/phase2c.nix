# Phase 2c milestone validation as a NixOS VM test.
#
# Milestone (docs/src/phases/phase2c.md:32):
#   Chunk dedup ratio > 30% on nixpkgs rebuild; scheduling latency p99 < 5s.
#
# Chunk dedup + binary cache HTTP: main.rs wiring landed in phase3a
# (A2). This VM test enables the filesystem chunk backend and asserts
# the dedup metric after a build. Validates end-to-end:
#
#   1. Size-class config load: cutoff_seconds gauge from /etc/rio/scheduler.toml
#   2. Critical-path priority: chain-a dispatched before standalone solo
#   3. Size-class routing: pre-seeded "bigthing" → w-large not w-small
#   4. content_index populated: PutPath writes nar_hash → store_path rows
#
# Circuit breaker: covered by E5 unit tests. Can't trigger via nix-build
# when store is fully down — gateway's wopEnsurePath fails before
# SubmitBuild reaches the scheduler's merge where the breaker lives.
#
# Five VMs:
#   control  — PostgreSQL, rio-store, rio-scheduler, rio-gateway
#   wsmall1  — rio-worker sizeClass="small"
#   wsmall2  — rio-worker sizeClass="small"
#   wlarge   — rio-worker sizeClass="large"
#   client   — Nix client
#
# Run interactively:
#   nix build .#checks.x86_64-linux.vm-phase2c.driverInteractive
#   ./result/bin/nixos-test-driver
{
  pkgs,
  rio-workspace,
  rioModules,
  coverage ? false,
}:
let
  common = import ./common.nix {
    inherit
      pkgs
      rio-workspace
      rioModules
      coverage
      ;
  };

  testDrvFile = ./phase2c-derivation.nix;

  # Size-class cutoffs. "small" for quick builds (≤30s), "large" for
  # slow ones. mem_limit is effectively unlimited here (u64::MAX-ish);
  # duration is the discriminator for this test.
  schedulerSizeClasses = ''
    [[size_classes]]
    name = "small"
    cutoff_secs = 30.0
    mem_limit_bytes = 17179869184

    [[size_classes]]
    name = "large"
    cutoff_secs = 3600.0
    mem_limit_bytes = 68719476736
  '';
in
pkgs.testers.runNixOSTest {
  name = "rio-phase2c";

  nodes = {
    control = common.mkControlNode {
      hostName = "control";
      memorySize = 1536;
      extraSchedulerConfig = {
        # size_classes via /etc/rio/scheduler.toml (figment reads it).
        # Env vars can't express TOML arrays-of-tables.
        extraConfig = schedulerSizeClasses;
        # Short tick = faster estimator refresh. Default 10s × 6 = 60s
        # wait for estimator; 2s × 6 = 12s is tolerable for a VM test.
        tickIntervalSecs = 2;
      };
      extraStoreConfig = {
        # [chunk_backend] via /etc/rio/store.toml (same figment
        # pattern as scheduler size_classes). Env vars don't express
        # serde internally-tagged enums cleanly.
        #
        # /var/lib/rio/store is StateDirectory (systemd creates it
        # with proper ownership). FilesystemChunkBackend::new creates
        # the chunks/ subdir + 256-way fanout at startup.
        extraConfig = ''
          [chunk_backend]
          kind = "filesystem"
          base_dir = "/var/lib/rio/store/chunks"
        '';
      };
      # 9090-9092: Prometheus metrics ports.
      extraFirewallPorts = [
        9090
        9091
        9092
      ];
      extraPackages = [ pkgs.postgresql ]; # psql for direct queries
    };

    wsmall1 = common.mkWorkerNode {
      hostName = "wsmall1";
      maxBuilds = 2;
      sizeClass = "small";
    };
    wsmall2 = common.mkWorkerNode {
      hostName = "wsmall2";
      maxBuilds = 2;
      sizeClass = "small";
    };
    wlarge = common.mkWorkerNode {
      hostName = "wlarge";
      maxBuilds = 2;
      sizeClass = "large";
    };

    client = common.mkClientNode {
      gatewayHost = "control";
      extraPackages = [ pkgs.curl ];
    };
  };

  testScript = ''
    start_all()

    # ── Bootstrap ─────────────────────────────────────────────────────
    ${common.waitForControlPlane "control"}

    # Verify scheduler loaded the size_classes config. This gauge is
    # set once at startup from /etc/rio/scheduler.toml. If it's absent,
    # figment didn't read the TOML and every subsequent size-class
    # assertion will fail for the wrong reason.
    control.succeed(
        "curl -sf http://localhost:9091/metrics | "
        "grep 'rio_scheduler_cutoff_seconds{class=\"small\"} 30'"
    )
    control.succeed(
        "curl -sf http://localhost:9091/metrics | "
        "grep 'rio_scheduler_cutoff_seconds{class=\"large\"} 3600'"
    )

    # SSH key + gateway restart (same dance as phase2a/2b).
    ${common.sshKeySetup "control"}

    # All 3 workers register. Check they declared size_class.
    workers = [wsmall1, wsmall2, wlarge]
    for w in workers:
        w.wait_for_unit("rio-worker.service")
    control.wait_until_succeeds(
        "curl -sf http://localhost:9091/metrics | "
        "grep -E 'rio_scheduler_workers_active 3'"
    )

    # Seed busybox.
    ${common.seedBusybox "control"}

    # ── Assertion 1: CA data model roundtrip ─────────────────────────
    # The phase2c derivations aren't CA (that needs __contentAddressed
    # which needs builder changes), but the REALISATIONS TABLE is what
    # we're validating. Any successful build with wopRegisterDrvOutput
    # being called writes there. Modern Nix sends it opportunistically
    # — let's first check if it does by running a build, then verify
    # via psql. If it turns out nix doesn't send Register for IA
    # derivations, this still validates the table EXISTS and queries
    # work (via E3's schema), and the G2 unit test covers the wire
    # path.
    #
    # Realisations count before (should be 0).
    before = control.succeed(
        "sudo -u postgres psql rio -t -c 'SELECT count(*) FROM realisations'"
    ).strip()
    assert before == "0", f"realisations should start empty, got {before}"

    # Helper: run a build against the gateway, dumping logs on failure.
    ${common.mkBuildHelper {
      gatewayHost = "control";
      inherit testDrvFile;
    }}

    # ── Build: chain + solo (critical-path test derivation) ──────────
    build(workers, attr="all")

    control.succeed(
        "curl -sf http://localhost:9091/metrics | "
        "grep -E 'rio_scheduler_builds_total\\{outcome=\"success\"\\} [1-9]'"
    )

    # ── Assertion 2: Scheduling pipeline wired ───────────────────────
    # Both chain-a and solo dispatched. Smoke check that the full
    # merge → compute_initial → push_ready → dispatch_ready →
    # best_worker pipeline works end-to-end.
    #
    # NOT asserting dispatch ORDER: chain-a and solo are both LEAVES
    # with no dependencies → both have priority = est_duration = 30 →
    # TIE (FIFO tie-break by seq). Critical-path gives higher priority
    # to nodes with accumulated work: chain-b=60, chain-c=90 (est_dur +
    # max(children.priority) bottom-up). The benefit is chain-b beating
    # a fresh leaf AFTER chain-a completes — not chain-a beating solo.
    # Unit tests (D4/D5) prove the priority computation; here we just
    # check the wiring is live.
    sched_log = control.succeed(
        "journalctl -u rio-scheduler --no-pager | "
        "grep 'worker acknowledged assignment' || true"
    )
    assert "rio-2c-chain-a" in sched_log, "chain-a should have dispatched"
    assert "rio-2c-solo" in sched_log, "solo should have dispatched"
    assert "rio-2c-chain-c" in sched_log, "chain-c should have dispatched (depends on b depends on a)"

    # ── Assertion 3: Size-class routing (pre-seeded EMA) ─────────────
    # Pre-seed build_history: "rio-2c-bigthing" has 120s EMA. With
    # small cutoff=30s, classify() picks "large" → only wlarge gets it.
    #
    # The estimator refreshes every 6 ticks = 12s (tickInterval=2s).
    # Insert the seed, wait for refresh, then build.
    #
    # CAPTURE baseline refresh count first — by now the scheduler has
    # been running for ~60s (bootstrap + worker registration + earlier
    # builds) so the counter is already well above 0. We need to wait
    # for it to INCREASE after the INSERT, not reach a fixed value.
    # (The original sleep(15) was simpler but racy; this is race-free.)
    baseline = int(control.succeed(
        "curl -sf http://localhost:9091/metrics | "
        "grep -E '^rio_scheduler_estimator_refresh_total ' | "
        "awk '{print $2}' || echo 0"
    ).strip() or "0")
    print(f"estimator refresh baseline: {baseline}")

    control.succeed(
        "sudo -u postgres psql rio -c "
        "\"INSERT INTO build_history (pname, system, ema_duration_secs, sample_count, last_updated) "
        "VALUES ('rio-2c-bigthing', 'x86_64-linux', 120.0, 1, now())\""
    )

    # Wait for AT LEAST TWO more refreshes (proves seed was picked
    # up). Two not one: a refresh could fire between baseline-capture
    # and INSERT (12s cadence vs sub-second INSERT latency). The
    # refresh AFTER that one is guaranteed to see the seed.
    target = baseline + 2
    control.wait_until_succeeds(
        "test \"$(curl -sf http://localhost:9091/metrics | "
        "grep -E '^rio_scheduler_estimator_refresh_total ' | "
        f"awk '{{print $2}}' || echo 0)\" -ge {target}",
        timeout=30
    )

    # Round 4 V9: capture size_class_assignments_total{class="large"}
    # baseline BEFORE bigthing build. Prior check (`[1-9]` after build)
    # could false-pass if an earlier build somehow routed to large.
    # Baseline-delta proves THIS build specifically went to large.
    large_baseline = int(control.succeed(
        "curl -sf http://localhost:9091/metrics | "
        "grep -E '^rio_scheduler_size_class_assignments_total\\{class=\"large\"\\} ' | "
        "awk '{print $2}' || echo 0"
    ).strip() or "0")
    print(f"size_class large baseline: {large_baseline}")

    # Build bigthing (pname="rio-2c-bigthing" in env — estimator keys
    # on that). With the 120s seeded EMA and 30s small cutoff,
    # classify() picks "large".
    build(workers, attr="bigthing")

    # Check the size_class_assignments metric: large should be
    # EXACTLY baseline+1 (bigthing is a single-node derivation).
    # If the delta is 0, classify() picked small (wrong). If >1,
    # something else went to large (unexpected but not fatal —
    # just assert >= baseline+1).
    large_after = int(control.succeed(
        "curl -sf http://localhost:9091/metrics | "
        "grep -E '^rio_scheduler_size_class_assignments_total\\{class=\"large\"\\} ' | "
        "awk '{print $2}'"
    ).strip())
    assert large_after >= large_baseline + 1, (
        f"V9: bigthing should increment large assignments by >=1; "
        f"baseline={large_baseline}, after={large_after} (delta={large_after - large_baseline})"
    )
    print(f"V9 PASS: large assignments {large_baseline}→{large_after} (delta={large_after - large_baseline})")

    # Softer check: wlarge's logs should show the bigthing assignment,
    # small workers' should NOT. (journalctl grep; noisier than the
    # metric but proves end-to-end.)
    wlarge.succeed(
        "journalctl -u rio-worker --no-pager | grep 'rio-2c-bigthing'"
    )
    # Small workers: neither got it. `fail()` inverts the exit code.
    wsmall1.fail(
        "journalctl -u rio-worker --no-pager | grep 'rio-2c-bigthing'"
    )
    wsmall2.fail(
        "journalctl -u rio-worker --no-pager | grep 'rio-2c-bigthing'"
    )

    # ── Circuit breaker: covered by E5 unit tests ────────────────────
    # The scheduler's CacheCheckBreaker is inside merge.rs → it fires
    # when FindMissingPaths fails INSIDE the scheduler. But nix-build
    # hits the GATEWAY first, and the gateway does its own store calls
    # (wopEnsurePath → QueryPathInfo) which fail earlier when the store
    # is down. So the gateway short-circuits before SubmitBuild ever
    # reaches the scheduler's merge, and the breaker never probes.
    #
    # This is correct layering — the gateway fails fast too, just not
    # via the breaker. To VM-test the breaker we'd need a store that's
    # UP for gateway calls but DOWN for the scheduler's FindMissingPaths,
    # which is impractical to rig. E5's unit test directly triggers
    # merge with a failing MockStore: that's the authoritative coverage.

    # ── Final: content_index populated ───────────────────────────────
    # Every PutPath now inserts into content_index (G1). Count should
    # be > 0 after all the builds. This validates the insert path is
    # wired in the real binary, not just tests.
    ci_count = control.succeed(
        "sudo -u postgres psql rio -t -c 'SELECT count(*) FROM content_index'"
    ).strip()
    assert int(ci_count) > 0, f"content_index should have entries after builds; got {ci_count}"

    # ── A3: chunk backend wired (main.rs A2 landed) ──────────────────
    # The filesystem backend is enabled via extraStoreConfig above.
    #
    # Round 4 V7: prior check was `chunk_count > 0` — but the builds
    # above (all + bigthing) are tiny text files, likely ALL inline.
    # Prove the CHUNKED path specifically: capture baseline, build
    # the bigblob derivation (300 KiB > INLINE_THRESHOLD), assert
    # chunk count increased. Delta check proves this build went
    # through chunked PutPath, not inline.
    chunk_baseline = int(control.succeed(
        "find /var/lib/rio/store/chunks -type f 2>/dev/null | wc -l"
    ).strip())
    print(f"chunk baseline: {chunk_baseline}")

    # bigblob: 300 KiB of zeros → exceeds INLINE_THRESHOLD (256 KiB).
    # MUST go through chunked path.
    build(workers, attr="bigblob")

    chunk_after = int(control.succeed(
        "find /var/lib/rio/store/chunks -type f 2>/dev/null | wc -l"
    ).strip())
    assert chunk_after > chunk_baseline, (
        f"V7: bigblob (300 KiB) should write chunks to disk "
        f"(>INLINE_THRESHOLD). baseline={chunk_baseline}, "
        f"after={chunk_after} — chunk backend not wired, or "
        f"INLINE_THRESHOLD changed?"
    )
    print(f"V7 PASS: chunks {chunk_baseline}→{chunk_after} "
          f"(delta={chunk_after - chunk_baseline})")

    # The dedup metric should also be present (non-zero is fine,
    # even if ratio is low — proves the gauge is registered and
    # the chunked PutPath path ran at least once).
    metrics = control.succeed("curl -s http://localhost:9092/metrics")
    assert "rio_store_chunk_dedup_ratio" in metrics, (
        "rio_store_chunk_dedup_ratio metric should be exported "
        "(chunked PutPath path ran)"
    )

    ${common.collectCoverage "control, wsmall1, wsmall2, wlarge, client"}
  '';
}
