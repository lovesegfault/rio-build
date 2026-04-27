# Spike: FUSE-layer negative-dentry caching via ReplyEntry{ino=0, ttl=MAX}.
#
# Settles ADR-022 §2.4's negative-reply row (review finding M7). The
# kernel's `fuse_lookup_name` says "Zero nodeid is same as -ENOENT, but
# with valid timeout"; `fuse_dentry_revalidate`'s `if (!inode) goto
# invalid` is INSIDE the timeout-expired-or-LOOKUP_EXCL/REVAL/RENAME
# branch — so a negative dentry with entry_valid=MAX is NOT invalidated
# on plain stat(). This test proves it bare-FUSE and under overlay.
{
  pkgs,
  rio-workspace,
}:
pkgs.testers.runNixOSTest {
  name = "spike-fuse-negdentry";

  nodes.machine = _: {
    virtualisation.memorySize = 512;
    boot.kernelModules = [
      "fuse"
      "overlay"
    ];
    environment.systemPackages = [ rio-workspace ];
  };

  testScript = ''
    import re, time

    machine.start()
    machine.wait_for_unit("multi-user.target")

    def counters():
        time.sleep(0.12)  # 50 ms ticker → settle
        out = machine.execute("cat /tmp/counters")[1]
        return {k: int(v) for k, v in re.findall(r"([\w-]+)=(\d+)", out)}

    with subtest("mount bare FUSE"):
        machine.succeed("mkdir -p /mnt/neg")
        machine.succeed(
            "${rio-workspace}/bin/spike_negdentry_fuse /mnt/neg /tmp/counters "
            ">/tmp/fuse.log 2>&1 &"
        )
        machine.wait_until_succeeds("mountpoint -q /mnt/neg", timeout=30)

    with subtest("bare FUSE: ino=0 cached, ENOENT not cached"):
        for _ in range(3):
            machine.fail("stat /mnt/neg/missing-ino0")
            machine.fail("stat /mnt/neg/missing-err")
        c = counters()
        print(f"bare counters: {c}")
        assert c.get("missing-ino0", 0) == 1, (
            f"ino=0 produced {c.get('missing-ino0')} upcalls — "
            "FUSE-layer negative caching does NOT work"
        )
        assert c.get("missing-err", 0) == 3, (
            f"ENOENT produced {c.get('missing-err')} upcalls — expected 3"
        )

    with subtest("overlay over FUSE: same behavior"):
        machine.succeed(
            "mkdir -p /mnt/ov/{upper,work,merged} && "
            "mount -t overlay overlay /mnt/ov/merged "
            "-o userxattr,upperdir=/mnt/ov/upper,workdir=/mnt/ov/work,lowerdir=/mnt/neg"
        )
        for _ in range(3):
            machine.fail("stat /mnt/ov/merged/missing-ino0-ov")
            machine.fail("stat /mnt/ov/merged/missing-err-ov")
        c = counters()
        print(f"overlay counters: {c}")
        # overlay's own dcache absorbs after one lower-miss regardless of
        # FUSE's reply mode (I-043). Both names should be 1 upcall.
        assert c.get("missing-ino0-ov", 0) == 1
        assert c.get("missing-err-ov", 0) == 1, (
            f"overlay did NOT cache lower ENOENT (got {c.get('missing-err-ov')}) — "
            "I-043 assumption broken"
        )

    print(
        "PASS: bare-FUSE ino=0+ttl=MAX caches negatives (1 upcall); "
        "ENOENT does not (3 upcalls); overlay caches both (1 upcall each)"
    )
  '';
}
