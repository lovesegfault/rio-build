# scheduling subtest fragment — composed by scenarios/scheduling.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # fuse-listxattr — listxattr(2) with size>0 returns empty, not EIO
  # ══════════════════════════════════════════════════════════════════
  # CPython os.listxattr calls listxattr(path, buf, 256) FIRST (no
  # size=0 probe), so this hits the size>0 branch directly. Before the
  # ops.rs fix, reply.size(0) on a size>0 request serialized an 8-byte
  # fuse_getxattr_out; kernel fuse_verify_xattr_list saw a zero-length
  # name → -EIO → OSError here. Direct on /var/rio/fuse-store (FUSE
  # root ino, no overlay) — same shape as fuse-direct.
  with subtest("fuse-listxattr: empty list (not EIO) on FUSE mount"):
      for w in all_workers:
          out = w.succeed(
              "${pkgs.python3}/bin/python3 -c "
              "'import os; r = os.listxattr(\"/var/rio/fuse-store\"); "
              "print(r); assert r == [], r'"
          )
          print(f"{w.name} listxattr: {out.strip()}")
''
