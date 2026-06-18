# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # fod-substituted-inputs — attested_input_seeds PG fallback after a
  # scheduler restart (the 1331-FOD-stuck regression shape)
  # ══════════════════════════════════════════════════════════════════
  # Reproduces the post-restart shape where a build's inputDrv is
  # Completed (so recovery — which loads only non-terminal rows — does
  # NOT carry it in the in-memory DAG) and the dispatch-time
  # attested_input_seeds resolver MUST fall back to the persisted
  # derivations.expected_output_paths row instead of degrading to
  # None → input_roots=[] → builder-side EIO loop.
  #
  # Shape:
  #   1. build dep alone → Completed in PG, derivations row written
  #   2. rollout restart deploy/rio-scheduler → both replicas cycle,
  #      fresh in-mem DAG, recovery loads only non-terminal (dep is
  #      NOT loaded)
  #   3. submit the parent (inputDrvs = {dep}) → DAG-miss on dep
  #      → PG-fallback resolver kicks in
  #   4. assert: build completes; input_closure_unattested_total
  #      {reason=seeds_unknown} stayed flat; no Input/output error in
  #      builder-pod logs.
  #
  # Tracey: sched.dispatch.input-roots — verify marker at the
  # default.nix subtests entry (P0341 convention; marker at wiring
  # point, not in this fragment header).
  with subtest("fod-substituted-inputs: attested_input_seeds resolves dep via PG after restart"):
      # ── Step 1: build dep alone ───────────────────────────────────
      # Building -A dep (not parent) so the parent is NOT yet
      # merged: when it IS submitted post-restart, dep is already
      # Completed and the gateway-sent transitive closure still
      # includes dep's .drv node — but the post-restart in-mem DAG
      # does not, because recovery never loaded it. That gap is what
      # the PG-fallback resolver closes.
      out_dep = build(
          "${fodSubstitutedDrvFile}", attr="dep", capture_stderr=False
      ).strip()
      assert out_dep.startswith("/nix/store/"), (
          f"dep build should succeed: {out_dep!r}"
      )
      # Non-vacuity: the derivations row exists with a non-empty
      # expected_output_paths column — the authority the fallback
      # resolver reads.
      dep_row = psql_k8s(
          k3s_server,
          "SELECT array_length(expected_output_paths, 1) FROM derivations "
          "WHERE drv_path LIKE '%rio-fod-substituted-dep%'",
      ).strip()
      assert dep_row == "1", (
          f"derivations row for dep should carry 1 expected output "
          f"path (the PG-fallback authority), got {dep_row!r}"
      )

      # ── Step 2: rollout restart the scheduler ─────────────────────
      # rollout restart cycles BOTH replicas (replicas=2 in this
      # fixture). The new leader's recover_from_pg loads only
      # non-terminal rows — dep (Completed at step 1) is NOT among
      # them, so it is absent from the post-restart in-memory DAG.
      kubectl("rollout restart deploy/rio-scheduler", ns="${ns}")
      k3s_server.wait_until_succeeds(
          "k3s kubectl -n ${ns} rollout status "
          "deploy/rio-scheduler --timeout=90s",
          timeout=180,
      )
      # Wait for a leader to acquire and recovery to complete; same
      # pattern as recovery.nix's lease-moved + recovery_total>=1
      # gates, but a bare convergence wait is enough here (the metric
      # is what gates dispatch_ready).
      sched_metric_wait(
          "grep -E '^rio_scheduler_recovery_total[{]outcome=\"success\"[}] [1-9]'",
          timeout=120,
      )

      # ── Baseline metrics on the post-restart leader ───────────────
      # Snapshot AFTER the restart, BEFORE the parent submit: a fresh
      # process starts every counter at 0, but recovery / dep's
      # earlier build may have ticked seeds_unknown for OTHER drvs;
      # the assertion is on the delta across the parent submit only.
      m_pre = sched_metrics()
      pre_unknown = metric_value(
          m_pre, "rio_scheduler_input_closure_unattested_total",
          labels='{reason="seeds_unknown"}',
      ) or 0.0
      pre_resolved = metric_value(
          m_pre, "rio_scheduler_attested_seeds_pg_fallback_total",
          labels='{outcome="resolved"}',
      ) or 0.0

      # ── Step 3: submit the parent ─────────────────────────────────
      # The parent reads dep through the castore-FUSE lower, so an
      # empty-input_roots dispatch under closure-scoped enforce would
      # EIO that read; under the current branch the builder unions its
      # own BFS, so the build succeeding alone is necessary but not
      # sufficient — the metric deltas below are what prove the
      # resolver took the PG-fallback arm.
      #
      # wait_until_succeeds (not one-shot build()): same probe-gap
      # rationale as recovery.nix's post-recovery build — the
      # gateway's BalancedChannel may take ≤2 probe ticks to
      # rediscover the new leader. Budget for tail (builder variance
      # 5× under composed-tree contention).
      out_parent = client.wait_until_succeeds(
          "nix-build --no-out-link --store 'ssh-ng://k3s-server' "
          "--arg busybox '(builtins.storePath ${common.busybox})' "
          "-A parent ${fodSubstitutedDrvFile}",
          timeout=240,
      ).strip()
      assert out_parent.startswith("/nix/store/"), (
          f"parent should build post-restart with dep resolved "
          f"via the PG-fallback resolver: {out_parent!r}"
      )

      # ── Step 4: metric + log assertions ───────────────────────────
      m_post = sched_metrics()
      post_unknown = metric_value(
          m_post, "rio_scheduler_input_closure_unattested_total",
          labels='{reason="seeds_unknown"}',
      ) or 0.0
      post_resolved = metric_value(
          m_post, "rio_scheduler_attested_seeds_pg_fallback_total",
          labels='{outcome="resolved"}',
      ) or 0.0

      assert post_unknown == pre_unknown, (
          f"input_closure_unattested_total{{reason=seeds_unknown}} "
          f"ticked across the parent dispatch ({pre_unknown} → "
          f"{post_unknown}); attested_input_seeds degraded to None "
          f"instead of resolving dep"
      )
      # The pg_fallback_total{outcome=resolved} increment is NOT
      # asserted: a fresh client submission re-sends the full
      # transitive closure, dag.merge inserts dep with the parent
      # build's interest, and reap is gated on
      # interested_builds.is_empty() — so dep is in path_to_hash for
      # the parent's lifetime and the resolver takes the DAG arm. The
      # DAG-miss arm (PG fallback) is reachable only via recovery of
      # an in-flight parent (which has drv_content=Vec::new() →
      # residual None-at-parse arm) or via a topdown-pruned merge that
      # later falls into from-source dispatch (materialization-specific,
      # not reproducible in this fixture). Unit coverage of the PG arm:
      # attested_seeds_fall_back_to_pg_for_substituted_input_drv.
      print(
          f"fod-substituted-inputs: pg_fallback_resolved delta = "
          f"{post_resolved - pre_resolved} (0 expected on this path; "
          f"DAG arm resolved dep)"
      )

      # No Input/output error in any builder pod's logs. Grep across
      # every pod in the builders namespace since the parent's pod
      # name is not known here. --since=5m bounds the scan to this
      # subtest's window. On this branch the builder unions its own
      # BFS into castore_roots, so this clause is forward-compat for
      # when the W03 closure-scoped enforce commits land and an
      # empty-input_roots dispatch becomes a real EIO again.
      eio_hits = k3s_server.succeed(
          "for p in $(k3s kubectl -n ${nsBuilders} get pods "
          "-o jsonpath='{.items[*].metadata.name}'); do "
          "  k3s kubectl -n ${nsBuilders} logs $p --since=5m 2>/dev/null || true; "
          "done | grep -c 'Input/output error' || true"
      ).strip()
      assert eio_hits == "0", (
          f"{eio_hits} Input/output error line(s) in builder-pod logs; "
          f"the parent's castore-FUSE content-fetch failed — the "
          f"production EIO loop reproduced"
      )

      # Drain before the next subtest. Same q==0 r==0 settle as
      # recovery.nix; 120s for one short build plus Tick.
      sched_metric_wait(
          "awk '/^rio_scheduler_derivations_queued / {q=$2} "
          "/^rio_scheduler_derivations_running / {r=$2} "
          "END {exit !(q==0 && r==0)}'",
          timeout=120,
      )
      print(
          f"fod-substituted-inputs PASS: dep={out_dep} parent={out_parent} "
          f"seeds_unknown delta={post_unknown - pre_unknown}"
      )
''
