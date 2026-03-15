# Scheduling scenario: fanout distribution, size-class routing, chunked
# PutPath, worker-disconnect reassignment, cgroup→build_history.
#
# Ports phase2a + phase2c + phase3a(cgroup) to the fixture architecture.
# Needs the standalone fixture with 3 workers (2 small, 1 large) and
# size-class + chunk-backend TOML:
#
#   fixture = standalone {
#     workers = {
#       wsmall1 = { maxBuilds = 2; sizeClass = "small"; };
#       wsmall2 = { maxBuilds = 2; sizeClass = "small"; };
#       wlarge  = { maxBuilds = 1; sizeClass = "large"; };
#     };
#     extraSchedulerConfig = { tickIntervalSecs = 2; extraConfig = <size-classes>; };
#     extraStoreConfig = { extraConfig = <chunk_backend filesystem>; };
#     extraPackages = [ pkgs.postgresql ];  # psql for build_history queries
#   };
#
# r[verify worker.overlay.stacked-lower]
# r[verify worker.ns.order]
#   The writableStore=false pattern in common.nix:mkWorkerNode keeps the
#   worker VM's /nix/store as a plain 9p mount (not itself an overlay),
#   so the per-build overlay's lowerdir=/nix/store:{fuse} stack is valid.
#   A build succeeding also proves mount-namespace ordering: both overlayfs
#   and nix-daemon's sandbox need unshare(CLONE_NEWNS); wrong order → fail.
#
# r[verify obs.metric.scheduler]
# r[verify obs.metric.worker]
# r[verify obs.metric.store]
#   Asserted end-to-end from /metrics scrapes via assert_metric_*: exact
#   values (not grep '[1-9]') so CI logs show actual-vs-expected on failure.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) gatewayHost;
  drvs = import ../lib/derivations.nix { inherit pkgs; };

  # reassign: slow build, no pname → estimator default → "small" class.
  # 25s gives the test ~20s to find+kill the assigned worker before the
  # build would naturally finish.
  reassignDrv = drvs.mkTrivial {
    marker = "sched-reassign";
    sleepSecs = 25;
  };

  # cgroup: needs pname in env (completion.rs:181 guards on state.pname;
  # gateway extracts from drv.env().get("pname")) AND sleep ≥2s (so the
  # 1Hz CPU poll in executor/mod.rs fires at least once). mkTrivial
  # doesn't set pname, so inline a custom drv.
  cgroupDrv = pkgs.writeText "drv-sched-cgroup.nix" ''
    { busybox }:
    derivation {
      name = "rio-sched-cgroup";
      pname = "rio-sched-cgroup";
      system = builtins.currentSystem;
      builder = "''${busybox}/bin/sh";
      args = [ "-c" '''
        ''${busybox}/bin/busybox sleep 3
        echo cgroup > $out
      ''' ];
    }
  '';
