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
  twoOutputDrv = pkgs.writeText "drv-chaos-reset.nix" ''
    { busybox }:
    derivation {
      name = "rio-test-chaos-reset";
      outputs = [ "out" "dev" ];
      builder = "''${busybox}/bin/sh";
      args = [ "-c" '''
        echo chaos-reset-out > $out
        echo chaos-reset-dev > $dev
      ''' ];
      system = builtins.currentSystem;
    }
  '';

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
  largeOutputDrv = pkgs.writeText "drv-chaos-bandwidth.nix" ''
    { busybox }:
    derivation {
      name = "rio-test-chaos-bandwidth";
      builder = "''${busybox}/bin/sh";
      args = [ "-c" '''
        ''${busybox}/bin/busybox dd if=/dev/urandom of=$out bs=1048576 count=2
      ''' ];
      system = builtins.currentSystem;
    }
  '';

  covTimeoutHeadroom = if common.coverage then 300 else 0;
in
pkgs.testers.runNixOSTest {
  name = "rio-chaos";
  skipTypeCheck = true;
  # Boot ~60s + subtest 1 ~10s + subtest 2 ~30s (journal-poll + retry
  # backoff) + subtest 3 ~15s (5s toxic-close + build) + subtest 4 ~30s
  # (dd + 16s throttled upload) + margin. 600s is generous; the dominant
  # term is subtest 2's wait_until_succeeds poll loop under VM jitter.
  globalTimeout = 600 + covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

    import time

    ${common.kvmPreopen}
    start_all()
    ${fixture.waitReady}
    ${common.sshKeySetup gatewayHost}
    ${common.seedBusybox gatewayHost}

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
            f"journalctl -u rio-worker --since=@{mark} --no-pager | "
            "grep -qE 'upload attempt failed|input metadata fetch failed'",
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
            f"journalctl -u rio-worker --since=@{mark} --no-pager | "
            "grep -q 'retrying upload' || "
            f"[ $(journalctl -u rio-worker --since=@{mark} --no-pager | "
            "grep -c 'received work assignment.*chaos-reset') -ge 2 ]"
        )

        # NO exhausted uploads. status="exhausted" (upload.rs:247) would
        # mean a build output failed all 3 attempts. Metric not registered
        # = zero (good); registered with value 0.0 also good.
        m = scrape_metrics(worker, 9093)
        exhausted = metric_value(
            m, "rio_worker_uploads_total", labels='{status="exhausted"}'
        ) or 0.0
        assert exhausted == 0.0, (
            f"upload retries exhausted ({exhausted}×) — heal too slow"
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

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
