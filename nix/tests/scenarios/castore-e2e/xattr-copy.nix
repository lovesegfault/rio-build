# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # xattr-copy — listxattr size-branch under a real archiver
  # ══════════════════════════════════════════════════════════════════
  # GNU `cp -a` (static coreutils, seeded as a tenant source) copies a
  # file and a directory tree out of the castore lower. cp -a probes
  # llistxattr with size=0 then size>0 on every source inode; the
  # size>0 branch must answer an empty list, not EIO (the regression
  # that broke shutil.copy2 — and overlayfs probes user.overlay.* on
  # every lower inode too). Synchronous build: success + byte equality
  # is the assertion. (The plan called this subtest shutil-copy2;
  # python3 in the sandbox would balloon the seed, GNU cp exercises the
  # same llistxattr branch.)
  with subtest("xattr-copy: cp -a out of the castore lower succeeds"):
      out_xattr = build(
          "${xattrDrv}",
          extra_args=(
              "--arg coreutils '(builtins.storePath ${coreutilsStatic})' "
              f"--arg small1m '(builtins.storePath {p_small1m})' "
              f"--arg metaTree '(builtins.storePath {p_meta_tree})'"
          ),
      )
      print(f"xattr-copy PASS: cp -a of file + tree succeeded ({out_xattr})")
''
