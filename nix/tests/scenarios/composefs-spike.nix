# Spike: composefs-style overlay (EROFS metadata + redirect-xattr → FUSE
# digest store) — does the warm read path hit zero FUSE upcalls?
#
# Hypothesis under test: with `lowerdir=/mnt/meta::/mnt/objects` (data-only
# lower), `metacopy=on`, `redirect_dir=on`, and the FUSE open() returning
# FOPEN_KEEP_CACHE, the SECOND `dd` of a file produces zero `read` upcalls.
#
# This is a spike — single VM, no rio services, no fixtures.
{
  pkgs,
  rio-workspace,
}:
let
  digestAA = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
  digestBB = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
  digestCC = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
in
pkgs.testers.runNixOSTest {
  name = "composefs-spike";

  nodes.machine = _: {
    virtualisation.memorySize = 1024;
    boot.kernelModules = [
      "erofs"
      "overlay"
      "loop"
    ];
    environment.systemPackages = [
      pkgs.erofs-utils
      pkgs.attr
      pkgs.util-linux
      rio-workspace
    ];
  };

  testScript = ''
    import re, time

    machine.start()
    machine.wait_for_unit("multi-user.target")

    def counters():
        # 50ms ticker in the FUSE daemon → settle 120ms before reading.
        time.sleep(0.12)
        out = machine.succeed("cat /tmp/counters")
        return {k: int(v) for k, v in re.findall(r"(\w+)=(\d+)", out)}

    with subtest("mount FUSE digest store"):
        machine.succeed("mkdir -p /mnt/objects /mnt/meta /mnt/merged")
        machine.succeed(
            "${rio-workspace}/bin/spike_digest_fuse /mnt/objects /tmp/counters "
            ">/tmp/fuse.log 2>&1 &"
        )
        machine.wait_until_succeeds("mountpoint -q /mnt/objects", timeout=30)
        # sanity: FUSE serves the file directly
        out = machine.succeed("stat -c %s /mnt/objects/aa/${digestAA}")
        assert out.strip() == "1048576", f"FUSE direct stat size: {out}"

    with subtest("build EROFS metadata image with redirect xattrs"):
        machine.succeed("mkdir -p /tmp/stage")
        for name, prefix, digest in [
            ("file1", "aa", "${digestAA}"),
            ("file2", "bb", "${digestBB}"),
            ("file3", "cc", "${digestCC}"),
        ]:
            machine.succeed(f"touch /tmp/stage/{name}")
            # redirect: absolute path resolved against the data-only lower's root
            machine.succeed(
                f"setfattr -n trusted.overlay.redirect -v /{prefix}/{digest} /tmp/stage/{name}"
            )
            # ovl_metacopy struct: {version=0, len=4, flags=0, padding=0}.
            # Kernel rejects 1..3 bytes with "too small xattr"; 0 bytes is the
            # legacy form, 4+ is the structured form.
            machine.succeed(
                f"setfattr -n trusted.overlay.metacopy -v 0x00040000 /tmp/stage/{name}"
            )
        machine.succeed("mkfs.erofs -x1 /tmp/meta.erofs /tmp/stage 2>&1")
        machine.succeed("mount -t erofs -o loop,ro /tmp/meta.erofs /mnt/meta")
        out = machine.succeed("getfattr -n trusted.overlay.redirect /mnt/meta/file1 2>&1")
        assert "${digestAA}" in out, f"redirect xattr not preserved in EROFS: {out}"

    with subtest("mount overlay (meta :: data-only FUSE)"):
        # `::` marks /mnt/objects as a data-only lower — only reachable via
        # redirect, not via path lookup. This is the composefs topology.
        machine.succeed(
            "mount -t overlay overlay /mnt/merged "
            "-o ro,lowerdir=/mnt/meta::/mnt/objects,metacopy=on,redirect_dir=on"
        )
        machine.succeed("ls -la /mnt/merged")

    base = counters()
    print(f"baseline counters after mount: {base}")

    with subtest("stat does NOT upcall to FUSE (EROFS owns metadata)"):
        before = counters()
        machine.succeed("stat /mnt/merged/file1 >/dev/null")
        after = counters()
        d_lookup = after["lookup"] - before["lookup"]
        d_getattr = after["getattr"] - before["getattr"]
        print(f"stat deltas: lookup={d_lookup} getattr={d_getattr}")
        # overlayfs may stat the lower root once at mount; per-file stat should
        # add zero. Allow ≤1 here as a soft check; the hard assertion is on read.
        assert d_lookup + d_getattr <= 1, (
            f"stat caused FUSE upcalls: lookup+{d_lookup} getattr+{d_getattr}"
        )

    with subtest("cold read: content correct, upcalls observed"):
        before = counters()
        out = machine.succeed("head -c1 /mnt/merged/file1 | od -An -tx1")
        assert out.strip() == "aa", f"content byte mismatch: {out!r}"
        machine.succeed("dd if=/mnt/merged/file1 of=/dev/null bs=4k 2>&1")
        after = counters()
        cold_reads = after["read"] - before["read"]
        cold_opens = after["open"] - before["open"]
        print(f"cold: read+={cold_reads} open+={cold_opens}")
        assert cold_reads > 0, "expected nonzero read upcalls on cold path"
        assert cold_opens > 0, "expected nonzero open upcalls on cold path"

    with subtest("warm read: ZERO read upcalls (the hypothesis)"):
        before = counters()
        machine.succeed("dd if=/mnt/merged/file1 of=/dev/null bs=4k 2>&1")
        after = counters()
        warm_reads = after["read"] - before["read"]
        warm_opens = after["open"] - before["open"]
        print(f"warm: read+={warm_reads} open+={warm_opens}")
        # THE assertion this spike exists to make:
        assert warm_reads == 0, (
            f"HYPOTHESIS FAIL: warm read produced {warm_reads} FUSE read upcalls; "
            f"overlay+metacopy+FUSE-lower does NOT give kernel-native warm reads"
        )
        # open may still upcall (one per dd) — that's fine, it's O(1) not O(bytes)
        print(f"(warm open upcalls = {warm_opens}; informational, not asserted)")

    with subtest("cross-file: file2 cold then warm"):
        machine.succeed("dd if=/mnt/merged/file2 of=/dev/null bs=4k 2>&1")
        before = counters()
        machine.succeed("dd if=/mnt/merged/file2 of=/dev/null bs=4k 2>&1")
        after = counters()
        warm2 = after["read"] - before["read"]
        assert warm2 == 0, f"file2 warm read upcalls = {warm2}"

    print("PASS: composefs-style overlay-on-FUSE-lower gives zero warm-read upcalls")
  '';
}
