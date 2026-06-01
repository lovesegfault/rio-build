# Chaos scenario: 4 transport-fault subtests via toxiproxy.
#
# Each subtest injects a single toxic into one of the two proxies
# (scheduler_store or worker_store), drives a build, asserts the
# graceful-degradation path fired, then removes the toxic. Subtests
# are sequentially independent — each cleans up before the next.
#
# | # | toxic       | proxy            | exercises                        |
# |---|-------------|------------------|----------------------------------|
# | 1 | latency     | scheduler_store  | cache-check RPC under slowness   |
# | 2 | reset_peer  | worker_store     | upload retry loop (upload.rs)    |
# | 3 | timeout     | scheduler_store  | cache-check breaker (merge.rs)   |
# | 4 | bandwidth   | worker_store     | large-NAR streaming under cap    |
#
# Caller wires `fixture = toxiproxy { }` (see default.nix). The fixture
# guarantees scheduler's store_client is Some (proxy up before scheduler
# boot — see toxiproxy.nix's waitReady hard-check).
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) gatewayHost;
  drvs = import ../lib/derivations.nix { inherit pkgs; };

  # ── Per-subtest derivations ──────────────────────────────────────────
  # Distinct markers → distinct .drv hashes → no DAG dedup between
  # subtests. Each build is a fresh dispatch + fresh upload.

  # Subtest 1: trivial, just needs to complete under latency.
  latencyDrv = drvs.mkTrivial { marker = "chaos-latency"; };

  # Subtest 2: two-output derivation. reset_peer mid-upload means one
  # (or both) output's PutPath sees a RST; upload_output's retry loop
  # (upload.rs:197) re-reads disk and re-sends. Two outputs also
  # exercises MAX_PARALLEL_UPLOADS (upload.rs:36) with concurrent
  # streams hitting the same toxic.
  #
  # P0267 cross-check: when PutPathBatch lands, multi-output uploads
  # switch to a single stream. This drv's retry semantics will shift
  # (one stream reset → whole batch retried). Until then, each output
  # retries independently.
  twoOutputDrv = drvs.mkCustom {
    name = "rio-test-chaos-reset";
    extraAttrs.outputs = [
      "out"
      "dev"
    ];
    script = ''
      echo chaos-reset-out > $out
      echo chaos-reset-dev > $dev
    '';
  };

  # Subtest 3: trivial. Partition is on scheduler_store only — the
  # worker path is unaffected, so any build that gets past cache-check
  # completes normally.
  partitionDrv = drvs.mkTrivial { marker = "chaos-partition"; };
  # Second build for the post-heal half: proves RPC works again.
  partitionHealDrv = drvs.mkTrivial { marker = "chaos-partition-heal"; };

  # Subtest 4: ~2MiB output via dd from /dev/urandom. At 125 KB/s
  # (1 Mbps), the NAR upload takes ~16s — long enough to prove the
  # bandwidth toxic is biting (no-toxic baseline would be <1s on the
  # vlan) without pushing against globalTimeout. urandom is in the Nix
  # sandbox's /dev allowlist. Non-determinism is fine here: the .drv
  # hash is what's cached, not the output content, and this test runs
  # the build exactly once.
  largeOutputDrv = drvs.mkCustom {
    name = "rio-test-chaos-bandwidth";
    script = ''
      ''${busybox}/bin/busybox dd if=/dev/urandom of=$out bs=1048576 count=2
    '';
  };

  # Subtest 5: chatty ~90s build. 850 iterations x 0.1s sleep, 130 fat
  # lines per iteration (~1300 lines/s — far under the 250k lines/s
  # rate limit): size-driven batching (64-line batches, ~20/s) fills
  # the full absorption chain — 256-slot sink + 256-slot gRPC outbound
  # + frozen h2/TCP windows (~540 batches) — about 27s into the 60s
  # scheduler stall, so the shed counter is decisively nonzero at the
  # in-stall scrape rather than racing the pipeline depth.
  stallSurvivorDrv = drvs.mkCustom {
    name = "rio-test-chaos5stallmark";
    script = ''
      i=0
      while [ $i -lt 850 ]; do
        j=0
        while [ $j -lt 130 ]; do
          echo "chaos5stallmark-line-$i-$j-................................................................................................................................"
          j=$((j+1))
        done
        # CPU-positive busywork: the survives-the-stall assertion reads
        # cgroup cpu.stat deltas, and a purely sleep-paced script runs
        # too close to the frozen baseline to discriminate.
        k=0
        while [ $k -lt 400 ]; do
          k=$((k+1))
        done
        ''${busybox}/bin/busybox sleep 0.1
        i=$((i+1))
      done
      echo chaos-stall-done > $out
    '';
  };

  # Subtest 6: endlessly chatty build (chaos6forevermark in argv); only
  # the executor's timeout kill ever ends it.
  stallForeverDrv = drvs.mkCustom {
    name = "rio-test-chaos6forevermark";
    script = ''
      while true; do
        echo "chaos6forevermark-line-............................................................"
        ''${busybox}/bin/busybox sleep 0.05
      done
    '';
  };
