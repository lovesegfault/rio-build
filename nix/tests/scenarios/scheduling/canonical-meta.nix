# scheduling subtest fragment — composed by scenarios/scheduling.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # canonical-metadata — does a build see mtime=1 on castore-served inputs?
  # ══════════════════════════════════════════════════════════════════
  # rio's chroot store is overlay(lower=castore-FUSE, upper=local-SSD).
  # The FUSE attribute layer (castore_fuse/tree.rs make_attr) MUST
  # present canonical Nix store-path metadata regardless of the backing
  # cache state — Nix's reference daemon does this via
  # canonicalisePathMetaData (mtime=1, perms 444/555/777, root:root).
  # This is load-bearing: nixpkgs' set-source-date-epoch-to-latest.sh
  # postUnpackHook scans $sourceRoot for the newest regular file and
  # raises SOURCE_DATE_EPOCH to it. With mtime≈now, every tar-producing
  # FOD (fetchPnpmDeps/fetchYarnDeps/…) bakes a different timestamp into
  # its archive each run → non-deterministic NAR hash → permanent
  # FOD failure on rio.
  #
  # canonical-meta.nix stats two castore-served inputs (the busybox ELF
  # and a symlink to it) and writes `<mtime> <perm>` for each. The
  # tree.rs unit tests cover the attr presentation in isolation; this
  # proves the deployed binary + overlay mount config actually presents
  # canonical metadata to a real build through the gateway path.
  with subtest("canonical-metadata: castore-served input has mtime=1"):
      out = build("${drvs.canonicalMeta}", capture_stderr=False).strip()
      meta = client.succeed(
          f"nix store cat --store 'ssh-ng://${gatewayHost}' {out}"
      ).strip()
      # Two probe lines, one per stat target:
      #   1 555 — busybox itself, a regular executable ELF (exec/file canonical perm).
      #   1 777 — the `sh` symlink to busybox (symlink canonical perm; Linux symlinks
      #           are always S_IFLNK | 0o777 — Nix never chmods them).
      # mtime=1 = one second past Epoch on both lines. Not 0 (some tools treat 0 as
      # "no timestamp"), and never the wall clock.
      lines = meta.splitlines()
      assert lines == ["1 555", "1 777"], (
          f"castore-served input metadata is {lines!r}, expected ['1 555', '1 777']. "
          f"A wall-clock mtime here means the castore tree's make_attr regressed; "
          f"a 555 on the symlink line means symlinks are not presented as 0o777 "
          f"(cross-builder non-determinism for builds that read input symlink "
          f"perms — tar -p, cpio, stat -c %a, find -perm)."
      )
''
