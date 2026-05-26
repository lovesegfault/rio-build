# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # warm-read — second build of the same input: passthrough on hit
  # ══════════════════════════════════════════════════════════════════
  # cold-read promoted big_a into /var/rio/cache; this is a DIFFERENT
  # derivation on the same node re-reading it. Because the pod is
  # one-shot (counters from zero) the assertions are absolute:
  #   - the open hit the node digest cache (objects_cache_hit ≥ 1,
  #     case=hit ≥ 1) and was served via passthrough;
  #   - zero read upcalls, zero keep_cache degrades, zero fetched bytes
  #     — nothing was re-fetched, busybox included;
  #   - no open took the remote path (open_seconds{hit="remote"} empty).
  # Gate: objects_cache_bytes ≥ 32 MiB — only big_a's hit can push the
  # served-bytes counter past its own size, so the dd has run by then.
  with subtest("warm-read: same node, same input — passthrough hit, nothing fetched"):
      k3s_agent.succeed(f"test -e {cache_path(b3_big_a)}")
      drv_warm, build_warm = submit_drv(
          "${warmDrv}",
          extra_args=f"--arg bigA '(builtins.storePath {p_big_a})'",
      )
      pod = castore_pod()
      wait_worker_metric(
          pod,
          "awk '/^rio_builder_objects_cache_bytes/ {if ($2 >= 33000000) ok=1} END {exit !ok}'",
          timeout=300,
          ctx="warm-read big_a cache hit",
      )

      m = worker_metrics(pod)
      n_hit_counter = series(m, "rio_builder_objects_cache_hit_total")
      n_hit_case = series(m, "rio_builder_castore_fuse_open_case_total", must=('case="hit"',))
      n_read_upcalls = series(m, "rio_builder_castore_fuse_upcalls_total", must=('op="read"',))
      n_keep_cache = series(m, "rio_builder_castore_fuse_open_mode_total", must=('mode="keep_cache"',))
      n_fetched = series(m, "rio_builder_castore_fuse_fetch_bytes_total")
      n_remote_open = series(m, "rio_builder_castore_fuse_open_seconds_count", must=('hit="remote"',))
      assert n_hit_counter >= 1 and n_hit_case >= 1, (
          f"warm-read: expected node-cache hits (objects_cache_hit={n_hit_counter}, "
          f"case=hit={n_hit_case}); open_case series: "
          f"{fam(m, 'rio_builder_castore_fuse_open_case_total')!r}"
      )
      assert n_read_upcalls == 0, (
          f"warm-read: {n_read_upcalls} read upcalls — passthrough is not "
          f"engaging; upcalls: {fam(m, 'rio_builder_castore_fuse_upcalls_total')!r}"
      )
      assert n_keep_cache == 0, (
          f"warm-read: {n_keep_cache} keep_cache opens — a hit degraded to "
          f"userspace reads; open_mode: "
          f"{fam(m, 'rio_builder_castore_fuse_open_mode_total')!r}"
      )
      assert n_fetched == 0, (
          f"warm-read: {n_fetched} bytes fetched — a warm input was re-fetched; "
          f"fetch_bytes: {fam(m, 'rio_builder_castore_fuse_fetch_bytes_total')!r}"
      )
      assert n_remote_open == 0, (
          f"warm-read: {n_remote_open} opens took the remote path; open_seconds: "
          f"{fam(m, 'rio_builder_castore_fuse_open_seconds_count')!r}"
      )

      finish_async(build_warm, "warm-read")
      print("warm-read PASS: backing-cache hit, passthrough, zero re-fetch")
''
