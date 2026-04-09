# scheduling subtest fragment — composed by scenarios/scheduling.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # fuse-direct — readdir/access on the FUSE mount (bypasses overlay)
  # ══════════════════════════════════════════════════════════════════
  # chain.nix:43 does `ls -la $dep/` INSIDE the build sandbox (overlay
  # lower 2 = FUSE). ops.rs readdir() stays 0 hits even in lifecycle-k3s
  # — overlayfs is not delegating FUSE_READDIR to the lower. This subtest
  # ls's the FUSE mount DIRECTLY (no overlay) to prove readdir() is
  # reachable at all; if THIS is 0 hits, the problem is fuser/kernel,
  # not overlayfs.
  with subtest("fuse-direct: readdir/access on FUSE mount (overlay bypass)"):
      # Both small workers fetched ≥1 path (asserted above). `ls` on
      # /var/rio/fuse-store (the mount point, not /var/rio/cache) hits
      # FUSE readdir(ino=ROOT) → fs::read_dir(cache_dir).
      for w in small_workers:
          listing = w.succeed("ls -la /var/rio/fuse-store/ 2>&1")
          print(f"{w.name} fuse-store root:\n{listing}")
          # access(R_OK) via faccessat(2). make_fuse_config (fuse/mod.rs
          # :189) does NOT set MountOption::DefaultPermissions → kernel
          # forwards the permission check to userspace access().
          w.succeed("test -r /var/rio/fuse-store/")

      # Subdir readdir dropped — one-shot builder restarts with a
      # fresh :memory: index after every build, so paths cached by
      # the prior process aren't lookup-able on the new mount.
      # /var/rio/cache/ on disk has stale dirs (clean_stale_tmp_dirs
      # was removed) that root readdir lists but lookup() can't
      # resolve. Root readdir + access above proves readdir() fires;
      # overlay-readdir below proves the in-build path.
''
