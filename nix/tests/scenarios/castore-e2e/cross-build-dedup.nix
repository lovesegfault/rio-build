# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # cross-build-dedup — a different drv reuses earlier builds' promotes
  # ══════════════════════════════════════════════════════════════════
  # cold-read promoted head4k and big_a; this distinct derivation reads
  # both. Everything must come from the node digest cache: ≥ 32 MiB of
  # hits, zero remote fetch bytes. The shared-backing-cache read half of
  # what cold-read proved on the write half.
  with subtest("cross-build-dedup: shared inputs served from the node cache"):
      drv_xd, build_xd = submit_drv(
          "${xdedupDrv}",
          extra_args=(
              f"--arg head4kA '(builtins.storePath {p_head4k})' "
              f"--arg bigA '(builtins.storePath {p_big_a})'"
          ),
      )
      pod = castore_pod()
      wait_worker_metric(
          pod,
          "awk '/^rio_builder_objects_cache_bytes/ {if ($2 >= 33000000) ok=1} END {exit !ok}'",
          timeout=300,
          ctx="cross-build-dedup cache hits",
      )

      m = worker_metrics(pod)
      n_hits = series(m, "rio_builder_objects_cache_hit_total")
      n_hit_bytes = series(m, "rio_builder_objects_cache_bytes")
      n_remote_bytes = series(m, "rio_builder_castore_fuse_fetch_bytes_total", must=('hit="remote"',))
      assert n_hits >= 2, (
          f"cross-build-dedup: only {n_hits} node-cache hits (head4k + big_a "
          f"should both hit); open_case: "
          f"{fam(m, 'rio_builder_castore_fuse_open_case_total')!r}"
      )
      assert n_hit_bytes >= 32 * 1024 * 1024, (
          f"cross-build-dedup: only {n_hit_bytes} bytes served from the node "
          f"cache, expected ≥ 32 MiB (big_a)"
      )
      assert n_remote_bytes == 0, (
          f"cross-build-dedup: {n_remote_bytes} bytes re-fetched from the store "
          f"for inputs an earlier build already promoted; fetch_bytes: "
          f"{fam(m, 'rio_builder_castore_fuse_fetch_bytes_total')!r}"
      )

      finish_async(build_xd, "cross-build-dedup")
      print("cross-build-dedup PASS: shared inputs reused, zero remote bytes")
''
