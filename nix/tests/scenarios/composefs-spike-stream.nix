# Spike: streaming-open for the composefs digest-FUSE (P0575 / ADR-022 §C.7).
#
# Tested bare-FUSE (no overlay/erofs layer). The overlay stack is already
# validated by composefs-spike{,-scale}; streaming-open is a FUSE-handler
# behavior orthogonal to whether overlay sits on top.
#
# Asserts:
#   1. open() returns after first chunk (~10 ms), not whole-file (~2.5 s)
#   2. read() of an unfilled range blocks until the fill HWM reaches it
#   3. full sequential read after fill complete populates page cache
#   4. second full read = 0 FUSE read upcalls (FOPEN_KEEP_CACHE warm path)
#   5. close+reopen+read = 0 read upcalls (KEEP_CACHE survives)
#   6. mmap() page faults route through FUSE read
{
  pkgs,
  rio-workspace,
}:
pkgs.testers.runNixOSTest {
  name = "composefs-spike-stream";

  nodes.machine = _: {
    # 256 MiB file in page cache + headroom.
    virtualisation.memorySize = 1536;
    boot.kernelModules = [ "fuse" ];
    environment.systemPackages = [
      pkgs.python3
      rio-workspace
    ];
  };

  testScript = ''
    import re, time

    DIGEST = "ee" * 32
    PATH = f"/mnt/objects/ee/{DIGEST}"
    FILE_SIZE = 256 * 1024 * 1024
    CHUNK_MS = 10

    machine.start()
    machine.wait_for_unit("multi-user.target")

    def counters():
        time.sleep(0.12)  # 50 ms ticker → settle
        out = machine.succeed("cat /tmp/counters")
        return {k: int(v) for k, v in re.findall(r"(\w+)=(\d+)", out)}

    def must_match(pat: str, s: str) -> "re.Match[str]":
        m = re.search(pat, s)
        assert m is not None, f"{pat!r} no match in {s!r}"
        return m

    with subtest("mount streaming FUSE"):
        machine.succeed("mkdir -p /mnt/objects")
        machine.succeed(
            f"STREAM_CHUNK_MS={CHUNK_MS} "
            "${rio-workspace}/bin/spike_stream_fuse /mnt/objects /tmp/counters "
            ">/tmp/fuse.log 2>&1 &"
        )
        machine.wait_until_succeeds("mountpoint -q /mnt/objects", timeout=30)
        c = counters()
        assert c["hwm"] == 0, f"fill should be lazy: hwm={c['hwm']}"

    with subtest("open() latency: first-chunk not whole-file"):
        # Time inside the VM to exclude test-harness RPC overhead. The fill
        # thread lives in the FUSE daemon — fill continues after this python3
        # process exits.
        out = machine.succeed(
            "python3 -c '"
            "import os,time; "
            f"t0=time.perf_counter(); fd=os.open(\"{PATH}\", os.O_RDONLY); "
            "el=time.perf_counter()-t0; "
            "print(f\"OPEN_MS={el*1000:.1f}\")'"
        )
        open_ms = float(must_match(r"OPEN_MS=([\d.]+)", out).group(1))
        c = counters()
        print(f"open() took {open_ms:.1f} ms; counters={c}")
        # First chunk ≈ CHUNK_MS; allow generous slack for scheduler/condvar.
        assert open_ms < 80, f"open() blocked too long ({open_ms:.1f} ms) — not streaming"
        assert open_ms >= CHUNK_MS * 0.5, f"open() too fast ({open_ms:.1f} ms) — fill not awaited?"
        assert c["open"] == 1
        assert c["open_us"] / 1000 < 80
        assert 0 < c["hwm"] < FILE_SIZE, f"fill not in progress: hwm={c['hwm']}"

    with subtest("read offset 0: already filled, fast"):
        out = machine.succeed(
            "python3 -c '"
            "import os,time; "
            f"fd=os.open(\"{PATH}\", os.O_RDONLY); "
            "t0=time.perf_counter(); b=os.pread(fd,4096,0); "
            "el=time.perf_counter()-t0; "
            "print(f\"READ0_MS={el*1000:.1f} LEN={len(b)} BYTE={b[0]}\")'"
        )
        m = must_match(r"READ0_MS=([\d.]+) LEN=(\d+) BYTE=(\d+)", out)
        read0_ms, ln, byte = float(m[1]), int(m[2]), int(m[3])
        print(f"read@0: {read0_ms:.1f} ms, {ln} bytes, byte={byte:#x}")
        assert ln == 4096 and byte == 0xEE
        assert read0_ms < 20, f"read at filled offset slow: {read0_ms:.1f} ms"

    with subtest("read offset 200M: blocks on fill, ~2 s"):
        before = counters()
        off = 200 * 1024 * 1024
        out = machine.succeed(
            "python3 -c '"
            "import os,time; "
            f"fd=os.open(\"{PATH}\", os.O_RDONLY); "
            f"t0=time.perf_counter(); b=os.pread(fd,4096,{off}); "
            "el=time.perf_counter()-t0; "
            "print(f\"READ200_MS={el*1000:.1f} LEN={len(b)}\")'"
        )
        m = must_match(r"READ200_MS=([\d.]+) LEN=(\d+)", out)
        read200_ms, ln = float(m[1]), int(m[2])
        after = counters()
        d_reads = after["read"] - before["read"]
        d_wait_ms = (after["read_wait_us"] - before["read_wait_us"]) / 1000
        print(
            f"read@200M: {read200_ms:.1f} ms wall, {d_wait_ms:.0f} ms FUSE-wait, "
            f"{d_reads} read upcalls"
        )
        assert ln == 4096
        # 200 chunks × 10 ms ≈ 2000 ms; some already elapsed since open.
        assert 800 < read200_ms < 2400, (
            f"read@200M blocked {read200_ms:.0f} ms — expected ~2 s on fill HWM"
        )
        assert d_reads >= 1

    with subtest("await fill complete"):
        machine.wait_until_succeeds(
            f"grep -q hwm={FILE_SIZE} /tmp/counters", timeout=60
        )
        print(f"fill complete: {counters()}")

    with subtest("first full dd (cold pages): bounded upcalls"):
        before = counters()
        machine.succeed(f"dd if={PATH} of=/dev/null bs=1M 2>&1")
        after = counters()
        cold_reads = after["read"] - before["read"]
        cold_wait_ms = (after["read_wait_us"] - before["read_wait_us"]) / 1000
        print(f"first full dd: read+={cold_reads}, wait={cold_wait_ms:.0f} ms")
        # Upper bound: ~FILE_SIZE / (kernel max_read ≈128 KiB) ≈ 2048; readahead
        # may coalesce. Some pages already cached from earlier reads.
        assert 0 < cold_reads <= 2200, f"unexpected cold read upcalls: {cold_reads}"
        # Fill is done — every read should be hwm-hit, no waiting.
        assert cold_wait_ms < 50, f"cold dd waited {cold_wait_ms:.0f} ms post-fill"

    with subtest("WARM full dd: ZERO read upcalls (KEEP_CACHE works mid-stream)"):
        before = counters()
        machine.succeed(f"dd if={PATH} of=/dev/null bs=1M 2>&1")
        after = counters()
        warm_reads = after["read"] - before["read"]
        print(f"warm full dd: read+={warm_reads}")
        assert warm_reads == 0, (
            f"FAIL: warm read produced {warm_reads} upcalls — "
            "FOPEN_KEEP_CACHE set at open() does NOT preserve pages served "
            "during the streaming fill window"
        )

    with subtest("fresh open + dd: KEEP_CACHE survives across opens"):
        # Each `dd` above was already a fresh open; this re-asserts after the
        # cache survived one full warm cycle.
        before = counters()
        machine.succeed(f"dd if={PATH} of=/dev/null bs=1M 2>&1")
        after = counters()
        d_open = after["open"] - before["open"]
        d_read = after["read"] - before["read"]
        d_open_us = (after["open_us"] - before["open_us"])
        print(f"reopen dd: open+={d_open} (took {d_open_us} µs), read+={d_read}")
        assert d_open >= 1
        assert d_open_us < 5000, f"reopen blocked {d_open_us} µs (fill already done)"
        assert d_read == 0, f"reopen read upcalls = {d_read} (KEEP_CACHE not honored)"

    with subtest("mmap MAP_PRIVATE page fault routes through FUSE read"):
        # Use a fresh offset (50 MiB) that no prior read touched, so the
        # page is genuinely uncached. Drop caches first to be certain.
        machine.succeed("sync; echo 1 > /proc/sys/vm/drop_caches")
        before = counters()
        out = machine.succeed(
            "python3 -c '"
            "import mmap,os,time; "
            f"fd=os.open(\"{PATH}\", os.O_RDONLY); "
            f"mm=mmap.mmap(fd,{FILE_SIZE},mmap.MAP_PRIVATE,mmap.PROT_READ); "
            "t0=time.perf_counter(); v=mm[50*1024*1024]; "
            "el=time.perf_counter()-t0; "
            "print(f\"MMAP_MS={el*1000:.2f} VAL={v}\")'"
        )
        after = counters()
        m = must_match(r"MMAP_MS=([\d.]+) VAL=(\d+)", out)
        mmap_ms, val = float(m[1]), int(m[2])
        d_read = after["read"] - before["read"]
        print(f"mmap fault @50M: {mmap_ms:.2f} ms, val={val:#x}, read+={d_read}")
        assert val == 0xEE
        assert d_read >= 1, (
            "mmap page fault did NOT produce a FUSE read upcall — "
            "linker mmap() path would bypass streaming-open entirely"
        )

    print("PASS: streaming-open mechanism works; KEEP_CACHE-from-start gives 0-upcall warm reads")
  '';
}
