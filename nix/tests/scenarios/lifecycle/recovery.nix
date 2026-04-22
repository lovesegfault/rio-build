# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # recovery — kill leader pod mid-build, standby takes over
  # ══════════════════════════════════════════════════════════════════
  # STRONGER than phase3b's single-instance `systemctl kill`: with
  # scheduler.replicas=2 (podAntiAffinity across server+agent), killing
  # the leader means the STANDBY acquires the lease. The standby's
  # recovery_total was 0 (standby never ran recover_from_pg —
  # LeaderAcquired never fired). After acquiring, it's exactly 1.
  #
  # Proves: lease TTL expiry detection → standby LeaderAcquired →
  # recover_from_pg loads REAL non-terminal rows from PG → dispatch
  # gate unblocks (if !is_leader || !recovery_complete → no-op).
  with subtest("recovery: kill leader mid-build, standby acquires + recovers"):
      # Baseline: boot-time leader already ran recovery once (its
      # LeaderAcquired fired during waitReady). This scrape goes to
      # the CURRENT leader — confirms recovery happened at all before
      # we trust any of the dispatch paths above.
      m = sched_metrics()
      boot_recovery = metric_value(m, "rio_scheduler_recovery_total",
                                   labels='{outcome="success"}')
      assert boot_recovery is not None and boot_recovery >= 1.0, (
          f"boot-time leader should have run recovery >=1 time, "
          f"got {boot_recovery!r}\n"
          f"  all recovery series: {m.get('rio_scheduler_recovery_total', {})!r}"
      )

      # Settle: q==0 AND r==0 BEFORE starting the slow build. The
      # derivations_running gauge is Tick-updated (scheduler default
      # ~10s; worker.rs:604-623). Without this baseline, the running
      # gauge might still show a stale count from pinDrv/watchDedup →
      # we'd kill the leader before the slow build is even dispatched
      # → PG has 0 non-terminal rows → assert fails. 120s: Tick is
      # 10s, each retry spawns a fresh port-forward (2s bind), and
      # port 19091 TIME_WAIT can eat a retry. Observed: v17 timed
      # out at 60s with connection-reset noise from the pf churn.
      sched_metric_wait(
          "awk '/^rio_scheduler_derivations_queued / {q=$2} "
          "/^rio_scheduler_derivations_running / {r=$2} "
          "END {exit !(q==0 && r==0)}'",
          timeout=180,
      )

      # Capture the pre-kill leader name. After `delete pod`, the
      # Deployment controller creates a NEW replacement pod with a
      # DIFFERENT name — so seeing leader_pod() return a different
      # value is positive proof the lease moved.
      old_leader = leader_pod()
      print(f"recovery: pre-kill leader = {old_leader}")

      # Backgrounded slow build. `nohup ... < /dev/null &` fully
      # detaches (no stdin read, no HUP on shell exit). client.execute
      # (not .succeed): returns immediately, no exit-code check.
      client.execute(
          "nohup nix-build --no-out-link "
          "--store 'ssh-ng://k3s-server' "
          "--arg busybox '(builtins.storePath ${common.busybox})' "
          "${recoverySlowDrv} "
          "> /tmp/recovery-slow.log 2>&1 < /dev/null &"
      )

      # Poll for dispatch (running ≥1). Settle-wait guaranteed
      # baseline 0, so a nonzero reading IS our slow build. 60s:
      # nix-build needs ~10-15s to reach dispatch (ssh-ng handshake
      # + SubmitBuild + DAG merge + dispatch on 10s Tick).
      sched_metric_wait(
          "grep -E '^rio_scheduler_derivations_running [1-9]'",
          timeout=60,
      )

      # PG snapshot BEFORE the kill. At kill time the worker's gRPC
      # stream is dead — it CANNOT report completion until a scheduler
      # is back. So the row is guaranteed non-terminal NOW. Checking
      # after recovery races with the build finishing (worker
      # reconnects → reports → status='completed' before the assert).
      # Same TERMINAL_STATUSES filter as load_nonterminal_derivations
      # (db/mod.rs:58 TERMINAL_STATUS_SQL).
      #
      # psql_k8s (NOT psql): bitnami PG runs in a pod, not systemd.
      nonterminal = int(psql_k8s(k3s_server,
          "SELECT COUNT(*) FROM derivations "
          "WHERE status NOT IN "
          "('completed','poisoned','dependency_failed','cancelled','skipped')"
      ))
      assert nonterminal >= 1, (
          f"PG snapshot at kill time should have >=1 non-terminal drv "
          f"(slow build in-flight), got {nonterminal}"
      )
      print(f"recovery: PG has {nonterminal} non-terminal row(s) for recovery to load")

      # Kill the leader pod. --grace-period=0 --force: immediate
      # deletion, no SIGTERM drain. Simulates a node crash / OOMKill,
      # NOT graceful shutdown. The Deployment controller immediately
      # creates a replacement — but the STANDBY pod acquires the
      # lease first (it's already running, watching, probing;
      # replacement pod takes ~10-20s to reach Ready).
      kubectl(f"delete pod {old_leader} --grace-period=0 --force")

      # Standby acquires. Lease holderIdentity becomes a DIFFERENT,
      # NON-EMPTY pod name. 60s timeout: lease TTL + acquire tick
      # (~5s poll). Two transient states to reject:
      #   (a) holderIdentity stays old name until lease expires
      #       (so != check, not just -n)
      #   (b) under KVM, --grace-period=0 --force deletes the pod so
      #       fast that holderIdentity is briefly EMPTY before the
      #       standby claims it (observed: 0.2s window) — without
      #       the -n guard, "" != old_leader is trivially true and
      #       new_leader below captures the empty string.
      k3s_server.wait_until_succeeds(
          "h=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
          "-o jsonpath='{.spec.holderIdentity}') && "
          f"test -n \"$h\" && test \"$h\" != '{old_leader}'",
          timeout=60,
      )
      new_leader = leader_pod()
      assert new_leader != old_leader, (
          f"lease should move off killed pod: old={old_leader} new={new_leader}"
      )
      print(f"recovery: new leader = {new_leader}")

      # New leader ran recovery. EXACTLY 1.0: this pod was the
      # standby, it never ran recovery before (LeaderAcquired never
      # fired for it). Fresh acquisition → exactly one recovery. If
      # the replacement pod somehow won the lease race instead, same
      # thing — fresh process, first acquire, recovery_total = 1.
      #
      # wait_until_succeeds (not one-shot): recovery runs in the
      # LeaderAcquired handler, asynchronously after lease acquire.
      # There's a small window where lease moved but recovery hasn't
      # finished yet.
      sched_metric_wait(
          "grep -qx 'rio_scheduler_recovery_total{outcome=\"success\"} 1'",
          timeout=180,
      )

      # Worker re-registered with the new leader. Fresh scheduler
      # process = metrics reset → workers_active climbs back to ≥1.
      # ≥1 not ==1: the slow build's worker may have briefly
      # disconnected/reconnected during failover.
      sched_metric_wait(
          "grep -E '^rio_scheduler_workers_active [1-9]'",
          timeout=180,
      )

      # Post-recovery build. DIFFERENT marker → different output path
      # → NOT a cache hit. Proves dispatch is unblocked AFTER the
      # lease re-acquire + recover_from_pg sequence (if recovery
      # failed or never ran, dispatch_ready stays false forever).
      #
      # wait_until_succeeds (not one-shot build()): each nix-build is
      # a FRESH SSH connect → gateway runs resolve_and_mint() per
      # connection. Its scheduler client is a BalancedChannel that
      # health-probes rio.scheduler.SchedulerService every
      # DEFAULT_PROBE_INTERVAL=3s and only routes to SERVING (=
      # is_leader, see spawn_health_toggle). During the failover gap
      # the old leader is gone, the standby reports NOT_SERVING, and
      # the replacement is not yet probed → 0 endpoints →
      # resolve_and_mint times out (500ms). With jwt.required=false
      # the gateway DEGRADES to tokenless (rio_gateway_jwt_mint_
      # degraded_total++); the JWT-mode scheduler then rejects
      # SubmitBuild with Unauthenticated. The very next 3s probe tick
      # discovers the new leader and the next connect mints fine.
      # 30s budget covers ≤2 probe ticks + nix-build dispatch (~15s).
      #
      # Structural (convergence wait), not retry-on-error: same
      # pattern as sched_metric_wait above. The probe-interval gap is
      # the only window; a sustained mint failure exhausts 30s and
      # raises.
      out_recovery = client.wait_until_succeeds(
          "nix-build --no-out-link --store 'ssh-ng://k3s-server' "
          "--arg busybox '(builtins.storePath ${common.busybox})' "
          "${recoveryDrv}",
          timeout=30,
      ).strip()
      assert out_recovery.startswith("/nix/store/"), (
          f"post-recovery build should succeed: {out_recovery!r}"
      )

      # Re-check recovery_total is EXACTLY 1 at the end — proves
      # recovery ran exactly once in THIS leader's process lifetime
      # (no spurious re-acquires, no double-recovery bugs).
      m = sched_metrics()
      final_recovery = metric_value(m, "rio_scheduler_recovery_total",
                                    labels='{outcome="success"}')
      assert final_recovery == 1.0, (
          f"new leader should have recovery_total=1 (fresh process, one "
          f"acquire), got {final_recovery!r}\n"
          f"  all recovery series: {m.get('rio_scheduler_recovery_total', {})!r}"
      )

      # Drain the slow build before the next sections. 150s: up to
      # ~60s sleep remainder + re-dispatch overhead after failover +
      # ReconcileAssignments cross-check delay.
      sched_metric_wait(
          "awk '/^rio_scheduler_derivations_queued / {q=$2} "
          "/^rio_scheduler_derivations_running / {r=$2} "
          "END {exit !(q==0 && r==0)}'",
          timeout=150,
      )
      print(f"recovery PASS: standby took over, loaded {nonterminal} row(s), built {out_recovery}")
''
