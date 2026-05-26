# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # cold-read — streaming open of a >threshold input + whole-file miss
  # ══════════════════════════════════════════════════════════════════
  # First build to ever touch big_a (32 MiB > 8 MiB stream threshold)
  # and head4k (4 KiB) on this node. Asserted, all during the build's
  # sleep tail (one-shot pod ⇒ counters start at zero):
  #   - the open dispatched as miss_stream (streaming engaged) and the
  #     4 KiB input as miss_small (whole-file JIT fetch);
  #   - open_seconds recorded a streamed="1" sample (the open returned
  #     inside the fill window) and exactly one DAG prefetch ran;
  #   - the dd|cmp in the script passed (streamed prefix == head4k) —
  #     proven by the sentinel read placed after it (a cmp failure
  #     exits the script before the sentinel, so its cache entry never
  #     appears);
  #   - the fill completed and promoted: both digests appear in
  #     /var/rio/cache and the chunk cache is non-empty (host probes);
  #   - rio-mountd served Mount + Promote/PromoteChunks (DS metrics).
  # Latency is printed, never gated.
  with subtest("cold-read: streaming open >threshold, whole-file miss, promote"):
      drv_cold, build_cold = submit_drv(
          "${coldDrv}",
          extra_args=(
              f"--arg bigA '(builtins.storePath {p_big_a})' "
              f"--arg head4kA '(builtins.storePath {p_head4k})' "
              f"--arg sentinel '(builtins.storePath {p_sent_cold})'"
          ),
      )
      pod = castore_pod()
      wait_worker_metric(
          pod, "grep -q 'case=\"miss_stream\"'", timeout=300, ctx="cold-read miss_stream"
      )

      m = worker_metrics(pod)
      n_stream = series(m, "rio_builder_castore_fuse_open_case_total", must=('case="miss_stream"',))
      n_small = series(m, "rio_builder_castore_fuse_open_case_total", must=('case="miss_small"',))
      n_streamed_open = series(m, "rio_builder_castore_fuse_open_seconds_count", must=('streamed="1"',))
      n_prefetch = series(m, "rio_builder_castore_dag_prefetch_seconds_count")
      n_lookup = series(m, "rio_builder_castore_fuse_upcalls_total", must=('op="lookup"',))
      assert n_stream >= 1, (
          f"cold-read: no miss_stream open recorded; open_case series: "
          f"{fam(m, 'rio_builder_castore_fuse_open_case_total')!r}"
      )
      assert n_small >= 1, (
          f"cold-read: no miss_small open recorded (head4k should be a whole-file "
          f"miss); open_case series: {fam(m, 'rio_builder_castore_fuse_open_case_total')!r}"
      )
      assert n_streamed_open >= 1, (
          f"cold-read: open_seconds has no streamed=1 sample; series: "
          f"{fam(m, 'rio_builder_castore_fuse_open_seconds_count')!r}"
      )
      assert n_prefetch == 1, (
          f"cold-read: expected exactly one DAG prefetch, got {n_prefetch}; "
          f"series: {fam(m, 'rio_builder_castore_dag_prefetch_seconds_count')!r}"
      )
      assert n_lookup >= 1, (
          f"cold-read: no lookup upcalls recorded; upcalls series: "
          f"{fam(m, 'rio_builder_castore_fuse_upcalls_total')!r}"
      )
      # Informational latency print (never gated — vm-mountd precedent).
      open_sum = series(m, "rio_builder_castore_fuse_open_seconds_sum")
      open_cnt = series(m, "rio_builder_castore_fuse_open_seconds_count")
      mean_open = (open_sum / open_cnt) if open_cnt else 0.0
      print(
          f"cold-read: opens so far={open_cnt}, mean open latency="
          f"{mean_open:.4f}s (informational)"
      )

      # Fill completion + promote, structurally: both digests appear in
      # the node backing cache and the chunk cache gained entries.
      assert_cached(b3_big_a, "big_a after the streaming fill", timeout=300)
      assert_cached(b3_head4k, "head4k after the whole-file miss", timeout=120)
      k3s_agent.succeed("find /var/rio/chunks -type f -print -quit | grep -q .")

      # The dd|cmp passed: the script reads the sentinel only after the
      # cmp succeeded, and this is the sentinel's first-ever read on the
      # node, so its cache entry appearing is the structural proof the
      # streamed prefix byte-compared equal to head4k.
      assert_cached(b3_sent_cold, "cold-read sentinel (dd|cmp passed)", timeout=180)

      # mountd observability: the DaemonSet served this build's Mount and
      # at least one promote (whole-file or chunk batch).
      md = mountd_metrics()
      n_mount = series(md, "rio_mountd_request_seconds_count", must=('op="mount"',))
      n_promote = series(md, "rio_mountd_request_seconds_count", must=('op="promote"',)) + series(
          md, "rio_mountd_request_seconds_count", must=('op="promote_chunks"',)
      )
      assert n_mount >= 1, (
          f"cold-read: rio-mountd recorded no Mount requests; series: "
          f"{fam(md, 'rio_mountd_request_seconds_count')!r}"
      )
      assert n_promote >= 1, (
          f"cold-read: rio-mountd recorded no Promote/PromoteChunks; series: "
          f"{fam(md, 'rio_mountd_request_seconds_count')!r}"
      )

      finish_async(build_cold, "cold-read")
      print("cold-read PASS: streaming open + whole-file miss + promote verified")
''
