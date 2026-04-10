# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # gc-dry-run — TriggerGC via AdminService proxy, sweep ROLLBACK
  # ══════════════════════════════════════════════════════════════════
  # Same RPC path as phase3b (scheduler → store → mark → sweep →
  # progress stream → proxy back). grace=24h → nothing past grace → empty
  # unreachable set → proves the stream runs end-to-end, NOT that
  # the for-batch loop body executes (that's gc-sweep's job).
  with subtest("gc-dry-run: TriggerGC completes, currentPath describes outcome"):
      # force=true bypasses the empty-refs safety gate — mkTrivial
      # outputs embed no store-path strings, so the ref scanner
      # correctly finds refs=[] for every fixture path, tripping
      # the >10%-empty-refs precondition even on dry-run.
      result = sched_grpc(
          '{"dry_run": true, "grace_period_hours": 24, "force": true}',
          "rio.admin.AdminService/TriggerGC",
      )
      # GCProgress stream: at least one message with isComplete=true.
      # grpcurl emits one PRETTY-PRINTED JSON object per stream message
      # (proto3 camelCase, multi-line with indented fields). Parse
      # structurally — substring match on "delete"/"path" is satisfied
      # by any error like "failed to delete, path unknown".
      gc_msgs = grpcurl_json_stream(result)
      complete_msgs = [m for m in gc_msgs if m.get("isComplete")]
      assert complete_msgs, (
          f"expected at least one GCProgress with isComplete=true; "
          f"got {len(gc_msgs)} messages: {result[:500]}"
      )
      # currentPath describes the outcome (mark+sweep RAN, even if sweep
      # found nothing). Looking for "would delete" (dry-run phrasing)
      # in the final message's currentPath — NOT a substring match on
      # the whole stream blob.
      final = complete_msgs[-1]
      assert "would delete" in final.get("currentPath", "").lower(), (
          f"expected dry-run currentPath to say 'would delete'; "
          f"got: {final.get('currentPath')!r}"
      )
      print("gc-dry-run PASS: TriggerGC stream completed via AdminService proxy")
''
