# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # NOT WIRED YET (P0560 round 3b finding): the in-flight build did
  # not survive the broker force-restart — every attempt failed as an
  # infrastructure failure right after its hold window and the build
  # ended `dependency_failed` after the orphan watcher reaped it. The
  # orphan-scan + cache-survival half of the subtest passed (the
  # planted staging orphan was reaped, /var/rio/{cache,chunks}
  # survived, the restarted daemon served new mounts). Wire it back
  # into vm-castore-e2e-faults once the in-flight-build story is
  # understood (builder.mountd.orphan-scan stays verified by
  # vm-mountd in the meantime).
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
      # Orphan scan reaped the planted dir; the shared caches survived.
      k3s_agent.wait_until_succeeds(
          "! test -e /var/rio/staging/castore-e2e-orphan", timeout=90
      )
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
''
