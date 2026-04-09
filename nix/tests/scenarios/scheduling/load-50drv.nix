# scheduling subtest fragment — composed by scenarios/scheduling.nix mkTest.
scope: with scope; ''
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
''
