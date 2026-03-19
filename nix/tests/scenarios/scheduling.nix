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
#
# Fragment architecture: returns { fragments, mkTest }. default.nix
# composes into 2 parallel VM tests (core, disrupt). fanout → fuse-direct
# → fuse-slowpath chain via FUSE cache state; all else independent.
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
#
# r[verify worker.fuse.lookup-caches]
#   fanout asserts rio_worker_fuse_cache_misses_total ≥1 on each small
#   worker. Nonzero misses prove lookup()→ensure_cached()→materialize
#   ran and the inode→realpath mapping is cached (ops.rs:52+).
#
# r[verify store.inline.threshold]
#   chunks builds a 300 KiB blob (> INLINE_THRESHOLD=256 KiB) and asserts
#   chunk_after > chunk_baseline. Proves put_path.rs:494 nar_data.len()
#   >= INLINE_THRESHOLD gate fired (tiny-text builds go inline).
#
# r[verify obs.metric.transfer-volume]
#   chunks asserts rio_store_put_path_bytes_total delta ≥300000 after
#   bigblob upload. Proves the volume counter (put_path.rs:574) runs on
#   the chunked path.
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
  protoset = import ../lib/protoset.nix { inherit pkgs; };

  # reassign: slow build, no pname → estimator default → "small" class.
  # 25s gives the test ~20s to find+kill the assigned worker before the
  # build would naturally finish.
  reassignDrv = drvs.mkTrivial {
    marker = "sched-reassign";
    sleepSecs = 25;
  };

  # cancel-timing: 300s sleep — cancelled long before natural end. No
  # pname → default "small" class → lands on wsmall1 OR wsmall2. 300s
  # >> 5s budget: if cgroup-gone passes, the kill DID it (not sleep end).
  cancelDrv = drvs.mkTrivial {
    marker = "sched-cancel-timing";
    sleepSecs = 300;
  };

  # max-silent-time: echoes ONCE then sleeps 60s. wlarge has
  # RIO_MAX_SILENT_TIME_SECS=10 (default.nix fixture). The worker's
  # silence select! arm fires ~10s after the echo → TimedOut → cgroup.kill
  # reaps the sleep. 60s sleep proves the kill was at ~10s SILENCE, not
  # 60s wall-clock. pname in env so the test can seed build_history and
  # route to wlarge (same pattern as sizeclass/bigthing). mkTrivial echoes
  # AFTER sleep, so inline a custom drv with echo-then-sleep ordering.
  silenceDrv = pkgs.writeText "drv-sched-silence.nix" ''
    { busybox }:
    derivation {
      name = "rio-sched-silence";
      pname = "rio-sched-silence";
      system = builtins.currentSystem;
      builder = "''${busybox}/bin/sh";
      args = [ "-c" '''
        echo start-silence-marker
        ''${busybox}/bin/busybox sleep 60
        echo unreachable > $out
      ''' ];
    }
  '';

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

  # ── testScript prelude: bootstrap + Python helpers ────────────────────
  # Shared by all fragment compositions. start_all + waitReady + SSH +
  # seed + build() helper + size-class precondition asserts.
  prelude = ''
    ${common.assertions}


    ${common.kvmPreopen}
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
  '';

  # ── Subtest fragments ─────────────────────────────────────────────────
  fragments = {
    fanout = ''
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

          # scheduler_builds_total is per-SubmitBuild, not per-derivation
          # (protocol.nix:122-124). fanout submits once → exactly 1 here.
          # BUT: leaf_count=="4" above is strictly stronger (proves all 5
          # derivations built AND the NAR retrievable). A ≥1 metric check
          # would pass even if 4 leaves silently failed and only the
          # collector cache-hit. Deleted — the content check IS the test.

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
          # (busybox must be fetched before any build runs). lookup()
          # materializes the tree on miss (ops.rs:105 ensure_cached)
          # then caches the inode→realpath mapping for the kernel TTL.
          # Nonzero misses prove the cold-lookup→fetch→cache path ran.
          # wlarge never built anything → never fetched anything.
          for w in small_workers:
              assert_metric_ge(w, 9093,
                  "rio_worker_fuse_cache_misses_total", 1.0)

          # wsmall2 runs with RIO_FUSE_PASSTHROUGH=false (default.nix).
          # Its reads go through the userspace FUSE callback instead of
          # kernel passthrough. fallback_reads_total ≥1 proves fuse/ops.rs
          # read() actually ran — passthrough bypasses it entirely.
          # wsmall1 (passthrough ON) should be near-zero or absent.
          assert_metric_ge(wsmall2, 9093,
              "rio_worker_fuse_fallback_reads_total", 1.0)

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
    '';

    fuse-direct = ''
      # ══════════════════════════════════════════════════════════════════
      # fuse-direct — readdir/access on the FUSE mount (bypasses overlay)
      # ══════════════════════════════════════════════════════════════════
      # chain.nix:43 does `ls -la $dep/` INSIDE the build sandbox (overlay
      # lower 2 = FUSE). ops.rs readdir() stays 0 hits even in lifecycle-k3s
      # — overlayfs is not delegating FUSE_READDIR to the lower. This subtest
      # ls's the FUSE mount DIRECTLY (no overlay) to prove readdir() is
      # reachable at all; if THIS is 0 hits, the problem is fuser/kernel,
      # not overlayfs.
      with subtest("fuse-direct: readdir/access on FUSE mount (overlay bypass)"):
          # Both small workers fetched ≥1 path (asserted above). `ls` on
          # /var/rio/fuse-store (the mount point, not /var/rio/cache) hits
          # FUSE readdir(ino=ROOT) → fs::read_dir(cache_dir).
          for w in small_workers:
              listing = w.succeed("ls -la /var/rio/fuse-store/ 2>&1")
              print(f"{w.name} fuse-store root:\n{listing}")
              # access(R_OK) via faccessat(2). make_fuse_config (fuse/mod.rs
              # :189) does NOT set MountOption::DefaultPermissions → kernel
              # forwards the permission check to userspace access().
              w.succeed("test -r /var/rio/fuse-store/")

          # Subdir readdir — fast path at ops.rs:410 (tree already
          # materialized by a prior lookup). Cache contains BOTH .drv
          # files (regular files) and output dirs; `find -type d` filters
          # to dirs. busybox is always there (fetched as input for the
          # leaves wsmall1 built).
          cached = wsmall1.succeed(
              "find /var/rio/cache/ -mindepth 1 -maxdepth 1 -type d "
              "-printf '%f\\n'"
          ).strip()
          assert cached, "wsmall1 /var/rio/cache/ has no dirs after fanout"
          cached = cached.split("\n")[0]
          sub = wsmall1.succeed(f"ls -la /var/rio/fuse-store/{cached}/ 2>&1")
          print(f"wsmall1 fuse-store/{cached}:\n{sub}")
    '';

    overlay-readdir = ''
      # ══════════════════════════════════════════════════════════════════
      # overlay-readdir-correctness — does ls INSIDE a build see ALL files?
      # ══════════════════════════════════════════════════════════════════
      # fuse-direct above proves readdir() CAN fire. This probes whether
      # the per-build overlay (lowerdir=/nix/store:{fuse}) serves a
      # CORRECT listing. multifile.nix: dep has 5 files, consumer ls's it
      # with a cold overlay dcache (no child names previously looked up).
      # If overlay shortcuts via its own dcache, count<5 = correctness bug.
      with subtest("overlay-readdir-correctness: ls in build sees all files"):
          out = build("${drvs.multifile}", capture_stderr=False).strip()
          count = client.succeed(
              f"nix store cat --store 'ssh-ng://${gatewayHost}' {out}"
          ).strip()
          # count=5: overlay reads the FULL lower listing (correct). May
          #   or may not go through /dev/fuse — coverage tells us which.
          # count<5: overlay serves from dcache (H1). Correctness bug:
          #   builds that `ls` a FUSE-served dep see only names they've
          #   already touched. None of our tests would have caught this
          #   — they all cat known filenames.
          assert count == "5", (
              f"overlay readdir returned {count} entries, expected 5. "
              f"If <5: overlay serves from stale dcache (CORRECTNESS BUG). "
              f"If =5 but ops.rs readdir still 0: overlay reads lower "
              f"via a path that skips /dev/fuse (coverage gap only)."
          )
    '';

    sizeclass = ''
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
    '';

    chunks = ''
      # ══════════════════════════════════════════════════════════════════
      # chunks — 300KiB output forces chunked PutPath, not inline
      # ══════════════════════════════════════════════════════════════════
      #   300 KiB > INLINE_THRESHOLD (256 KiB) → nar_data.len() >= cas::
      #   INLINE_THRESHOLD at put_path.rs:494 is true → chunked path.
      #   chunk_after > chunk_baseline proves the threshold gate fired.
      #   The chunked PutPath increments rio_store_put_path_bytes_total
      #   (put_path.rs:574). bytes_after - bytes_before ≥ 300*1024 proves
      #   the volume counter runs on the chunked path (tiny text-file
      #   builds above probably went inline, so this is the first check
      #   that hits the counter inside the chunked branch).
      with subtest("chunks: 300KiB bigblob writes chunk files to disk"):
          # All builds above are tiny text files, likely inline — a
          # post-build `chunk_count > 0` check would NOT prove the chunked
          # path ran. Capture baseline, build bigblob (300 KiB >
          # INLINE_THRESHOLD = 256 KiB), assert chunk count increased.
          chunk_baseline = int(${gatewayHost}.succeed(
              "find /var/lib/rio/store/chunks -type f 2>/dev/null | wc -l"
          ).strip())
          bytes_before = scrape_metrics(${gatewayHost}, 9092)

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

          # transfer-volume: bigblob is 300 KiB of zeros. NAR framing
          # adds a few hundred bytes of overhead. ≥300000 is a loose
          # floor — chunk dedup doesn't change what PutPath RECEIVES.
          bytes_after = scrape_metrics(${gatewayHost}, 9092)
          b_before = metric_value(bytes_before, "rio_store_put_path_bytes_total") or 0.0
          b_after = metric_value(bytes_after, "rio_store_put_path_bytes_total") or 0.0
          assert b_after - b_before >= 300000, (
              f"expected ≥300000 bytes delta for 300 KiB bigblob upload; "
              f"before={b_before}, after={b_after}, delta={b_after - b_before}"
          )

          # Dedup metric registered + exported (proves chunked PutPath
          # codepath ran at least once; ratio value is irrelevant here).
          metrics = ${gatewayHost}.succeed("curl -s http://localhost:9092/metrics")
          assert "rio_store_chunk_dedup_ratio" in metrics, (
              "rio_store_chunk_dedup_ratio metric should be exported "
              "(chunked PutPath path ran)"
          )
    '';

    reassign = ''
      import threading
      import time as _time

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
    '';

    cgroup = ''
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
    '';

    fuse-slowpath = ''
      # ══════════════════════════════════════════════════════════════════
      # fuse-slowpath — fault-inject cache corruption → open/readlink/
      # readdir slow-paths (ops.rs:217-235, 251-268, 422-437, ~55 lines)
      # ══════════════════════════════════════════════════════════════════
      # lookup() at ops.rs:105 eagerly materializes the WHOLE store-path
      # tree on first access (ensure_cached → fetch NAR → extract). Every
      # subsequent getattr/readlink/open/readdir hits the fast path
      # (file already on disk). The slow paths are: fast-path File::open/
      # read_link/read_dir returns ENOENT → store_basename_for_inode(ino)
      # finds the store path → ensure_cached → retry. Structurally
      # unreachable unless the cache dir is corrupted AFTER lookup
      # populated the kernel's dentry cache but BEFORE the next access.
      #
      # Fault: rm files from /var/rio/cache/<busybox>/ while the kernel
      # still holds valid dentries (ATTR_TTL=3600s from fuse-direct's
      # `ls -la` above). Kernel routes the next open/readlink/readdir to
      # FUSE by cached inode — no fresh lookup — and the real path is gone.
      #
      # ensure_cached won't re-fetch: it checks the SQLite index
      # (cache.rs:342 get_path → get_and_touch), not disk. The index
      # still says busybox is present, so ensure_cached returns Ok and
      # the slow path's SECOND File::open/read_link/read_dir fails again
      # → reply.error(ENOENT). That's the "failed after ensure_cached"
      # branch — defensive code proving the SQLite-vs-disk divergence
      # surfaces as a build error, not a silent hang.
      #
      # getattr slow-path (153-185) NOT hit: kernel caches attrs for
      # ATTR_TTL=3600s after fuse-direct's stat-via-ls-la, doesn't
      # re-ask FUSE within the test's remaining lifetime. open/readlink/
      # readdir don't use the attr cache.
      #
      # DESTRUCTIVE to wsmall1's FUSE cache. After all builds, before
      # collectCoverage — profraws are already on disk, the cache is
      # throwaway.
      with subtest("fuse-slowpath: cache-vs-disk divergence → open/readlink/readdir ENOENT"):
          # Same cache-entry discovery as fuse-direct above. busybox is
          # always there (fetched as input for every build).
          cache_bb = wsmall1.succeed(
              "find /var/rio/cache/ -mindepth 1 -maxdepth 1 -type d "
              "-name '*busybox*' | head -1"
          ).strip()
          assert cache_bb, "no busybox dir in wsmall1 FUSE cache"
          fuse_bb = cache_bb.replace("/var/rio/cache/", "/var/rio/fuse-store/")

          # Fresh ls -la: re-cache the child dentries. fuse-direct's
          # earlier ls -la was the original seed, but reassign above
          # SIGKILLs a worker (possibly wsmall1). AutoUnmount + systemd
          # restart → fresh FUSE mount → kernel dentry cache cleared.
          # cgroup's build re-looked-up bin/sh (so bin IS re-cached —
          # v3 confirmed: readdir slow-path fired, ino=4 = post-restart
          # counter) but never touched default.script/linuxrc. This ls
          # re-lstats every child → fresh dentries for all three targets
          # with ATTR_TTL=3600s.
          wsmall1.succeed(f"ls -la {fuse_bb}/ 2>&1")

          # Delete targets from the CACHE dir. The SQLite index is
          # untouched, so ensure_cached thinks they're still there.
          wsmall1.succeed(
              f"rm -f {cache_bb}/default.script {cache_bb}/linuxrc && "
              f"rm -rf {cache_bb}/bin"
          )

          # Trigger all three slow paths. Kernel has cached dentries
          # (ATTR_TTL=3600s from the ls -la above) → path resolution
          # hits the cached ino without a fresh lookup → FUSE open/readlink/
          # readdir(ino) → File::open/read_link/read_dir on the deleted
          # cache path → ENOENT → store_basename_for_inode(ino) →
          # ensure_cached (SQLite index still says yes, Ok) → SECOND
          # open/readlink/readdir → still ENOENT → tracing::warn! +
          # reply.error. .execute() not .fail(): cat/readlink do exit
          # non-zero, but `ls` exits 0 even though FUSE readdir returned
          # ENOENT (observed v3 2026-03-16: warn fired, ls succeeded —
          # kernel/glibc swallowed the getdents64 error somewhere). The
          # shell exit code is a proxy; the journalctl grep is the proof.
          wsmall1.execute(f"cat {fuse_bb}/default.script 2>&1")
          wsmall1.execute(f"readlink -v {fuse_bb}/linuxrc 2>&1")
          wsmall1.execute(f"ls {fuse_bb}/bin/ 2>&1")

          # THE assertion: the slow-path warn!s fired. Each has a
          # distinctive message (ops.rs:232, 265, 437). If the kernel's
          # dentry cache had expired and a fresh LOOKUP failed instead,
          # we'd see "lookup: not found" (trace, not warn) and these
          # three would be 0 — the slow paths never entered.
          slowpath_warns = wsmall1.succeed(
              "journalctl -u rio-worker --no-pager | "
              "grep -cE 'failed after ensure_cached' || echo 0"
          ).strip()
          assert int(slowpath_warns) >= 3, (
              f"expected ≥3 'failed after ensure_cached' warns (one each "
              f"from open/readlink/readdir slow-paths); got {slowpath_warns}. "
              f"If 0: kernel dentry cache expired → fresh lookup failed at "
              f"ops.rs:138 before the slow paths could run (ATTR_TTL "
              f"assumption wrong, or cache was dropped elsewhere)."
          )

          # Disambiguate: which of the three fired? `| sort | uniq -c`
          # breaks it out by message. readlink/open/readdir each have
          # their own warn text.
          breakdown = wsmall1.succeed(
              "journalctl -u rio-worker --no-pager | "
              "grep 'failed after ensure_cached' | "
              "grep -oE '(open|readlink|readdir) failed' | sort | uniq -c"
          ).strip()
          print(f"fuse-slowpath PASS: {slowpath_warns} total slow-path warns\n{breakdown}")
    '';

    # r[verify worker.silence.timeout-kill]
    max-silent-time = ''
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

          # client.fail: TimedOut is a build FAILURE (nix-build exits nonzero).
          t0 = _time.monotonic()
          out = client.fail(
              "nix-build --no-out-link --store 'ssh-ng://${gatewayHost}' "
              "--arg busybox '(builtins.storePath ${common.busybox})' "
              "${silenceDrv} 2>&1"
          )
          elapsed = _time.monotonic() - t0

          # Timing proof. 10s silence + daemon-setup + QEMU/SSH overhead
          # → expect ~12-25s. 45s upper bound is <<60s (the key constraint).
          # Lower bound 8s: the silence arm can't fire before the 10s
          # deadline; if elapsed<8s the failure was something else (eval
          # error, wrong-worker routing, immediate daemon crash).
          assert 8 < elapsed < 45, (
              f"expected silence kill at ~10s (wall-clock ~12-25s), "
              f"got {elapsed:.1f}s. If ~60s: silence arm never fired, "
              f"sleep ran to completion (routed to a worker without "
              f"RIO_MAX_SILENT_TIME_SECS?). If <8s: failed before silence "
              f"deadline.\nBuild output:\n{out}"
          )

          # wlarge must have logged the silence warn. journalctl grep is
          # the end-to-end proof that RIO (not the local nix-daemon) fired.
          # If this is 0 but elapsed is in-range, the nix-daemon enforced
          # it instead — rio-side backstop didn't fire (impl bug).
          warn_count = wlarge.succeed(
              "journalctl -u rio-worker --no-pager | "
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
                  "journalctl -u rio-worker --no-pager | "
                  "grep 'rio-sched-silence'"
              )

          print(f"max-silent-time PASS: killed at {elapsed:.1f}s wall-clock (drv sleep was 60s)")
    '';

    cancel-timing = ''
      import time as _time

      # ══════════════════════════════════════════════════════════════════
      # cancel-timing — gRPC CancelBuild mid-exec → cgroup gone within 5s
      # ══════════════════════════════════════════════════════════════════
      # Same cancel chain as lifecycle.nix:cancel-cgroup-kill but on the
      # standalone fixture (no k3s, plaintext gRPC :9001) and a TIGHTER
      # budget: 5s vs 30s. The 5s budget < 10s prom scrape interval —
      # cgroup-gone MUST be asserted via direct `test` on the worker VM,
      # NOT via a prometheus scrape. A `derivations_running==0` prom
      # check would race the scrape and flake.
      #
      # Flow: CancelBuild RPC → scheduler handle_cancel_build →
      # cancel_signals_total++ → CancelSignal over worker stream →
      # runtime.rs try_cancel_build → fs::write(cgroup.kill, "1") →
      # kernel SIGKILLs tree → executor drain → BuildCgroup::Drop rmdirs.
      # lifecycle.nix observed <1.5s; 5s is comfortable on local VMs.
      #
      # SubmitBuild via gRPC, NOT ssh-ng://: ssh-ng doesn't surface
      # build_id to the nix client. And client-disconnect mid-
      # wopBuildDerivation does NOT trigger CancelBuild — session.rs:107's
      # EOF-cancel path fires only BETWEEN opcodes; mid-opcode the build
      # handler removes the id before bubbling (handler/build.rs:462), so
      # active_build_ids is empty by the time the outer loop could see it.
      with subtest("cancel-timing: CancelBuild → cgroup gone within 5s"):
          cancel_before = scrape_metrics(${gatewayHost}, 9091)

          # Instantiate on client (has a local store), copy .drv to
          # rio-store so the scheduler can find it by path.
          drv_path = client.succeed(
              "nix-instantiate "
              "--arg busybox '(builtins.storePath ${common.busybox})' "
              "'${cancelDrv}' 2>/dev/null"
          ).strip()
          client.succeed(
              f"nix copy --derivation --to 'ssh-ng://${gatewayHost}' {drv_path}"
          )

          # SubmitBuild via plaintext gRPC :9001 (withPki=false here).
          # Streams BuildEvents; first event carries buildId. -max-time 5
          # caps the stream read — the 300s build won't finish, grpcurl
          # exits DeadlineExceeded. `|| true` swallows that; build is
          # already persisted in PG, stream was just observability.
          #
          # DerivationNode: drvHash=drvPath for input-addressed (gateway
          # translate.rs:361 does the same). system is fixture platform.
          # outputNames=["out"] — mkTrivial's single output. No deps.
          #
          # json.dumps: produces double-quoted JSON, safe inside the
          # single-quoted shell -d '...'.
          submit_payload = json.dumps({
              "nodes": [{
                  "drvPath": drv_path,
                  "drvHash": drv_path,
                  "system": "${pkgs.stdenv.hostPlatform.system}",
                  "outputNames": ["out"],
              }],
              "edges": [],
          })
          submit_out = ${gatewayHost}.succeed(
              f"grpcurl -plaintext -max-time 5 "
              f"-protoset ${protoset}/rio.protoset "
              f"-d '{submit_payload}' "
              f"localhost:9001 rio.scheduler.SchedulerService/SubmitBuild "
              f"2>&1 || true"
          )
          brace = submit_out.find("{")
          assert brace >= 0, (
              f"no JSON in SubmitBuild output — submit failed?\n"
              f"{submit_out[:500]!r}"
          )
          first_ev, _ = json.JSONDecoder().raw_decode(submit_out, brace)
          build_id = first_ev.get("buildId", "")
          assert build_id, (
              f"first BuildEvent missing buildId; got: {first_ev!r}"
          )
          print(f"cancel-timing: submitted, build_id={build_id}")

          # Wait for cgroup — THIS IS the phase=Building signal (daemon
          # spawned, cgroup created, sleep started). sanitize_build_id
          # (executor/mod.rs:973) = basename with . → _, so the cgroup
          # name ends "-sched-cancel-timing_drv".
          #
          # No-pname drv → default "small" class → wsmall1 OR wsmall2.
          # Probe both; first hit wins. DelegateSubgroup=builds puts
          # per-build cgroups as SIBLINGS of builds/ under the service
          # cgroup (worker.nix:180). `find -print -quit` stops at first
          # match; `| grep .` fails on empty output (find exits 0 on
          # no-match) so the Python-side rc check works.
          cgroup_parent = "/sys/fs/cgroup/system.slice/rio-worker.service"
          assigned = None
          cgroup_path = None
          for _ in range(30):
              for w in small_workers:
                  rc, out = w.execute(
                      f"find {cgroup_parent} -maxdepth 1 -type d "
                      f"-name '*sched-cancel-timing_drv' -print -quit "
                      f"2>/dev/null | grep ."
                  )
                  if rc == 0 and out.strip():
                      assigned = w
                      cgroup_path = out.strip()
                      break
              if assigned:
                  break
              _time.sleep(1)
          assert assigned is not None, (
              "no small worker created a cgroup for cancelDrv within 30s"
          )
          print(f"cancel-timing: assigned={assigned.name}, cgroup={cgroup_path}")

          # Precondition: cgroup.procs NON-EMPTY. If empty, the build
          # isn't actually running in the cgroup and the 5s gone-assert
          # below proves nothing (could vanish for any reason). Without
          # this the test would pass on a broken add_process().
          procs_before = int(assigned.succeed(
              f"wc -l < {cgroup_path}/cgroup.procs"
          ).strip())
          assert procs_before > 0, (
              f"cgroup.procs empty at {cgroup_path} — build not running "
              f"in the cgroup? cancel-gone assertion would be vacuous."
          )

          # Cancel. Clock the end-to-end latency.
          t0 = _time.monotonic()
          cancel_payload = json.dumps({
              "buildId": build_id,
              "reason": "vm-test-cancel-timing",
          })
          ${gatewayHost}.succeed(
              f"grpcurl -plaintext "
              f"-protoset ${protoset}/rio.protoset "
              f"-d '{cancel_payload}' "
              f"localhost:9001 rio.scheduler.SchedulerService/CancelBuild"
          )

          # PRIMARY: cgroup GONE within 5s — DIRECT probe, NOT prom.
          # Kernel rejects rmdir on non-empty cgroup → gone ⇒ procs
          # emptied ⇒ kill landed. 300s sleep hasn't completed (elapsed
          # < 5s ≪ 300s) so removal PROVES the cancel chain ran.
          try:
              assigned.wait_until_succeeds(
                  f"! test -e {cgroup_path}",
                  timeout=5,
              )
          except Exception:
              procs_after = assigned.execute(
                  f"cat {cgroup_path}/cgroup.procs 2>/dev/null | wc -l"
              )[1].strip()
              dump_all_logs([${gatewayHost}] + all_workers)
              print(f"cancel-timing DIAG: procs_after={procs_after} "
                    f"(was {procs_before}), build_id={build_id}")
              raise
          elapsed = _time.monotonic() - t0
          print(f"cancel-timing: cgroup gone in {elapsed:.2f}s "
                f"(budget 5s, sleep was 300s)")

          # Worker logged the kill path (runtime.rs:238). No kubelet
          # http2-stream flake here (standalone journald, not k8s).
          assigned.succeed(
              "journalctl -u rio-worker --no-pager | "
              "grep 'build cancelled via cgroup.kill'"
          )

          # cancel_signals_total delta ≥1. Monotone counter — post-hoc
          # scrape is fine. The 5s<10s caveat is about tight-window
          # gauge assertions, not counters read after the fact.
          cancel_after = scrape_metrics(${gatewayHost}, 9091)
          c_before = metric_value(cancel_before,
              "rio_scheduler_cancel_signals_total") or 0.0
          c_after = metric_value(cancel_after,
              "rio_scheduler_cancel_signals_total") or 0.0
          assert c_after >= c_before + 1, (
              f"CancelBuild should increment cancel_signals_total by >=1; "
              f"before={c_before}, after={c_after}"
          )
    '';

    load-50drv = ''
      # ══════════════════════════════════════════════════════════════════
      # load-50drv — 50-leaf fanout dispatches + completes cleanly
      # ══════════════════════════════════════════════════════════════════
      # 50 independent leaves + 1 collector = 51 derivations. All 50
      # leaves Ready simultaneously on SubmitBuild. No-pname → default
      # "small" class → 4 slots (wsmall1:2 + wsmall2:2; wlarge idle).
      # ~13 dispatch waves. Proves: (a) scheduler doesn't stall or
      # deadlock under bulk-ready load, (b) every derivation gets
      # dispatched — no leaks in the ready-set, (c) store handles 50
      # near-back-to-back PutPath calls.
      #
      # Fanout NOT linear chain: 50 serial builds at tickIntervalSecs=2
      # ≈ 150-200s and exercises nothing interesting (serial dispatch is
      # the same as 1 build, 50 times). Fanout exercises the actual
      # load concern (bulk ready-set) in ~40-60s.
      with subtest("load-50drv: 50-leaf fanout completes, assignments +≥50"):
          assign_before = scrape_metrics(${gatewayHost}, 9091)

          out = build("${drvs.fiftyFanout}", capture_stderr=False).strip()
          assert out.startswith("/nix/store/"), f"unexpected output: {out!r}"

          # Content check — strongest proof, same pattern as fanout.
          # Collector stamp has "rio-load-root" + 50 "rio-load-N" lines.
          # grep '^rio-load-[0-9]' matches only the leaves (root has no
          # digit after the dash). EXACT 50: proves (a) all 50 built,
          # (b) all 50 PutPath'd, (c) collector read all 50 via FUSE,
          # (d) NAR bytes intact end-to-end. A 49 would mean the
          # scheduler dropped a ready derivation silently.
          leaf_count = client.succeed(
              f"nix store cat --store 'ssh-ng://${gatewayHost}' {out}/stamp | "
              f"grep -c '^rio-load-[0-9]'"
          ).strip()
          assert leaf_count == "50", (
              f"expected exactly 50 'rio-load-N' lines in {out}/stamp, "
              f"got {leaf_count}. Scheduler dropped a ready derivation? "
              f"Or PutPath/FUSE corruption under load?"
          )

          # assignments_total delta ≥50. The plan doc called for
          # rio_scheduler_derivations_completed_total but NO SUCH METRIC
          # EXISTS (scheduler lib.rs registers builds_total, derivations_
          # queued/running, assignments_total — no completed counter).
          # assignments_total increments once per dispatch at
          # dispatch.rs:482. ≥50 not ==51: reassign above may have
          # caused a re-dispatch, and a leaf could (rarely) cache-hit
          # if its input closure matches an earlier fragment's build.
          # The leaf_count==50 content check above is strictly stronger
          # anyway — proves completion, not just dispatch.
          assign_after = scrape_metrics(${gatewayHost}, 9091)
          a_before = metric_value(assign_before,
              "rio_scheduler_assignments_total") or 0.0
          a_after = metric_value(assign_after,
              "rio_scheduler_assignments_total") or 0.0
          assert a_after >= a_before + 50, (
              f"50-drv fanout should increment assignments_total by >=50; "
              f"before={a_before}, after={a_after}, "
              f"delta={a_after - a_before}"
          )

          print(f"load-50drv PASS: leaf_count=50, "
                f"assignments delta={a_after - a_before:.0f}")
    '';

  };

  # fanout populates the FUSE cache that fuse-direct and fuse-slowpath
  # read. fuse-slowpath is DESTRUCTIVE (rm from cache) — must run last.
  assertChains =
    subtests:
    let
      inherit (pkgs) lib;
      idx = name: lib.lists.findFirstIndex (s: s == name) (-1) subtests;
      has = name: builtins.elem name subtests;
      last = builtins.elemAt subtests (builtins.length subtests - 1);
    in
    assert lib.assertMsg (
      !(has "fuse-direct") || (has "fanout" && idx "fanout" < idx "fuse-direct")
    ) "scheduling: fuse-direct requires fanout earlier (FUSE cache state)";
    assert lib.assertMsg (
      !(has "fuse-slowpath") || (has "fanout" && idx "fanout" < idx "fuse-slowpath")
    ) "scheduling: fuse-slowpath requires fanout earlier (busybox in cache)";
    assert lib.assertMsg (
      !(has "fuse-slowpath") || last == "fuse-slowpath"
    ) "scheduling: fuse-slowpath is destructive (cache rm) — must run LAST";
    true;

  # Coverage mode: graceful-stop + profraw tar + copy_from_vm. Additive so
  # explicit globalTimeout overrides stack. Normal-mode CI budget unchanged.
  covTimeoutHeadroom = if common.coverage then 300 else 0;

  mkTest =
    {
      name,
      subtests,
      globalTimeout ? 600,
    }:
    assert assertChains subtests;
    pkgs.testers.runNixOSTest {
      name = "rio-scheduling-${name}";
      skipTypeCheck = true;
      globalTimeout = globalTimeout + covTimeoutHeadroom;
      inherit (fixture) nodes;
      testScript = ''
        ${prelude}
        ${pkgs.lib.concatMapStrings (s: fragments.${s} + "\n") subtests}
        ${common.collectCoverage fixture.pyNodeVars}
      '';
    };
in
{
  inherit fragments mkTest;
}
