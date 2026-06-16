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
  # Proves: observed-staleness steal (STEAL_AFTER) → standby LeaderAcquired →
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
          timeout=300,
      )

      # Capture the pre-kill leader name. After `delete pod`, the
      # Deployment controller creates a NEW replacement pod with a
      # DIFFERENT name — so seeing leader_pod() return a different
      # value is positive proof the lease moved.
      old_leader = leader_pod()
      print(f"recovery: pre-kill leader = {old_leader}")

      # Snapshot recovery_total on the pre-kill leader. Taken AFTER
      # the q==0/r==0 settle and BEFORE the :98 kill — NOT after the
      # lease-moved gate (s3-delta-snapshot-race): snapshotting there
      # would capture base=1 from the already-recovered new leader on
      # a fast path and the delta wait would seek >=2 forever. The
      # lease-moved gate guarantees the delta-wait below takes the
      # leader-CHANGED branch (snap leader == old_leader, killed at
      # :98; new leader is a fresh process whose counter starts at 0
      # and the wait is for >=1).
      recovery_snap = sched_metric_snapshot(
          '^rio_scheduler_recovery_total[{]outcome="success"[}] '
      )
      print(f"recovery: pre-kill recovery_total snap = {recovery_snap}")

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

      # PG snapshot BEFORE the kill. At kill time no scheduler leader
      # is serving — the builder CANNOT land its ReportOutcome until
      # one is back. So the row is guaranteed non-terminal NOW.
      # Checking after recovery races with the build finishing (the
      # report lands → status='completed' before the assert).
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
      # NON-EMPTY pod name. 60s timeout: STEAL_AFTER (19s) + one 5s
      # poll, with headroom. Two transient states to reject:
      #   (a) holderIdentity stays old name until the standby's
      #       STEAL_AFTER threshold elapses (so != check, not just -n)
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

      # New leader ran recovery. Delta-wait (NOT absolute grep -qx
      # for `... 1`): this is the load-robust convergence gate after
      # the strike-4 wall-clock-under-load timeouts (1ef4cc6bd
      # carried-forward to r27). The structural assertion is a delta
      # from a baseline captured on a KNOWN pod before the kill, with
      # an explicit fresh-process branch — not an absolute that races
      # whether recovery has already fired by the time we resolve the
      # leader. The lease-moved gate above guarantees `cur != snap`
      # is the expected path: standby (or replacement) is a fresh
      # process whose counter starts at 0, so the wait is for >=1.
      #
      # The EXACTLY-ONCE property (recovery_total == 1, no spurious
      # re-acquires) is checked separately at the end-of-subtest
      # assert below; this wait is convergence only.
      #
      # wait_until_succeeds (not one-shot): recovery runs in the
      # LeaderAcquired handler, asynchronously after lease acquire.
      # There's a small window where lease moved but recovery hasn't
      # finished yet.
      sched_metric_wait_delta(
          recovery_snap,
          '^rio_scheduler_recovery_total[{]outcome="success"[}] ',
          delta=1,
          timeout=300,
      )

      # The in-flight slow build runs on the pull path (T-1c.2b
      # re-point): its execution row was minted by the pull
      # transaction (the only execution writer) before the kill, so
      # assert the row exists and SURVIVED the failover. The stream
      # era waited here for the worker to RE-REGISTER with the new
      # leader; pull pods hold no session, so there is nothing to
      # re-establish — the report is a unary against whichever pod is
      # leader when the sleep ends, and the end-of-subtest drain below
      # is what proves the new leader accepts it.
      pull_execs = int(psql_k8s(k3s_server,
          "SELECT COUNT(*) FROM drv_executions e "
          "JOIN assignments a ON a.exec_id = e.exec_id "
          "JOIN derivations d ON d.derivation_id = a.derivation_id "
          "WHERE d.drv_path LIKE '%lifecycle-recovery-slow%'"
      ))
      assert pull_execs >= 1, (
          f"the in-flight recovery build should have been dispatched "
          f"on the pull path (>=1 pull-minted execution row), "
          f"got {pull_execs}"
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
      #
      # Budget: ≤2 probe ticks + nix-build dispatch ≈ 21s NOMINAL,
      # but the dispatch leg is builder-load-sensitive — observed
      # 39.15s under the round-9 boundary gate (18 VM tests in
      # parallel on the shared builder; solo re-run green at the same
      # tip). 120s held through round-12 then crossed once at round-13
      # (the 4th recorded occurrence: "interrupted by the user" at
      # the 120s budget under composed-tree contention; lease moved in
      # 1.3s and recovery_total{outcome=success}=1 observed in 6.65s,
      # so the recovery itself completed — only the post-recovery
      # client dispatch leg exceeded the budget). 240s = ~6× the
      # 39.15s observed tail / ~11× nominal — the ci-failure-patterns
      # tail-budget discipline (budget for tail, not typical; builder
      # variance 5×).
      #
      # Structural (convergence wait), not retry-on-error: same
      # pattern as sched_metric_wait above. The probe-interval gap is
      # the only window; a sustained mint failure exhausts the budget
      # and raises.
      out_recovery = client.wait_until_succeeds(
          "nix-build --no-out-link --store 'ssh-ng://k3s-server' "
          "--arg busybox '(builtins.storePath ${common.busybox})' "
          "${recoveryDrv}",
          timeout=240,
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