in
pkgs.testers.runNixOSTest {
  name = "rio-scheduling";
  # Bootstrap (~60s) + fanout (~20s) + sizeclass estimator-wait (~30s)
  # + reassign (25s×2 worst-case + kill latency) + cgroup (~10s).
  globalTimeout = 600;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

    import threading
    import time as _time

    start_all()
    ${fixture.waitReady}
    ${common.sshKeySetup gatewayHost}
    ${common.seedBusybox gatewayHost}

    all_workers = [wsmall1, wsmall2, wlarge]
    small_workers = [wsmall1, wsmall2]

    def build(drv_file, attr="", capture_stderr=True):
        cmd = (
            f"nix-build --no-out-link --store 'ssh-ng://${gatewayHost}' "
            f"--arg busybox '(builtins.storePath ${common.busybox})' "
            f"{drv_file}"
        )
        if attr:
            cmd += f" -A {attr}"
        if capture_stderr:
            cmd += " 2>&1"
        try:
            return client.succeed(cmd)
        except Exception:
            dump_all_logs([${gatewayHost}] + all_workers)
            raise

    # ── Size-class config load (precondition) ─────────────────────────
    # This gauge is set once at startup from /etc/rio/scheduler.toml.
    # If absent, figment didn't read the TOML and every subsequent
    # size-class assertion will fail for the wrong reason.
    assert_metric_exact(${gatewayHost}, 9091,
        "rio_scheduler_cutoff_seconds", 30.0, labels='{class="small"}')
    assert_metric_exact(${gatewayHost}, 9091,
        "rio_scheduler_cutoff_seconds", 3600.0, labels='{class="large"}')

    # ══════════════════════════════════════════════════════════════════
    # fanout — 4 leaves + 1 collector distributed across workers
    # ══════════════════════════════════════════════════════════════════
    with subtest("fanout: 4-leaf DAG distributes across workers"):
        # capture_stderr=False → stdout is just the output path.
        out = build("${drvs.fanout}", capture_stderr=False).strip()
        assert out.startswith("/nix/store/"), f"unexpected output: {out!r}"

        # rio-root's $out/stamp has its own name + 4 child stamps
        # (fanout.nix:27-28). EXACT count, not grep-match: proves
        # (a) output retrievable via wopNarFromPath, (b) collector
        # concatenated all 4 children (DAG walked correctly), (c) NAR
        # bytes intact (wrong content → wrong count).
        leaf_count = client.succeed(
            f"nix store cat --store 'ssh-ng://${gatewayHost}' {out}/stamp | "
            f"grep -c 'rio-leaf-'"
        ).strip()
        assert leaf_count == "4", (
            f"expected exactly 4 'rio-leaf-' lines in {out}/stamp, "
            f"got {leaf_count}. DAG walk or NAR content corrupted?"
        )

        # Scheduler: at least one build reached terminal success.
        assert_metric_ge(${gatewayHost}, 9091,
            "rio_scheduler_builds_total", 1.0, labels='{outcome="success"}')

        # Distribution: EACH SMALL worker executed ≥1 derivation.
        # With size-classes configured, no-pname leaves route to the
        # default class ("small") → wlarge gets nothing from this
        # fanout. 4 parallel leaves + 4 small slots (2×2) means BOTH
        # small workers get at least one leaf. If either sat idle,
        # dispatch is broken (scheduler not round-robin, or worker
        # registration metadata wrong).
        for w in small_workers:
            assert_metric_ge(w, 9093,
                "rio_worker_builds_total", 1.0, labels='{outcome="success"}')

        # FUSE fetch: each SMALL worker pulled ≥1 path from rio-store
        # (busybox must be fetched before any build runs). wlarge
        # never built anything → never fetched anything.
        for w in small_workers:
            assert_metric_ge(w, 9093,
                "rio_worker_fuse_cache_misses_total", 1.0)

        # Store: received 5 build outputs via PutPath (+ busybox seed).
        # ≥5 to be robust against retries.
        assert_metric_ge(${gatewayHost}, 9092,
            "rio_store_put_path_total", 5.0, labels='{result="created"}')

        # PrefetchHint: the collector (rio-root) has 4 DAG children.
        # When root dispatches, approx_input_closure returns the 4
        # leaf output paths. Worker bloom filter is cold (first hint
        # for that worker) → hint sent with ≥1 path. paths_sent is
        # tighter than hints_sent: an empty-hint bug (message sent,
        # 0 paths) would pass hints≥1 but fail paths≥1. (phase3a:485)
        assert_metric_ge(${gatewayHost}, 9091,
            "rio_scheduler_prefetch_paths_sent_total", 1.0)

        # content_index populated: every PutPath inserts a row. ≥5
        # (same bound as put_path_total above). Proves the insert is
        # wired in the real binary, not just unit tests. (phase2c:304)
        ci_count = int(psql(${gatewayHost},
            "SELECT count(*) FROM content_index"))
        assert ci_count >= 5, (
            f"content_index should have ≥5 entries after fanout; "
            f"got {ci_count}"
        )

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
        wlarge_before = scrape_metrics(wlarge, 9093)

        build("${drvs.sizeclass}", attr="bigthing")

        sched_after = scrape_metrics(${gatewayHost}, 9091)
        wlarge_after = scrape_metrics(wlarge, 9093)

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

        # wlarge's worker_builds_total incremented (proves DISPATCH,
        # not just classification).
        wl_before = metric_value(wlarge_before,
            "rio_worker_builds_total", '{outcome="success"}') or 0.0
        wl_after = metric_value(wlarge_after,
            "rio_worker_builds_total", '{outcome="success"}') or 0.0
        assert wl_after >= wl_before + 1, (
            f"wlarge should have built bigthing; "
            f"before={wl_before}, after={wl_after}"
        )

        # Small workers: NEITHER got it. journalctl grep is noisier
        # than metrics but proves end-to-end — the derivation name
        # never appeared in their logs at all.
        wlarge.succeed(
            "journalctl -u rio-worker --no-pager | grep 'rio-2c-bigthing'"
        )
        for w in small_workers:
            w.fail(
                "journalctl -u rio-worker --no-pager | grep 'rio-2c-bigthing'"
            )

    # ══════════════════════════════════════════════════════════════════
    # chunks — 300KiB output forces chunked PutPath, not inline
    # ══════════════════════════════════════════════════════════════════
    with subtest("chunks: 300KiB bigblob writes chunk files to disk"):
        # All builds above are tiny text files, likely inline — a
        # post-build `chunk_count > 0` check would NOT prove the chunked
        # path ran. Capture baseline, build bigblob (300 KiB >
        # INLINE_THRESHOLD = 256 KiB), assert chunk count increased.
        chunk_baseline = int(${gatewayHost}.succeed(
            "find /var/lib/rio/store/chunks -type f 2>/dev/null | wc -l"
        ).strip())

        build("${drvs.sizeclass}", attr="bigblob")

        chunk_after = int(${gatewayHost}.succeed(
            "find /var/lib/rio/store/chunks -type f 2>/dev/null | wc -l"
        ).strip())
        assert chunk_after > chunk_baseline, (
            f"bigblob (300 KiB) MUST write chunks to disk "
            f"(>INLINE_THRESHOLD). baseline={chunk_baseline}, "
            f"after={chunk_after} — chunk backend not wired, or "
            f"INLINE_THRESHOLD changed?"
        )

        # Dedup metric registered + exported (proves chunked PutPath
        # codepath ran at least once; ratio value is irrelevant here).
        metrics = ${gatewayHost}.succeed("curl -s http://localhost:9092/metrics")
        assert "rio_store_chunk_dedup_ratio" in metrics, (
            "rio_store_chunk_dedup_ratio metric should be exported "
            "(chunked PutPath path ran)"
        )

    # ══════════════════════════════════════════════════════════════════
    # reassign — SIGKILL worker mid-build, build completes on another
    # ══════════════════════════════════════════════════════════════════
    # NEW coverage — no phase test exercises the smoke-test step 7
    # pattern. Proves: disconnect detection → DAG node back to Ready
    # → redispatch → completes on a different worker.
    with subtest("reassign: SIGKILL mid-build, build completes elsewhere"):
        disc_before = scrape_metrics(${gatewayHost}, 9091)

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

        # Find which SMALL worker got the assignment. No-pname drv →
        # estimator default → "small" class. With 4 small slots free
        # (2×2) and 0 builds in flight, it MUST go to wsmall1 or
        # wsmall2. If neither logs the marker within 30s, the build
        # either hung in SubmitBuild or routed to wlarge (both bugs).
        assigned = None
        for _ in range(30):
            for w in small_workers:
                c = w.succeed(
                    "journalctl -u rio-worker --no-pager | "
                    "grep -c 'rio-test-sched-reassign' || true"
                ).strip()
                if int(c or "0") > 0:
                    assigned = w
                    break
            if assigned:
                break
            _time.sleep(1)
        assert assigned is not None, (
            "no small worker picked up rio-test-sched-reassign within 30s"
        )
        print(f"reassign: assigned to {assigned.name}, killing")

        # SIGKILL the assigned worker. systemd restarts it (Restart=
        # on-failure) but the scheduler sees the gRPC stream drop
        # immediately → increments disconnects → requeues the build.
        assigned.succeed("systemctl kill -s KILL rio-worker.service")

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

        # Wait for the killed worker to come back before collectCoverage
        # (otherwise its profraw from pre-kill is all we get, and the
        # workers_active count is wrong for any later scenario runs).
        assigned.wait_for_unit("rio-worker.service")

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

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
