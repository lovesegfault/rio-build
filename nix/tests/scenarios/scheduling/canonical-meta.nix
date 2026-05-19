# scheduling subtest fragment — composed by scenarios/scheduling.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # canonical-metadata — does a build see mtime=1 on FUSE-served inputs?
  # ══════════════════════════════════════════════════════════════════
  # rio's chroot store is overlay(lower=FUSE, upper=local-SSD). The
  # FUSE attribute layer (stat_to_attr) MUST present canonical Nix
  # store-path metadata regardless of the on-disk cache state — Nix's
  # reference daemon does this via canonicalisePathMetaData (mtime=1,
  # perms 444/555, root:root). This is load-bearing: nixpkgs'
  # set-source-date-epoch-to-latest.sh postUnpackHook scans
  # $sourceRoot for the newest regular file and raises
  # SOURCE_DATE_EPOCH to it. With mtime≈now, every tar-producing FOD
  # (fetchPnpmDeps/fetchYarnDeps/…) bakes a different timestamp into
  # its archive each run → non-deterministic NAR hash → permanent
  # FOD failure on rio.
  #
  # canonical-meta.nix stats $busybox (a FUSE-served input) and
  # writes `<mtime> <perm>`. The unit tests cover stat_to_attr() and
  # restore_node() in isolation; this proves the deployed binary +
  # overlay mount config actually presents canonical metadata to a
  # real build through the gateway path.
  with subtest("canonical-metadata: FUSE-served input has mtime=1"):
      out = build("${drvs.canonicalMeta}", capture_stderr=False).strip()
      meta = client.succeed(
          f"nix store cat --store 'ssh-ng://${gatewayHost}' {out}"
      ).strip()
      # 1 = one second past Epoch. 555 = exec/dir perm (busybox is a
      # multi-call binary; the symlink it's stat'd through gives 555).
      # Not 0 (some tools treat 0 as "no timestamp"), and never the
      # wall clock.
      assert meta == "1 555", (
          f"FUSE-served input metadata is {meta!r}, expected '1 555'. "
          f"A wall-clock mtime here means restore_path_streaming or "
          f"stat_to_attr regressed — every tar-producing FOD on rio "
          f"will hash-mismatch non-deterministically."
      )
''
