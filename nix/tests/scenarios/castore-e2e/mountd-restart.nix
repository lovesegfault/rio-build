# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # P0560 round 3b finding (c), now fixed: the in-flight build DID
  # survive the restart (its reads kept working) but every attempt then
  # failed at output upload with "path not authorized by assignment
  # token" — gRPC-direct submissions carry no expected_output_paths, so
  # the HMAC claim authorized nothing; the scheduler now backfills them
  # from the inlined drv (rio-scheduler/src/domain.rs). The restarted
  # daemon's startup orphan scan was also reaping the LIVE build's
  # staging dir; it now skips dirs whose builder still holds the
  # .rio-live flock (castore_fuse/{mount,mountd,sweep}.rs) while still
  # reaping genuinely orphaned ones — which this subtest asserts both
  # ways (planted orphan reaped, live staging + caches intact).
  # ══════════════════════════════════════════════════════════════════
  # mountd-restart — broker restart is non-disruptive + orphan scan
  # ══════════════════════════════════════════════════════════════════
  # Phase A: a build holds a passthrough fd on small_1m (a cache hit —
  #   chunk-warm promoted it) across the restart, then keeps reading
  #   from it and opens warm_4k (also a hit). Already-registered
  #   passthrough fds are pure kernel↔backing-file, so they must
  #   survive; the post-restart open either BackingOpens against the
  #   new daemon or degrades to keep_cache — both keep the build alive.
  # Phase B: the DaemonSet pod on k3s-agent is force-deleted mid-build
  #   (grace 0, so cleanup falls to the NEXT incarnation's startup
  #   orphan scan, not the dying pod). A planted orphan staging dir must
  #   be reaped by the restarted daemon while the shared cache/chunk
  #   trees survive untouched, and the new pod's metrics endpoint must
  #   answer.
  # Phase C: the in-flight build runs to completion (not cancelled).
  # Phase D: a fresh build with a never-seen input forces a full
  #   miss → fetch → Promote against the restarted daemon.
  # Phase E: a build takes a COLD whole-file miss while the broker is
  #   away (killed again mid-build): its Promote meets a dead
  #   connection, so it must recover via the client's reconnect
  #   (re-dial + re-Mount + retried Promote) or be served from the
  #   verified staged copy (degraded) — the build completes either way
  #   and takes no EIO.
  with subtest("mountd-restart: in-flight build survives, orphan scan reaps, next build promotes"):
      # ── Phase A: in-flight build with a held passthrough fd ─────────
      drv_mr, build_mr = submit_drv(
          "${mountdRestartDrv}",
          extra_args=(
              f"--arg small1m '(builtins.storePath {p_small1m})' "
              f"--arg warm4k '(builtins.storePath {p_warm4k})'"
          ),
      )
      pod = castore_pod()
      wait_worker_metric(
          pod, "grep -q 'case=\"hit\"'", timeout=300, ctx="mountd-restart fd held"
      )

      # ── Phase B: force-restart the broker on the build's node ───────
      old_mountd = mountd_pod()
      # Plant an orphan staging entry the dying pod cannot reap (it gets
      # no grace period) — the restarted daemon's startup scan must.
      k3s_agent.succeed(
          "mkdir -p /var/rio/staging/castore-e2e-orphan && "
          "echo orphan > /var/rio/staging/castore-e2e-orphan/leftover"
      )
      staging_before = k3s_agent.succeed("ls /var/rio/staging").strip()
      print(f"mountd-restart: staging before kill: {staging_before!r}")
      kubectl(
          "delete pod -l app.kubernetes.io/name=rio-mountd "
          "--field-selector spec.nodeName=k3s-agent --grace-period=0 --force",
          ns="${nsBuilders}",
      )
      # New DS pod Running on the agent (a different pod name).
      k3s_server.wait_until_succeeds(
          "p=$(k3s kubectl -n ${nsBuilders} get pod -l app.kubernetes.io/name=rio-mountd "
          "--field-selector spec.nodeName=k3s-agent,status.phase=Running "
          "-o jsonpath='{.items[0].metadata.name}' 2>/dev/null) && "
          f"test -n \"$p\" && test \"$p\" != \"{old_mountd}\"",
          timeout=180,
      )
      # Orphan scan reaped the planted dir; the in-flight build's own
      # staging dir (its builder holds the .rio-live flock) and the
      # shared caches survived.
      k3s_agent.wait_until_succeeds(
          "! test -e /var/rio/staging/castore-e2e-orphan", timeout=90
      )
      build_id_mr = drv_mr.rsplit("/", 1)[-1].replace(".", "_")
      k3s_agent.succeed(f"test -d /var/rio/staging/{build_id_mr}")
      print(f"mountd-restart: live build's staging /var/rio/staging/{build_id_mr} "
            "survived the restarted daemon's orphan scan")
      k3s_agent.succeed(f"test -e {cache_path(b3_small1m)}")
      k3s_agent.succeed(f"test -e {cache_path(b3_warm4k)}")
      k3s_agent.succeed("find /var/rio/chunks -type f -print -quit | grep -q .")
      # The restarted broker answers on its metrics port.
      md = mountd_metrics()
      print(
          "mountd-restart: restarted mountd serving; connections_current="
          f"{fam(md, 'rio_mountd_connections_current')!r}"
      )

      # ── Phase C: the in-flight build completes across the restart ───
      wait_drv_status(drv_mr, ["completed"], timeout=420, ctx="mountd-restart in-flight build")
      wait_no_running_builders()

      # ── Phase D: a fresh miss → fetch → Promote on the new daemon ───
      out_post = build(
          "${postRestartDrv}",
          extra_args=f"--arg postSeed '(builtins.storePath {p_post_seed})'",
      )
      assert_cached(b3_post_seed, "post-restart seed promoted via the new daemon", timeout=120)
      print(f"mountd-restart PASS: build survived the restart, orphan reaped, "
            f"post-restart promote landed ({out_post})")

      # ── Phase E: a COLD whole-file miss while the broker is away ────
      # Round-3b reconnect-or-degrade follow-up: previously a cold miss
      # whose Promote hit the dead broker connection EIO'd the build
      # (the phases above deliberately pre-warm their inputs). Two fresh
      # never-cached inputs: cold_gate (64 KiB) is the in-build progress
      # signal, cold_miss (2 MiB, < the 8 MiB stream threshold) is the
      # whole-file miss taken while the broker is down/restarting.
      client.succeed(
          "dd if=/dev/urandom of=/tmp/castore-cold-gate.bin bs=64k count=1 2>/dev/null && "
          "dd if=/dev/urandom of=/tmp/castore-cold-miss.bin bs=1M count=2 2>/dev/null"
      )
      b3_cold_gate = client_b3("/tmp/castore-cold-gate.bin")
      b3_cold_miss = client_b3("/tmp/castore-cold-miss.bin")
      p_cold_gate, p_cold_miss = add_and_push(
          "/tmp/castore-cold-gate.bin", "/tmp/castore-cold-miss.bin"
      )
      assert_not_cached(b3_cold_miss, "cold_miss before the cold-miss restart build")

      wait_no_running_builders()
      old_mountd2 = mountd_pod()
      drv_cm, build_cm = submit_drv(
          "${coldMissRestartDrv}",
          extra_args=(
              f"--arg coldGate '(builtins.storePath {p_cold_gate})' "
              f"--arg coldMiss '(builtins.storePath {p_cold_miss})'"
          ),
      )
      pod_cm = castore_pod()
      # cold_gate promoted ⇒ the script is inside its 30 s pre-miss
      # sleep: kill the broker now, so the cold_miss open's Promote can
      # only meet a dead (or just-restarted) daemon.
      assert_cached(b3_cold_gate, "cold_gate read before the second broker kill", timeout=300)
      kubectl(
          "delete pod -l app.kubernetes.io/name=rio-mountd "
          "--field-selector spec.nodeName=k3s-agent --grace-period=0 --force",
          ns="${nsBuilders}",
      )
      print("mountd-restart: broker killed again while the cold-miss build sleeps")

      # Evidence, scraped DURING the build's 90 s tail (one-shot pod):
      # either the client re-established the mountd session and the
      # promote landed (reconnect counter, outcome="ok") or the open was
      # served from the verified staged copy (degraded counter) — both
      # are acceptable recoveries; EIO is not.
      deadline = time.time() + 240
      reconnects = degraded = 0.0
      m = {}
      while time.time() < deadline:
          try:
              m = worker_metrics(pod_cm)
          except Exception:
              time.sleep(5)
              continue
          reconnects = series(
              m, "rio_builder_castore_fuse_mountd_reconnect_total", must=('outcome="ok"',)
          )
          degraded = series(m, "rio_builder_castore_fuse_degraded_serve_total")
          if reconnects > 0 or degraded > 0:
              break
          time.sleep(5)
      else:
          dump_castore_diag("cold-miss-during-restart evidence", pod=pod_cm)
          raise AssertionError(
              "neither a successful mountd reconnect nor a degraded staged serve was "
              "observed for the cold whole-file miss taken while the broker was away; "
              f"families: reconnect={fam(m, 'rio_builder_castore_fuse_mountd_reconnect_total')!r} "
              f"degraded={fam(m, 'rio_builder_castore_fuse_degraded_serve_total')!r}"
          )
      eio_cm = series(m, "rio_builder_castore_fuse_eio_total")
      assert eio_cm == 0, (
          f"the cold-miss build must not take EIO during the broker outage "
          f"(eio_total={eio_cm}, reconnect_ok={reconnects}, degraded={degraded})"
      )
      print(
          f"mountd-restart: cold miss while the broker was away recovered via "
          f"reconnect_ok={int(reconnects)} degraded_serve={int(degraded)} (eio_total=0)"
      )

      # The build completes either way, and the pool drains.
      wait_drv_status(drv_cm, ["completed"], timeout=420, ctx="mountd-restart cold-miss build")
      wait_no_running_builders()

      # Leave a healthy broker behind for the subtests that follow.
      k3s_server.wait_until_succeeds(
          "p=$(k3s kubectl -n ${nsBuilders} get pod -l app.kubernetes.io/name=rio-mountd "
          "--field-selector spec.nodeName=k3s-agent,status.phase=Running "
          "-o jsonpath='{.items[0].metadata.name}' 2>/dev/null) && "
          f"test -n \"$p\" && test \"$p\" != \"{old_mountd2}\"",
          timeout=180,
      )
      md_post = mountd_metrics()
      print(
          "mountd-restart: broker healthy after the cold-miss phase; "
          f"connections_current={fam(md_post, 'rio_mountd_connections_current')!r}"
      )
      print(f"mountd-restart PASS (cold-miss): build {build_cm} completed across the broker outage")
''
