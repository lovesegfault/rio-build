# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # refs-end-to-end — refscan → PG references → GC mark walks refs
  # ══════════════════════════════════════════════════════════════════
  # End-to-end test of the worker ref scanner → PG references →
  # GC mark-walks-refs chain. gc-sweep proves the DELETE path
  # (victim has refs=[] by construction — mkTrivial leaf, scanner
  # correctly finds nothing). This proves the SURVIVAL path:
  # consumer's refscan-populated references[] makes dep REACHABLE via
  # mark's recursive CTE → dep survives a sweep even though dep itself
  # is unpinned AND past grace AND has refs=[] (no outbound edges).
  #
  # Four assertions in sequence, each with its own failure signature:
  #   1. PG references contains dep → RefScanSink + PutPath work
  #   2. PG deriver is the .drv path → deriver plumbing works
  #   3. pin consumer + sweep → dep survives → mark CTE walks refs
  #   4. unpin + sweep → both gone → sweep commits when unreachable
  #
  # Self-contained. Runs after gc-sweep (both exercise TriggerGC) but
  # does NOT depend on gc-sweep's state: gc-sweep's out_pin is in-grace
  # (never backdated) and its out_victim is already deleted.
  with subtest("refs-end-to-end: refscan → PG → GC mark walks refs"):
      # Build dep FIRST to capture its output path. Building consumer
      # alone would also build dep (it's an input), but nix-build only
      # prints the top-level out path — we need dep's path for the PG
      # assertion. Second build cache-hits dep (DAG dedup on the .drv).
      out_dep = build("${refsDrvFile} -A dep", capture_stderr=False).strip()
      assert out_dep.startswith("/nix/store/"), f"dep: {out_dep!r}"
      out_consumer = build("${refsDrvFile} -A consumer", capture_stderr=False).strip()
      assert out_consumer.startswith("/nix/store/"), f"consumer: {out_consumer!r}"
      print(f"refs-e2e: dep={out_dep}")
      print(f"refs-e2e: consumer={out_consumer}")

      # ── 1. references landed in PG ────────────────────────────────
      # array_to_string: readable assert message if it fails (shows
      # the full refs list, not just a count). \"references\" in this
      # Python source → "references" after Python parses the escape →
      # psql_k8s re-escapes it for bash → PG sees the quoted keyword.
      refs_str = psql_k8s(k3s_server,
          "SELECT array_to_string(\"references\", ' ') FROM narinfo "
          f"WHERE store_path = '{out_consumer}'"
      )
      assert out_dep in refs_str, (
          f"consumer's references should contain dep path {out_dep!r}; "
          f"PG returned: {refs_str!r}. RefScanSink did not find the "
          "store-path hash embedded in $out/script, OR PutPath "
          "dropped the references field on the wire."
      )
      # cardinality ≥1: guards against array_to_string returning the
      # path from a DIFFERENT row via a wrong WHERE (e.g. empty WHERE
      # accidentally selecting the first row). Belt-and-suspenders.
      refs_len = int(psql_k8s(k3s_server,
          "SELECT cardinality(\"references\") FROM narinfo "
          f"WHERE store_path = '{out_consumer}'"
      ))
      assert refs_len >= 1, f"cardinality should be ≥1, got {refs_len}"

      # ── 2. deriver populated ──────────────────────────────────────
      # Name + .drv suffix. NOT the exact /nix/store/HASH-...drv path
      # (would need a nix-instantiate round-trip to learn the hash);
      # the name half is deterministic (derivation.name) and .drv
      # distinguishes it from an output path.
      deriver = psql_k8s(k3s_server,
          f"SELECT deriver FROM narinfo WHERE store_path = '{out_consumer}'"
      )
      assert deriver and "rio-refs-consumer" in deriver and deriver.endswith(".drv"), (
          "deriver should be the consumer .drv path "
          f"(/nix/store/HASH-rio-refs-consumer.drv), got: {deriver!r}"
      )

      # ── 3. GC survival: pin consumer, sweep, dep SURVIVES ─────────
      # Backdate BOTH past grace. Without this they'd be auto-roots
      # via the in-grace seed (created_at > now() - grace_hours) and
      # the sweep would be a no-op regardless of refs correctness.
      psql_k8s(k3s_server,
          "UPDATE narinfo SET created_at = now() - interval '25 hours' "
          f"WHERE store_path IN ('{out_dep}', '{out_consumer}')"
      )
      pin_live(out_consumer, "vm-refs-e2e")
      # force=true: dep is sweep-eligible (past grace, unpinned, and
      # unreachable from any root EXCEPT via consumer's refs) and dep
      # has refs=[] (its output is plain text with zero store paths).
      # That's 1-of-N-eligible with empty refs → ≥10% → gate trips.
      # force bypasses the gate; the SURVIVAL assertion below is what
      # proves correctness, not the gate.
      result = sched_grpc(
          '{"dry_run": false, "grace_period_hours": 24, "force": true}',
          "rio.admin.AdminService/TriggerGC",
      )
      gc_msgs = grpcurl_json_stream(result)
      final = [m for m in gc_msgs if m.get("isComplete")][-1]
      # proto3 JSON omits zero-valued uint64 fields. pathsCollected=0
      # → the field is ABSENT, not "0". .get() with default "0".
      collected = final.get("pathsCollected", "0")
      assert collected == "0", (
          "with consumer pinned, dep should be reachable via the "
          "mark CTE's walk over narinfo.references → nothing swept; "
          f"got pathsCollected={collected!r}. Either the CTE is not "
          "following the references column, or dep's row was swept "
          f"despite being in the closure. Full: {final!r}"
      )
      # Direct PG check — stronger than pathsCollected. If the CTE
      # walked refs correctly, dep's narinfo row is still there.
      dep_still = psql_k8s(k3s_server,
          f"SELECT 1 FROM narinfo WHERE store_path = '{out_dep}'"
      )
      assert dep_still == "1", (
          "dep MUST survive sweep when consumer is pinned "
          "(reachable via references CTE walk); row is GONE. "
          "mark.rs CTE is not following references."
      )
      cons_still = psql_k8s(k3s_server,
          f"SELECT 1 FROM narinfo WHERE store_path = '{out_consumer}'"
      )
      assert cons_still == "1", "consumer (pinned root) must survive"

      # ── 4. unpin → sweep → both GONE ──────────────────────────────
      # Removes the only root covering these paths. Consumer is now
      # unreachable (no pin, past grace, nothing else references it).
      # Dep is unreachable (consumer was its only referrer; consumer
      # is unreachable). Sweep should collect EXACTLY these two —
      # every other path in PG (out_pin from gc-sweep, busybox seed,
      # earlier subtest outputs) is still in-grace.
      unpin_live(out_consumer, "vm-refs-e2e")
      result = sched_grpc(
          '{"dry_run": false, "grace_period_hours": 24, "force": true}',
          "rio.admin.AdminService/TriggerGC",
      )
      gc_msgs = grpcurl_json_stream(result)
      final = [m for m in gc_msgs if m.get("isComplete")][-1]
      assert final.get("pathsCollected") == "2", (
          "after unpin, BOTH dep+consumer should be swept "
          "(unreachable + past grace); got pathsCollected="
          f"{final.get('pathsCollected')!r}. Full: {final!r}"
      )
      dep_gone = psql_k8s(k3s_server,
          f"SELECT COUNT(*) FROM narinfo WHERE store_path = '{out_dep}'"
      )
      assert dep_gone == "0", "dep should be swept after unpin"
      cons_gone = psql_k8s(k3s_server,
          f"SELECT COUNT(*) FROM narinfo WHERE store_path = '{out_consumer}'"
      )
      assert cons_gone == "0", "consumer should be swept after unpin"
      print("refs-end-to-end PASS: refscan→PG→mark-walks-refs proven")
''
