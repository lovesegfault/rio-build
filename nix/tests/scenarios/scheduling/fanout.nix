# scheduling subtest fragment — composed by scenarios/scheduling.nix mkTest.
scope: with scope; ''
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

      # Distribution: EACH worker executed ≥1 derivation. 4 parallel
      # leaves across 3 workers means every worker gets at least one
      # leaf. If any sat idle, dispatch is broken (scheduler not
      # round-robin, or worker registration metadata wrong).
      # journald-counted because rio-builder is one-shot — per-process
      # counters reset on every restart.
      for w in all_workers:
          n = journal_builds_succeeded(w)
          assert n >= 1, (
              f"{w.name} did 0 builds (journald); fanout should "
              f"hit every worker"
          )

      # Store: received 5 build outputs via PutPath (+ busybox seed).
      # ≥5 to be robust against retries.
      assert_metric_ge(${gatewayHost}, 9092,
          "rio_store_put_path_total", 5.0, labels='{result="created"}')

      # PrefetchHint: the collector (rio-root) has 4 DAG children.
      # When root dispatches, approx_input_closure returns the 4
      # leaf output paths → hint sent with ≥1 path. paths_sent is
      # tighter than hints_sent: an empty-hint bug (message sent,
      # 0 paths) would pass hints≥1 but fail paths≥1. (phase3a:485)
      assert_metric_ge(${gatewayHost}, 9091,
          "rio_scheduler_prefetch_paths_sent_total", 1.0)
''
