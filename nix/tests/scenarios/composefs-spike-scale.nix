# Scale-test: composefs-style overlay against a chromium-shaped closure.
#
# Real chromium-146 closure topology (357 store paths, 23 218 regular files,
# 8 221 dirs, 3 374 symlinks — captured on host, content synthetic). Uses
# `mkcomposefs --from-file` so stub inodes carry real i_size with zero data
# blocks. Asserts: stat() correctness, warm-read upcalls == 0, kernel-side
# stat-walk over the full tree. Companion to composefs-spike.nix.
{
  pkgs,
  rio-workspace,
}:
let
  treeTsv = ../lib/chromium-tree.tsv.zst;
  stagePy = ../lib/spike_stage.py;
in
pkgs.testers.runNixOSTest {
  name = "composefs-spike-scale";
  skipTypeCheck = true;

  nodes.machine = _: {
    virtualisation.memorySize = 3072;
    virtualisation.diskSize = 4096;
    boot.kernelModules = [
      "erofs"
      "overlay"
      "loop"
    ];
    environment.systemPackages = [
      pkgs.composefs
      pkgs.erofs-utils
      pkgs.attr
      pkgs.util-linux
      pkgs.zstd
      pkgs.python3
      pkgs.time
      rio-workspace
    ];
  };

  testScript = ''
    import json, re, time

    machine.start()
    machine.wait_for_unit("multi-user.target")

    def counters():
        # 50ms ticker in the FUSE daemon → settle 120ms before reading.
        time.sleep(0.12)
        out = machine.succeed("cat /tmp/counters")
        return {k: int(v) for k, v in re.findall(r"(\w+)=(\d+)", out)}

    def timed(cmd):
        out = machine.succeed(
            "/run/current-system/sw/bin/time -f 'WALL=%e' "
            f"bash -c {cmd!r} 2>&1"
        )
        m = re.search(r"WALL=([\d.]+)", out)
        return float(m.group(1)), out

    R = {}

    with subtest("build EROFS metadata image via mkcomposefs --from-file"):
        machine.succeed("mkdir -p /mnt/objects /mnt/meta /mnt/merged")
        machine.succeed("zstd -d ${treeTsv} -o /tmp/tree.tsv")
        machine.succeed(
            "python3 ${stagePy} /tmp/tree.tsv /tmp/dump.txt "
            "/tmp/fuse-manifest /tmp/samples.json 2>&1"
        )
        samples = json.loads(machine.succeed("cat /tmp/samples.json"))
        R["n_files"] = samples["n_files"]
        R["total_size_manifest"] = samples["total_size"]
        print(f"dump: {R['n_files']} files, total {R['total_size_manifest']} bytes")

        wall, out = timed("mkcomposefs --from-file /tmp/dump.txt /tmp/meta.erofs")
        R["mkcomposefs_secs"] = wall
        R["erofs_image_bytes"] = int(
            machine.succeed("stat -c %s /tmp/meta.erofs").strip()
        )
        print(f"mkcomposefs: {wall:.2f}s, image={R['erofs_image_bytes']} bytes")
        machine.succeed("mount -t erofs -o loop,ro /tmp/meta.erofs /mnt/meta")

        # Gotcha check: does mkcomposefs set fs-verity digest in metacopy when
        # the dump's DIGEST field is '-'? (Expect: no digest, just the marker.)
        meta_xattr = machine.succeed(
            f"getfattr -e hex -n trusted.overlay.metacopy "
            f"/mnt/meta/{samples['shallow']!r} 2>&1 || true"
        )
        R["metacopy_xattr_sample"] = meta_xattr.strip()
        print(f"metacopy xattr: {meta_xattr.strip()}")

    with subtest("mount FUSE digest store with chromium manifest"):
        machine.succeed(
            "${rio-workspace}/bin/spike_digest_fuse /mnt/objects /tmp/counters "
            "/tmp/fuse-manifest >/tmp/fuse.log 2>&1 & echo $! >/tmp/fuse.pid"
        )
        machine.wait_until_succeeds("mountpoint -q /mnt/objects", timeout=30)

    base = counters()

    with subtest("overlay mount latency (the headline number)"):
        wall, _ = timed(
            "mount -t overlay overlay /mnt/merged "
            "-o ro,lowerdir=/mnt/meta::/mnt/objects,metacopy=on,redirect_dir=on"
        )
        after = counters()
        R["overlay_mount_secs"] = wall
        R["mount_upcalls"] = {k: after[k] - base[k] for k in after}
        print(f"overlay mount: {wall*1000:.1f}ms, FUSE upcalls={R['mount_upcalls']}")

    with subtest("stat() returns real i_size (the fix this commit validates)"):
        for label in ("shallow", "deep", "large"):
            p = "/mnt/merged/" + samples[label]
            want = samples[f"{label}_size"]
            got = int(machine.succeed(f"stat -c %s {p!r}").strip())
            print(f"stat {label}: want={want} got={got}")
            assert got == want, f"{label}: stat size {got} != manifest {want}"
            R[f"{label}_stat_size"] = got

    with subtest("cold + warm reads: byte-count matches stat, warm upcalls 0"):
        for label in ("shallow", "deep", "large"):
            p = "/mnt/merged/" + samples[label]
            want = samples[f"{label}_size"]
            before = counters()
            t_cold, out_cold = timed(f"dd if={p!r} of=/dev/null bs=64k")
            mid = counters()
            t_warm, out_warm = timed(f"dd if={p!r} of=/dev/null bs=64k")
            aft = counters()
            cold = {k: mid[k] - before[k] for k in mid}
            warm = {k: aft[k] - mid[k] for k in aft}
            # dd reports "N bytes (...) copied"
            dd_bytes = int(re.search(r"(\d+) bytes", out_cold).group(1))
            R[f"{label}_dd_bytes"] = dd_bytes
            R[f"{label}_cold"] = {"secs": t_cold, **cold}
            R[f"{label}_warm"] = {"secs": t_warm, **warm}
            print(
                f"{label}: dd={dd_bytes}B cold={t_cold:.3f}s {cold} | "
                f"warm={t_warm:.3f}s {warm}"
            )
            assert dd_bytes == want, f"{label}: dd read {dd_bytes} != stat {want}"
            assert warm["read"] == 0, (
                f"{label}: warm read produced {warm['read']} FUSE read upcalls"
            )

    with subtest("full stat-walk: total size matches, zero FUSE upcalls"):
        before = counters()
        wall, _ = timed("find /mnt/merged -type f -printf x%sx >/tmp/sizes")
        aft = counters()
        walk = {k: aft[k] - before[k] for k in aft}
        total = sum(
            int(s) for s in machine.succeed("cat /tmp/sizes").split("x") if s
        )
        R["stat_walk_secs"] = wall
        R["stat_walk_total"] = total
        R["stat_walk_upcalls"] = walk
        print(f"stat-walk: total={total} in {wall:.2f}s, FUSE upcalls={walk}")
        assert total == R["total_size_manifest"], (
            f"stat-walk total {total} != manifest {R['total_size_manifest']}"
        )
        assert all(walk[k] == 0 for k in ("lookup", "getattr", "open", "read")), (
            f"stat-walk hit FUSE: {walk}"
        )

    with subtest("FUSE peak RSS"):
        pid = machine.succeed("cat /tmp/fuse.pid").strip()
        hwm = machine.succeed(f"grep VmHWM /proc/{pid}/status")
        R["fuse_vmhwm"] = hwm.strip()
        print(f"FUSE {hwm.strip()}")

    print("=== SCALE RESULTS (mkcomposefs encoder) ===")
    print(json.dumps(R, indent=2))
  '';
}
