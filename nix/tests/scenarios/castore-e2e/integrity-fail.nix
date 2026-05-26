# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # integrity-fail — corrupted content can never be served to a build
  # ══════════════════════════════════════════════════════════════════
  # The corruption is injected into the NODE chunk cache, not the store
  # backend: the store verifies chunks on read (backend corruption
  # surfaces as a store-side error, never reaching the builder's
  # verifier), while node-cache chunks are only size-checked on read —
  # the streaming fill's whole-file BLAKE3 is the layer that must catch
  # it. Size-preserving byte flips (conv=notrunc), never truncation,
  # or the size probe would silently re-fetch and the subtest would be
  # vacuous.
  #
  # Expected: integrity_fail_total == 1, the blocked reader gets EIO
  # (dd fails inside the script), the corrupted bytes came from the
  # local chunk cache, and the digest is NOT promoted back into
  # /var/rio/cache. The build is cancelled after evidence capture (its
  # script never writes $out, so wrong bytes can never become an
  # output), and the corrupted chunk is removed so later subtests
  # re-fetch cleanly.
  with subtest("integrity-fail: corrupted node-cache chunk fails the fill, no promote"):
      # Force a re-fill of big_f and corrupt one of its cached chunks.
      k3s_agent.succeed(f"rm {cache_path(b3_big_f)}")
      victim_chunk = k3s_agent.succeed(
          "find /var/rio/chunks -type f -print -quit"
      ).strip()
      assert victim_chunk, "integrity-fail: node chunk cache is empty (chunk-warm should have filled it)"
      k3s_agent.succeed(
          f"dd if=/dev/urandom of={victim_chunk} bs=1 count=64 seek=1024 conv=notrunc 2>/dev/null"
      )
      print(f"integrity-fail: corrupted 64 bytes of {victim_chunk}")

      drv_corrupt, build_corrupt = submit_drv(
          "${corruptDrv}",
          extra_args=f"--arg bigF '(builtins.storePath {p_big_f})'",
      )
      pod = castore_pod()
      wait_worker_metric(
          pod,
          "grep -q '^rio_builder_castore_fuse_integrity_fail_total'",
          timeout=300,
          ctx="integrity-fail verification",
      )

      m = worker_metrics(pod)
      n_integrity = series(m, "rio_builder_castore_fuse_integrity_fail_total")
      n_eio = series(m, "rio_builder_castore_fuse_eio_total")
      n_local_chunks = series(m, "rio_builder_castore_fuse_chunk_source_total", must=('src="node_ssd"',))
      assert n_integrity == 1, (
          f"integrity-fail: integrity_fail_total = {n_integrity}, expected exactly "
          f"1 (this pod ran exactly one fill)"
      )
      assert n_eio >= 1, (
          f"integrity-fail: no EIO surfaced to the reader; eio_total={n_eio}"
      )
      assert n_local_chunks >= 1, (
          f"integrity-fail: the fill never read from the local chunk cache — the "
          f"corruption was not exercised; chunk_source: "
          f"{fam(m, 'rio_builder_castore_fuse_chunk_source_total')!r}"
      )
      # The poisoned content must not be published: no backing-cache
      # entry for big_f reappears.
      assert_not_cached(b3_big_f, "big_f after the corrupted fill")
      # The corruption never reached the store layer (informational —
      # the store served nothing for this fill).
      sm = store_metrics()
      print(
          "integrity-fail: store integrity counters (expect zero/absent): "
          f"{fam(sm, 'rio_store_integrity_failures_total')!r}"
      )

      finish_async(build_corrupt, "integrity-fail")
      # Drop the corrupted chunk so any later read of big_f re-fetches
      # remotely instead of tripping on it again.
      k3s_agent.succeed(f"rm -f {victim_chunk}")
      assert_not_cached(b3_big_f, "big_f after cleanup")
      print("integrity-fail PASS: corrupted fill rejected, nothing promoted")
''
