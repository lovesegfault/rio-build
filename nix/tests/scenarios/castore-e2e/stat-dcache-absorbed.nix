# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # stat-dcache-absorbed — infinite-TTL metadata, count not clock
  # ══════════════════════════════════════════════════════════════════
  # The build traverses the 50-file meta_tree five times (find + stat on
  # every file). With the castore cache config (infinite entry/attr TTL,
  # READDIRPLUS, FOPEN_CACHE_DIR) the dcache and attr cache absorb the
  # repeats: lookup/getattr/readdir upcalls stay at ~one-traversal
  # counts. A broken TTL multiplies them ~5× — far above the bounds.
  # Bounds are computed from the tree shape plus a fixed slop for
  # busybox, path components, and the sentinel read.
  with subtest("stat-dcache-absorbed: repeated metadata is absorbed by the kernel"):
      drv_dc, build_dc = submit_drv(
          "${dcacheDrv}",
          extra_args=(
              f"--arg metaTree '(builtins.storePath {p_meta_tree})' "
              f"--arg sentinel '(builtins.storePath {p_sent_dcache})'"
          ),
      )
      pod = castore_pod()
      # The sentinel is read after the fifth traversal, and this is its
      # first read on the node — its cache entry appearing means all five
      # traversals are done (the counters below are final for them).
      assert_cached(b3_sent_dcache, "stat-dcache sentinel", timeout=300)

      m = worker_metrics(pod)
      n_lookup = series(m, "rio_builder_castore_fuse_upcalls_total", must=('op="lookup"',))
      n_getattr = series(m, "rio_builder_castore_fuse_upcalls_total", must=('op="getattr"',))
      n_readdir = series(m, "rio_builder_castore_fuse_upcalls_total", must=('op="readdir"',))
      lookup_bound = META_FILES + META_DIRS + 48
      getattr_bound = META_FILES + META_DIRS + 64
      readdir_bound = 2 * META_DIRS + 10
      assert n_lookup <= lookup_bound, (
          f"stat-dcache-absorbed: {n_lookup} lookup upcalls for 5 traversals of "
          f"{META_FILES} files (bound {lookup_bound}) — entry TTL is not "
          f"absorbing repeats; upcalls: "
          f"{fam(m, 'rio_builder_castore_fuse_upcalls_total')!r}"
      )
      assert n_getattr <= getattr_bound, (
          f"stat-dcache-absorbed: {n_getattr} getattr upcalls (bound "
          f"{getattr_bound}) — attr TTL is not absorbing repeats; upcalls: "
          f"{fam(m, 'rio_builder_castore_fuse_upcalls_total')!r}"
      )
      assert n_readdir <= readdir_bound, (
          f"stat-dcache-absorbed: {n_readdir} readdir upcalls (bound "
          f"{readdir_bound}) — directory pages are not being cached; upcalls: "
          f"{fam(m, 'rio_builder_castore_fuse_upcalls_total')!r}"
      )
      print(
          f"stat-dcache-absorbed: lookup={n_lookup}/{lookup_bound} "
          f"getattr={n_getattr}/{getattr_bound} readdir={n_readdir}/{readdir_bound}"
      )

      finish_async(build_dc, "stat-dcache-absorbed")
      print("stat-dcache-absorbed PASS: metadata upcalls stayed at one-traversal counts")
''