in
pkgs.testers.runNixOSTest {
  name = "rio-chaos";
  skipTypeCheck = true;
  # Boot ~60s + subtest 1 ~10s + subtest 2 ~30s (journal-poll + retry
  # backoff) + subtest 3 ~15s (5s toxic-close + build) + subtest 4 ~30s
  # (dd + 16s throttled upload) + subtest 5 ~130s (85s build spanning a
  # 60s scheduler stall + post-resume waits) + subtest 6 ~135s (worker
  # restart + 75s stall with the kill assert inside it) + margin.
  globalTimeout = 840 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.mkBootstrap {
      inherit fixture gatewayHost;
      withSeed = true;
    }}

    import time

    store_url = "ssh-ng://${gatewayHost}"
    busybox_arg = "--arg busybox '(builtins.storePath ${common.busybox})'"

    # Proxies exist and are toxic-free at start (fixture creates them
    # via -config JSON with no toxics). Belt check — if this fails, the
    # fixture's waitReady already passed wait_for_open_port(19002/29002)
    # but the proxy NAME is wrong (toxiproxy-cli can't find it).
    control.succeed("toxiproxy-cli inspect scheduler_store")
    control.succeed("toxiproxy-cli inspect worker_store")

    # ══════════════════════════════════════════════════════════════════
    # Subtest 1: scheduler↔store latency 500ms — builds complete
    # ══════════════════════════════════════════════════════════════════
    # latency toxic adds a fixed delay to every proxied packet. 500ms
    # one-way → ~1s round-trip added to scheduler's find_missing_paths
    # RPC (merge.rs:553). DEFAULT_GRPC_TIMEOUT is 30s (grpc.rs:15) —
    # 1s is well under; the RPC is slow but succeeds. No breaker trip,
    # no retry. Build completes normally.
    #
    # downstream direction (default): delay on store→scheduler response.
    # upstream would delay request; either proves the point. Using
    # downstream matches what a slow DB / slow disk on the store would
    # look like (response latency, not request latency).
    # toxiproxy-cli arg order: flags BEFORE proxy name. urfave/cli stops
    # flag parsing at the first positional — `toxic add scheduler_store
    # -t latency` parses scheduler_store as the proxy name and silently
    # drops -t/-a/-n as extra positionals → creates a toxic with empty
    # type → "toxic type required". Verified against v2.12.0.
    with subtest("latency 500ms on scheduler_store: build completes"):
        control.succeed(
            "toxiproxy-cli toxic add "
            "-t latency -a latency=500 -n lat scheduler_store"
        )
        try:
            out = client.succeed(
                f"nix-build --no-out-link --store '{store_url}' "
                f"{busybox_arg} ${latencyDrv}"
            ).strip()
            assert out.startswith("/nix/store/"), f"unexpected output: {out!r}"
            assert "chaos-latency" in out, f"wrong drv built: {out!r}"
            # Zero cache-check failures — latency is slow, not failed.
            # Metric not yet registered = zero (metrics-rs registers on
            # first increment). None from scrape_metrics → not present
            # → no failures.
            m = scrape_metrics(control, 9091)
            failures = metric_value(m, "rio_scheduler_cache_check_failures_total") or 0.0
            assert failures == 0.0, (
                f"latency should not fail cache-check, got {failures} failures"
            )
        finally:
            control.succeed("toxiproxy-cli toxic remove -n lat scheduler_store")

    # ══════════════════════════════════════════════════════════════════
    # Subtest 2: worker↔store reset mid-PutPath — upload retries succeed
    # ══════════════════════════════════════════════════════════════════
    # reset_peer sends RST after `timeout` ms of a connection existing.
    # With timeout=500, each PutPath connection gets RST 500ms after
    # open — long enough for the gRPC handshake + first few chunks, so
    # the reset is genuinely mid-stream (not at-open, which would look
    # like a connect failure to tonic).
    #
    # upload.rs:197 retry loop: 3 attempts, backoff 1s/2s.
    #   attempt 0 @ t=0    → RST @ t≈0.5s
    #   attempt 1 @ t≈1.5s → RST @ t≈2.0s
    #   attempt 2 @ t≈4.0s → succeeds IF toxic removed by then
    #
    # We don't time the heal blindly. Instead: poll worker journal for
    # "upload attempt failed" (upload.rs:240), THEN heal. The 1s+2s
    # backoff gives ~3s of slack between first-failure-logged and
    # attempt-2-starts — enough for wait_until_succeeds (1s poll) +
    # the heal SSH round-trip (~0.5s) to land.
    with subtest("reset_peer on worker_store: upload retries succeed"):
        # Timestamp mark: journal filter for this subtest only. Earlier
        # subtests don't touch worker_store, so there should be no prior
        # upload failures, but the mark makes that structural not hopeful.
        mark = worker.succeed("date +%s").strip()

        control.succeed(
            "toxiproxy-cli toxic add "
            "-t reset_peer -a timeout=500 -n rst worker_store"
        )

        # Background the build. `&` + pid capture. stdout (the output
        # path) to a file for later assertion. stderr to the same file
        # so failures are diagnosable.
        client.succeed(
            f"nix-build --no-out-link --store '{store_url}' "
            f"{busybox_arg} ${twoOutputDrv} "
            ">/tmp/chaos-rst.out 2>&1 & echo $! >/tmp/chaos-rst.pid"
        )

        # Wait for the toxic to bite. The 500ms reset_peer fires during
        # the worker's FIRST worker_store use — input-metadata-fetch
        # (runtime.rs), not upload. "build execution failed" at ERROR
        # with ConnectionReset in the error chain. The scheduler
        # re-dispatches (generation stays 1, same drv re-assigned) so
        # the overall retry path is scheduler→worker redispatch, not
        # the upload.rs internal loop. Grep both: if timing jitter lets
        # metadata-fetch through, the upload-retry path fires instead.
        worker.wait_until_succeeds(
            f"journalctl -u rio-builder --since=@{mark} --no-pager | "
            "grep -E 'upload attempt failed|input metadata fetch failed' >/dev/null",
            timeout=30,
        )

        # Heal. The backoff between attempt 0 and attempt 2 is ~3s;
        # we're racing that window but the poll loop above returns
        # ~1-2s after the first failure (1s poll interval + SSH latency)
        # leaving ~1-2s for this command. Tight but deterministic: even
        # if attempt 1 also fails (toxic still active), attempt 2 at
        # t≈4s sees the heal.
        control.succeed("toxiproxy-cli toxic remove -n rst worker_store")

        # Wait for the background build to finish. `! kill -0` succeeds
        # when the pid is gone. Shell `$(cat ...)` re-reads pid each poll.
        client.wait_until_succeeds(
            "! kill -0 $(cat /tmp/chaos-rst.pid) 2>/dev/null",
            timeout=60,
        )

        # Build produced a store path → retries succeeded. Two outputs
        # → two lines. Last line is the primary `out` (nix-build prints
        # outputs in declaration order, but --no-out-link with multi-
        # output prints one path per line).
        result = client.succeed("cat /tmp/chaos-rst.out")
        paths = [l for l in result.splitlines() if l.startswith("/nix/store/")]
        assert len(paths) >= 1, (
            f"expected ≥1 store path in build output, got:\n{result}"
        )
        assert any("chaos-reset" in p for p in paths), (
            f"wrong drv built: {paths!r}"
        )

        # Worker logged at least one retry. Two possible paths:
        # - upload.rs:200-205 "retrying upload" if reset bit during upload
        # - scheduler redispatch → ≥2 "received work assignment" for the
        #   same drv (runtime.rs work loop) if reset bit during metadata
        #   fetch and the scheduler re-assigned
        # Either proves the retry mechanism iterated after the RST.
        worker.succeed(
            f"journalctl -u rio-builder --since=@{mark} --no-pager | "
            "grep 'retrying upload' >/dev/null || "
            f"[ $(journalctl -u rio-builder --since=@{mark} --no-pager | "
            "grep -c 'received work assignment.*chaos-reset') -ge 2 ]"
        )

        # NO exhausted uploads — UploadError::UploadExhausted's Display
        # ("upload failed after N retries for ...") would appear in
        # journald if any output hit the retry ceiling. Per-process
        # counter resets on one-shot restart, so journald is the signal.
        rc, out = worker.execute(
            f"journalctl -u rio-builder --since=@{mark} --no-pager "
            "| grep -c 'upload failed after'"
        )
        exhausted = int(out.strip() or "0")
        assert exhausted == 0, (
            f"upload retries exhausted ({exhausted}× per journald) — "
            f"heal too slow"
        )

    # ══════════════════════════════════════════════════════════════════
    # Subtest 3: scheduler↔store partition — builds queue, resume
    # ══════════════════════════════════════════════════════════════════
    # timeout toxic: "stop all data, close connection after N ms".
    # With timeout=5000, scheduler's find_missing_paths sends its
    # request → toxic swallows it → 5s later the proxy RSTs. tonic
    # sees the RST → Ok(Err(Unavailable)) branch at merge.rs:563.
    #
    # CacheCheckBreaker (breaker.rs): 5 consecutive failures to trip.
    # One failure → record_failure() returns false → merge.rs:574
    # "under threshold: proceed with empty cache-hit set". Build
    # dispatches to worker with no cache optimization. Worker's
    # upload goes through worker_store (no toxic) → succeeds.
    #
    # This is the "builds queue, resume" behavior: the build doesn't
    # FAIL under partition, it runs without the cache shortcut. The
    # ~5s hang IS the queue (scheduler's actor loop is blocked on the
    # RPC until the RST arrives — merge.rs:543 comment).
    #
    # 5s not 30s: we want the RST (Ok(Err)) branch, not the tokio
    # timeout (Err(_)) branch at merge.rs:576. Both branches do the
    # same thing (record_failure + proceed-empty), but 30s would
    # blow the test budget. The toxic's 5s close beats scheduler's
    # 30s DEFAULT_GRPC_TIMEOUT → RST branch fires.
    with subtest("partition on scheduler_store: build degrades gracefully"):
        before = scrape_metrics(control, 9091)

        control.succeed(
            "toxiproxy-cli toxic add "
            "-t timeout -a timeout=5000 -n part scheduler_store"
        )

        # Build under partition. Blocks ~5s on cache-check, then
        # proceeds. Single foreground succeed — no backgrounding
        # needed (the partition self-heals at the connection level
        # via toxic timeout=5000; we don't need to race a heal).
        t0 = time.monotonic()
        out = client.succeed(
            f"nix-build --no-out-link --store '{store_url}' "
            f"{busybox_arg} ${partitionDrv}"
        ).strip()
        elapsed = time.monotonic() - t0
        assert out.startswith("/nix/store/"), f"unexpected: {out!r}"
        assert "chaos-partition" in out, f"wrong drv: {out!r}"

        # Loose lower bound: the 5s toxic-close delay should be
        # observable. Under ~3s would mean the cache-check was
        # skipped entirely (store_client = None — fixture ordering
        # broke) and we're not testing what we think. VM timing is
        # noisy so this is >3 not >4.5.
        assert elapsed > 3.0, (
            f"build completed in {elapsed:.1f}s — cache-check never blocked; "
            "scheduler store_client = None? check fixture boot order"
        )

        control.succeed("toxiproxy-cli toxic remove -n part scheduler_store")

        # Exactly one cache-check failure. None → 0.0 for the delta
        # (metric not registered before the first increment).
        after = scrape_metrics(control, 9091)
        before_v = metric_value(before, "rio_scheduler_cache_check_failures_total") or 0.0
        after_v = metric_value(after, "rio_scheduler_cache_check_failures_total") or 0.0
        assert after_v - before_v == 1.0, (
            f"expected exactly 1 cache-check failure, "
            f"got {after_v} - {before_v} = {after_v - before_v}"
        )

        # Post-heal: second build, cache-check RPC succeeds (no toxic).
        # Breaker's record_success() (merge.rs:560) resets the counter.
        # No new failures.
        out2 = client.succeed(
            f"nix-build --no-out-link --store '{store_url}' "
            f"{busybox_arg} ${partitionHealDrv}"
        ).strip()
        assert "chaos-partition-heal" in out2, f"wrong drv: {out2!r}"
        final = scrape_metrics(control, 9091)
        final_v = metric_value(final, "rio_scheduler_cache_check_failures_total") or 0.0
        assert final_v == after_v, (
            f"post-heal build added failures: {final_v} != {after_v}"
        )

    # ══════════════════════════════════════════════════════════════════
    # Subtest 4: worker↔store bandwidth 1Mbps — large NAR completes
    # ══════════════════════════════════════════════════════════════════
    # bandwidth toxic: rate in KB/s. 125 KB/s = 1 Mbit/s.
    #
    # --upstream flag: direction is client→server from toxiproxy's
    # perspective. worker is the client, store is the upstream. PutPath
    # streams NAR chunks worker→store, so the cap must be on the
    # upstream direction. Default is downstream (store→worker response
    # bytes) which would only throttle the tiny ack.
    #
    # 2MiB output → ~2.1MiB NAR (framing overhead) → ~17s at 125 KB/s.
    # Unconstrained vlan would be <1s. The build completing within
    # timeout but taking >10s proves the cap is biting AND the stream
    # doesn't stall out (no idle-timeout on the gRPC stream; it's
    # throughput-limited, not stuck).
    with subtest("bandwidth 1Mbps on worker_store: large upload completes"):
        control.succeed(
            "toxiproxy-cli toxic add "
            "-t bandwidth -a rate=125 --upstream -n bw worker_store"
        )
        try:
            t0 = time.monotonic()
            out = client.succeed(
                f"timeout 120 nix-build --no-out-link --store '{store_url}' "
                f"{busybox_arg} ${largeOutputDrv}"
            ).strip()
            elapsed = time.monotonic() - t0
            assert out.startswith("/nix/store/"), f"unexpected: {out!r}"
            assert "chaos-bandwidth" in out, f"wrong drv: {out!r}"

            # 2MiB @ 125KB/s ≈ 16s upload alone. dd + build dispatch
            # add ~2-5s. Lower bound 8s (half the theoretical upload)
            # catches "toxic not applied / wrong direction" without
            # being brittle to VM overhead.
            assert elapsed > 8.0, (
                f"large-NAR build took {elapsed:.1f}s — bandwidth toxic "
                "not biting; wrong direction or rate not applied"
            )
            # Upper bound: 120s is the shell `timeout` above; this
            # bound is redundant-but-informative (timing in assert msg).
            assert elapsed < 90.0, (
                f"large-NAR build took {elapsed:.1f}s — stall, not throttle"
            )
        finally:
            control.succeed("toxiproxy-cli toxic remove -n bw worker_store")

    # ══════════════════════════════════════════════════════════════════
    # Subtest 5: scheduler SIGSTOP 60s — running build survives
    # ══════════════════════════════════════════════════════════════════
    # The control/data-plane separation pin at system level: with the
    # sole scheduler frozen (TCP up, process dead-silent), the worker's
    # display stream sheds instead of backing up into the executor, the
    # build keeps consuming CPU mid-stall, and the client gets its store
    # path after recovery. Flake discipline: structural signals only —
    # cgroup cpu.stat, process existence, worker metrics; no scheduler
    # health probes during the stall (its PG lease may lapse and
    # re-acquire after SIGCONT; heartbeats time out and retry).
    with subtest("sched-stall-build-survives: CPU advances and build completes"):
        # Detached launch, the in-scenario proven shape (subtest 2):
        # the redirections apply to the WHOLE backgrounded group, so no
        # copy of the test backdoor's fds survives in the job and
        # `succeed` returns immediately. (A transient systemd unit
        # would detach too, but loses the login env the ssh-ng store
        # client needs; a `( ... ) &` with inner-only redirections
        # blocks `succeed` for the build's whole runtime.)
        client.succeed(
            "rm -f /tmp/chaos5.log; "
            f"{{ timeout 400 nix-build --no-out-link --store '{store_url}' "
            f"{busybox_arg} ${stallSurvivorDrv}; echo rc=$?; }} "
            "> /tmp/chaos5.log 2>&1 < /dev/null &"
        )
        # Dispatch confirmed structurally: rio-builder creates the
        # per-build cgroup (named from the sanitized drv path) on the
        # HOST when the build starts — host pgrep cannot see the
        # PID-namespaced build script, so the cgroup is the worker-side
        # dispatch signal. Generous budget: the worker slot is busy
        # with the previous subtest's teardown for a few seconds.
        worker.wait_until_succeeds(
            "find /sys/fs/cgroup -type d -name '*chaos5stallmark*' "
            "| grep -q .",
            timeout=180,
        )
        cg = worker.succeed(
            "find /sys/fs/cgroup -type d -name '*chaos5stallmark*' "
            "| head -1"
        ).strip()
        worker.succeed(f"test -r {cg}/cpu.stat")

        control.succeed("systemctl kill --signal=SIGSTOP rio-scheduler")
        try:
            time.sleep(30)
            cpu1 = int(worker.succeed(
                f"awk '/^usage_usec/ {{print $2}}' {cg}/cpu.stat"
            ).strip())
            time.sleep(20)
            cpu2 = int(worker.succeed(
                f"awk '/^usage_usec/ {{print $2}}' {cg}/cpu.stat"
            ).strip())
            # Pre-isolation: sink(256) + event channel + chunk channel +
            # pipe fill → the build write-blocks → flatline (<5ms of
            # residual kernel accounting over 20s). Running, the
            # sleep-paced-but-busyworked script burns ≥300ms per 20s
            # window; 100ms sits >20x above frozen and >3x below
            # running.
            assert cpu2 - cpu1 > 100_000, (
                f"build CPU flatlined during the scheduler stall "
                f"(delta {cpu2 - cpu1} usec over 20s) — the worker froze "
                "with the link"
            )
            time.sleep(10)  # complete the 60s stall window
            # The display stream shed during the stall (counted,
            # structural) — scraped INSIDE the stall window because the
            # per-build worker process (and its metric registry) exits
            # after completion; a post-recovery scrape would read a
            # fresh, empty process.
            worker.succeed(
                "curl -sf localhost:9093/metrics | "
                "grep -E 'rio_builder_log_messages_shed_total.+ [1-9][0-9]*$'"
            )
        finally:
            control.succeed("systemctl kill --signal=SIGCONT rio-scheduler")

        # Post-recovery: generous window (lease re-acquire, heartbeat
        # retries, completion delivery are all allowed to take time).
        client.wait_until_succeeds(
            "grep -q '^/nix/store/.*chaos5stallmark' /tmp/chaos5.log "
            "&& grep -qx 'rc=0' /tmp/chaos5.log",
            timeout=180,
        )

    # ══════════════════════════════════════════════════════════════════
    # Subtest 6: scheduler SIGSTOP 75s — the timeout kill fires INSIDE it
    # ══════════════════════════════════════════════════════════════════
    # 45s build timeout via worker config (drop-in + restart: the env is
    # worker-wide, and subtest 5's build needed the 2h default). 75s
    # stall with the kill due at 45s: at stall+55s the build tree must
    # be GONE while the scheduler is still frozen — the discriminator
    # between "killed on time" and "frozen by backpressure" (both
    # flatline CPU). 45s timeout + 75s stall stays under the scheduler's
    # 45+90=135s worker deadline: no reassignment race.
    with subtest("sched-stall-timeout-still-fires: kill lands mid-stall"):
        worker.succeed(
            "mkdir -p /run/systemd/system/rio-builder.service.d && "
            "printf '[Service]\nEnvironment=RIO_BUILD_TIMEOUT_SECS=45\n' "
            "> /run/systemd/system/rio-builder.service.d/chaos-timeout.conf && "
            "systemctl daemon-reload && systemctl restart rio-builder"
        )
        control.wait_until_succeeds(
            "curl -sf http://localhost:9091/metrics | "
            "grep -x 'rio_scheduler_workers_active 1'",
            timeout=60,
        )

        client.succeed(
            "rm -f /tmp/chaos6.log; "
            f"{{ timeout 400 nix-build --no-out-link --store '{store_url}' "
            f"{busybox_arg} ${stallForeverDrv}; echo rc=$?; }} "
            "> /tmp/chaos6.log 2>&1 < /dev/null &"
        )
        worker.wait_until_succeeds(
            "find /sys/fs/cgroup -type d -name '*chaos6forevermark*' "
            "| grep -q .",
            timeout=180,
        )

        control.succeed("systemctl kill --signal=SIGSTOP rio-scheduler")
        try:
            # Kill due ~45s after build start (the cgroup-creation
            # instant, moments before this stall began); assert at
            # stall+55s, still 20s inside the stall: the build cgroup
            # must hold no processes (or be torn down entirely) — the
            # discriminator between "killed on time" and "frozen".
            time.sleep(55)
            worker.fail(
                "for d in $(find /sys/fs/cgroup -type d "
                "-name '*chaos6forevermark*'); do cat $d/cgroup.procs; "
                "done | grep -q ."
            )
        finally:
            time.sleep(20)  # complete the 75s stall
            control.succeed("systemctl kill --signal=SIGCONT rio-scheduler")

        # The buffered TimedOut completion reaches the scheduler after
        # recovery. The structural receipt is the timeout handler's
        # signature action: it bumps and PERSISTS the derivation's
        # deadline floor before re-queueing. We deliberately do NOT
        # wait for the client-visible failure — the scheduler retries
        # TimedOut builds with a promoted deadline (up to its retry
        # max) by design, so nix-build only exits nonzero after that
        # ladder exhausts (~4 x 50s, beyond any sane window here); the
        # ladder itself is lifecycle-suite territory. This subtest's
        # property ends at "the kill's outcome crossed the recovered
        # link and was durably processed".
        control.wait_until_succeeds(
            "sudo -u postgres psql rio -qtAc "
            "\"SELECT 1 FROM derivations WHERE drv_path "
            "LIKE '%chaos6forevermark%' AND floor_deadline_secs > 0\" "
            "| grep -q 1",
            timeout=120,
        )
        # Tear down the retry ladder's in-flight client build and
        # restore the default-timeout worker.
        client.succeed("pkill -f 'chaos6forevermar[k]' || true")
        worker.succeed(
            "rm -f /run/systemd/system/rio-builder.service.d/chaos-timeout.conf && "
            "systemctl daemon-reload && systemctl restart rio-builder"
        )

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
