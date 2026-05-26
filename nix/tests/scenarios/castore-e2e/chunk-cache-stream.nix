# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # chunk-cache-stream — streaming fill served from /var/rio/chunks
  # ══════════════════════════════════════════════════════════════════
  # Deterministic recast of the plan's concurrent-builds dedup race:
  # cold-read promoted both big_a's whole file AND its chunks; evict
  # only the whole-file backing entry (host-side, root) and stream it
  # again. The fill must source its chunks from the node chunk cache:
  # chunk_source{node_ssd} ≥ 1, fetch_bytes{node_ssd} > 0, and remote
  # bytes < the input size (strictly: ≈ 0; ≤ 1 MiB slack for any chunk
  # the cold fill's unlink-after-promote batching raced). The backing
  # entry must reappear afterwards (re-promote).
  with subtest("chunk-cache-stream: re-stream after eviction hits the chunk cache"):
      k3s_agent.succeed("find /var/rio/chunks -type f -print -quit | grep -q .")
      k3s_agent.succeed(f"rm {cache_path(b3_big_a)}")

      drv_ch, build_ch = submit_drv(
          "${chunkhitDrv}",
          extra_args=f"--arg bigA '(builtins.storePath {p_big_a})'",
      )
      pod = castore_pod()
      # The fill re-promotes the whole file once it completes — that
      # reappearance is the structural "fill done, counters final" gate.
      assert_cached(b3_big_a, "big_a re-promoted after eviction", timeout=300)

      m = worker_metrics(pod)
      n_local_chunks = series(m, "rio_builder_castore_fuse_chunk_source_total", must=('src="node_ssd"',))
      n_local_bytes = series(m, "rio_builder_castore_fuse_fetch_bytes_total", must=('hit="node_ssd"',))
      n_remote_bytes = series(m, "rio_builder_castore_fuse_fetch_bytes_total", must=('hit="remote"',))
      assert n_local_chunks >= 1, (
          f"chunk-cache-stream: no chunks served from the node chunk cache; "
          f"chunk_source: {fam(m, 'rio_builder_castore_fuse_chunk_source_total')!r}"
      )
      assert n_local_bytes > 0, (
          f"chunk-cache-stream: zero bytes filled from /var/rio/chunks; "
          f"fetch_bytes: {fam(m, 'rio_builder_castore_fuse_fetch_bytes_total')!r}"
      )
      assert n_remote_bytes <= 1024 * 1024, (
          f"chunk-cache-stream: {n_remote_bytes} bytes re-fetched remotely for a "
          f"fully chunk-cached 32 MiB input (allowing ≤1 MiB slack); fetch_bytes: "
          f"{fam(m, 'rio_builder_castore_fuse_fetch_bytes_total')!r}"
      )
      print(
          f"chunk-cache-stream: local chunks={n_local_chunks}, local bytes="
          f"{n_local_bytes}, remote bytes={n_remote_bytes}"
      )

      finish_async(build_ch, "chunk-cache-stream")
      print("chunk-cache-stream PASS: fill served from the node chunk cache")
''
