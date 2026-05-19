# P0541 spike: composefs-style stack privilege boundary.
#
# Six questions, each a subtest with an explicit PASS/FAIL print:
#   Q1  /dev/fuse fd-handoff (root mounts → unpriv serves, root exits)
#   Q2  full stack (erofs+overlay+fuse) survives privileged mounter exit
#   Q3  unpriv userns inherits mounts; can bind-mount inside its mountns
#   Q4  overlayfs userxattr mode honors data-only-lower redirect unpriv
#   Q5  teardown under load → no D-state hang, clean umount chain
#   Q6  fsopen/fsmount detached-mount fd handoff → unpriv move_mount
#
# Q1/Q2 FAIL = Path-C blocker. Q4 PASS = rio-mountd shrinks (overlay can
# be done by builder). Q6 PASS = cleanest handoff shape.
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
  name = "composefs-spike-priv";
  globalTimeout = 600;

  nodes.machine = _: {
    virtualisation.memorySize = 1024;
    boot.kernelModules = [
      "erofs"
      "overlay"
      "loop"
      "fuse"
    ];
    environment.etc."fuse.conf".text = "user_allow_other\n";
    users.users.riobuild = {
      isNormalUser = true;
      uid = 1000;
    };
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

    verdict = {}

    def record(q, ok, detail=""):
        verdict[q] = ("PASS" if ok else "FAIL", detail)
        print(f"[{q}] {'PASS' if ok else 'FAIL'} {detail}")

    def counters(path="/tmp/counters"):
        time.sleep(0.12)
        out = machine.succeed(f"cat {path}")
        return {k: int(v) for k, v in re.findall(r"(\w+)=(\d+)", out)}

    # ── shared setup: dirs + two EROFS images (trusted.* and user.*) ──
    machine.succeed(
        "mkdir -p /mnt/objects /mnt/meta /mnt/merged "
        "/mnt/objects-q4 /mnt/meta-user /mnt/merged-user /mnt/q6 "
        "/tmp/stage /tmp/stage-user"
    )
    for ns in ("trusted", "user"):
        stage = "/tmp/stage" if ns == "trusted" else "/tmp/stage-user"
        for name, prefix, digest in [
            ("file1", "aa", "${digestAA}"),
            ("file2", "bb", "${digestBB}"),
            ("file3", "cc", "${digestCC}"),
        ]:
            machine.succeed(f"touch {stage}/{name}")
            machine.succeed(
                f"setfattr -n {ns}.overlay.redirect -v /{prefix}/{digest} {stage}/{name}"
            )
            machine.succeed(
                f"setfattr -n {ns}.overlay.metacopy -v 0x00040000 {stage}/{name}"
            )
    machine.succeed("mkfs.erofs -x1 /tmp/meta.erofs /tmp/stage 2>&1")
    machine.succeed("mkfs.erofs -x1 /tmp/meta-user.erofs /tmp/stage-user 2>&1")
    machine.succeed("chown riobuild /tmp")

    # ── Q1: fd-handoff ────────────────────────────────────────────────
    with subtest("Q1: /dev/fuse fd-handoff, root mounts → unpriv serves"):
        machine.succeed(
            "runuser -u riobuild -- bash -c "
            "'${rio-workspace}/bin/spike_digest_fuse --recv-fd /tmp/sock1 /tmp/counters "
            ">/tmp/fuse.log 2>&1 & echo $! >/tmp/fuse.pid'"
        )
        machine.wait_until_succeeds("test -S /tmp/sock1", timeout=10)
        machine.succeed(
            "${rio-workspace}/bin/spike_mountd fuse /mnt/objects /tmp/sock1 "
            ">/tmp/mountd1.log 2>&1"
        )
        machine.wait_until_succeeds("mountpoint -q /mnt/objects", timeout=10)
        out = machine.succeed("head -c1 /mnt/objects/aa/${digestAA} | od -An -tx1").strip()
        srv_uid = machine.succeed(
            "grep -m1 'serving (euid=' /tmp/fuse.log | grep -oE 'euid=[0-9]+'"
        ).strip()
        ok = out == "aa" and srv_uid == "euid=1000"
        record("Q1", ok, f"first-byte={out!r} server-{srv_uid}")
        if not ok:
            print(machine.succeed("cat /tmp/fuse.log /tmp/mountd1.log"))
        assert ok, "Q1 FAILED — Path-C blocker (fd-handoff does not work)"

    machine.succeed("umount /mnt/objects")
    machine.succeed("kill $(cat /tmp/fuse.pid) 2>/dev/null || true; rm -f /tmp/sock1")

    # ── Q2: full stack survives mounter exit ──────────────────────────
    with subtest("Q2: 3-mount stack; privileged mounter exits; data flows"):
        machine.succeed(
            "runuser -u riobuild -- bash -c "
            "'${rio-workspace}/bin/spike_digest_fuse --recv-fd /tmp/sock2 /tmp/counters "
            ">/tmp/fuse2.log 2>&1 & echo $! >/tmp/fuse2.pid'"
        )
        machine.wait_until_succeeds("test -S /tmp/sock2", timeout=10)
        machine.succeed(
            "${rio-workspace}/bin/spike_mountd stack "
            "/mnt/objects /tmp/meta.erofs /mnt/meta /mnt/merged /tmp/sock2 "
            ">/tmp/mountd2.log 2>&1"
        )
        machine.wait_until_succeeds("mountpoint -q /mnt/merged", timeout=10)
        out = machine.succeed(
            "head -c1 /mnt/merged/file1 | od -An -tx1"
        ).strip()
        mounts = machine.succeed("findmnt -rn -o FSTYPE /mnt/merged").strip()
        ok = out == "aa" and mounts == "overlay"
        record("Q2", ok, f"first-byte={out!r} fstype={mounts}")
        if not ok:
            print(machine.succeed("cat /tmp/fuse2.log /tmp/mountd2.log; dmesg | tail -30"))
        assert ok, "Q2 FAILED — Path-C blocker (stack does not survive mounter exit)"

    # ── Q3: build-in-unpriv-userns sees mounts + can bind-mount ───────
    with subtest("Q3: unshare -Urm inherits mounts; bind-mount works"):
        out = machine.succeed(
            "runuser -u riobuild -- unshare -Urm bash -ec '"
            "  head -c1 /mnt/merged/file2 | od -An -tx1; "
            "  mkdir -p /tmp/ns-store; "
            "  mount --bind /mnt/merged /tmp/ns-store; "
            "  head -c1 /tmp/ns-store/file3 | od -An -tx1; "
            "'"
        )
        bytes_seen = re.findall(r"\b([0-9a-f]{2})\b", out)
        ok = bytes_seen[:2] == ["bb", "cc"]
        record("Q3", ok, f"bytes={bytes_seen[:2]}")
        assert ok, f"Q3 FAILED: {out}"

    # ── Q5: teardown — server death → fast-fail, clean umounts ────────
    with subtest("Q5: kill FUSE server → ops fail fast, umount chain clean"):
        machine.succeed("kill -9 $(cat /tmp/fuse2.pid)")
        # New open() through the overlay must fail fast (ENOTCONN), not
        # block. `timeout 5` is the D-state guard: if head wedges, this
        # returns 124 and we fail. file2 is uncached (Q3 read it once,
        # but in a different mountns; even if cached, open() upcalls).
        rc, out = machine.execute(
            "timeout 5 head -c1 /mnt/merged/file2 2>&1; echo rc=$?"
        )
        m = re.search(r"rc=(\d+)", out)
        head_rc = int(m.group(1)) if m else -1
        loopdev = machine.succeed("findmnt -rn -o SOURCE /mnt/meta").strip()
        machine.succeed("umount -l /mnt/merged")
        machine.succeed("umount -l /mnt/objects")
        machine.succeed("umount /mnt/meta")
        machine.succeed(f"losetup -d {loopdev}")
        # head_rc==124 means timeout fired (D-state). Nonzero <124 means
        # it errored fast (expected). 0 would mean it served from cache
        # (also fine — no hang).
        ok = head_rc != 124
        record("Q5", ok, f"post-kill-head-rc={head_rc} out={out.strip()!r}")
        assert ok, "Q5 FAILED — open() hung after FUSE server death"

    # ── Q4: userxattr mode (overlay mount unprivileged) ───────────────
    with subtest("Q4: userxattr + data-only-lower redirect, unpriv overlay"):
        machine.succeed(
            "${rio-workspace}/bin/spike_digest_fuse /mnt/objects-q4 /tmp/counters-q4 "
            ">/tmp/fuse-q4.log 2>&1 & echo $! >/tmp/fuse-q4.pid"
        )
        machine.wait_until_succeeds("mountpoint -q /mnt/objects-q4", timeout=10)
        machine.succeed("mount -t erofs -o loop,ro /tmp/meta-user.erofs /mnt/meta-user")
        # Per overlayfs.rst: with `::` data-only lower, do NOT pass
        # metacopy=on — the data-redirect form is implied; explicit
        # metacopy=on with userxattr is rejected.
        cmd = (
            "runuser -u riobuild -- unshare -Urm bash -ec '"
            "  mount -t overlay overlay /mnt/merged-user "
            "    -o ro,userxattr,lowerdir=/mnt/meta-user::/mnt/objects-q4 2>&1; "
            "  head -c1 /mnt/merged-user/file1 | od -An -tx1; "
            "'"
        )
        rc, out = machine.execute(cmd)
        ok = rc == 0 and re.search(r"\baa\b", out) is not None
        record("Q4", ok, f"rc={rc} out={out.strip()!r}")
        machine.succeed("umount /mnt/meta-user || true")
        machine.succeed("kill $(cat /tmp/fuse-q4.pid) 2>/dev/null || true")
        machine.succeed("umount -l /mnt/objects-q4 2>/dev/null || true")

    # ── Q6: fsopen/fsmount detached-mount fd handoff ──────────────────
    with subtest("Q6: detached mount fd → unpriv move_mount"):
        # Receiver runs in its own userns+mountns. Fully detach stdio so
        # the backdoor shell's completion marker isn't blocked. `timeout 8`
        # bounds accept() so the ls always runs, even if the sender bails.
        machine.succeed(
            "( runuser -u riobuild -- unshare -Urm bash -c '"
            "    mkdir -p /tmp/q6-target; "
            "    timeout 8 ${rio-workspace}/bin/spike_mountd "
            "      recv-mount /tmp/sock6 /tmp/q6-target >/tmp/q6-recv.log 2>&1; "
            "    ls /tmp/q6-target >/tmp/q6-ls.log 2>&1; "
            "  ' "
            ") >/dev/null 2>&1 </dev/null &"
        )
        machine.wait_until_succeeds("test -S /tmp/sock6", timeout=10)
        rc, _ = machine.execute(
            "${rio-workspace}/bin/spike_mountd fsmount-erofs /tmp/meta.erofs /tmp/sock6 "
            ">/tmp/q6-send.log 2>&1"
        )
        machine.wait_until_succeeds("test -f /tmp/q6-ls.log", timeout=12)
        send_log = machine.succeed("cat /tmp/q6-send.log 2>/dev/null || true")
        recv_log = machine.succeed("cat /tmp/q6-recv.log 2>/dev/null || true")
        ls_log = machine.succeed("cat /tmp/q6-ls.log 2>/dev/null || true")
        ok = "move_mount OK" in recv_log and "file1" in ls_log
        record(
            "Q6", ok,
            f"send_rc={rc} send={send_log.strip()!r} "
            f"recv={recv_log.strip()!r} ls={ls_log.strip()!r}"
        )

    # ── Q7-Q12: P0578 — FUSE passthrough under overlay ────────────────
    #
    # ADR-022 §2 castore-FUSE design needs four kernel facts to hold:
    #   Q7  overlay can stack on a FUSE lower with FOPEN_PASSTHROUGH
    #       (FUSE max_stack_depth=1 → overlay at depth 2, the kernel cap)
    #   Q8  BACKING_OPEN ioctl needs CAP_SYS_ADMIN (unpriv → EPERM); a
    #       root-held dup() of the same /dev/fuse fd succeeds — this is
    #       the rio-mountd broker boundary
    #   Q9  reads through the passthrough fd survive kill -9 of the FUSE
    #       server (the kernel holds the backing fd, not the server)
    #   Q10 copy-up through overlay works on a passthrough-backed lower
    #       (chmod / append → upperdir gets the full file)
    #   Q11 cache files are readonly: unpriv O_WRONLY → EACCES
    #   Q12 readlink/getattr come from the FUSE server, but read() on a
    #       regular file never hits it (passthrough bypasses the request
    #       path) — assert the read counter doesn't move

    def setup_passthrough():
        machine.succeed(
            "mkdir -p /tmp/pt-cache /tmp/pt-mnt /tmp/pt-up /tmp/pt-work /tmp/pt-merged"
        )
        # 4 MiB pseudo-random file: enough that a partial read shows up.
        machine.succeed(
            "head -c 4194304 /dev/urandom > /tmp/pt-cache/big.bin && "
            "echo -n 'hello passthrough' > /tmp/pt-cache/small.txt && "
            "chmod 0444 /tmp/pt-cache/* && "
            "sha256sum /tmp/pt-cache/big.bin | cut -d' ' -f1 > /tmp/pt-big.sha"
        )
        machine.succeed(
            "${rio-workspace}/bin/spike_passthrough_fuse "
            "/tmp/pt-mnt /tmp/pt-cache /tmp/pt-probe 1000 "
            ">/tmp/pt-fuse.log 2>&1 & echo $! >/tmp/pt-fuse.pid"
        )
        machine.wait_until_succeeds("test -f /tmp/pt-probe.ready", timeout=15)
        # Direct read through the FUSE mount BEFORE adding overlay — if
        # passthrough breaks, this isolates FUSE from overlay.
        rc, out = machine.execute("head -c 16 /tmp/pt-mnt/small.txt 2>&1")
        if rc != 0:
            print("FUSE direct-read failed:", out.strip())
            print("fuse-log:", machine.succeed("cat /tmp/pt-fuse.log 2>&1").strip())
        assert rc == 0, "FUSE passthrough read failed before overlay"
        machine.succeed(
            "mount -t overlay overlay -o "
            "lowerdir=/tmp/pt-mnt,upperdir=/tmp/pt-up,workdir=/tmp/pt-work,userxattr "
            "/tmp/pt-merged"
        )

    def teardown_passthrough():
        machine.succeed("umount -l /tmp/pt-merged 2>/dev/null || true")
        machine.succeed(
            "kill -9 $(cat /tmp/pt-fuse.pid 2>/dev/null) 2>/dev/null || true"
        )
        machine.succeed("umount -l /tmp/pt-mnt 2>/dev/null || true")

    with subtest("Q7: overlay stacks on FUSE lower with passthrough"):
        setup_passthrough()
        # The mount itself succeeding (in setup_passthrough) IS the depth
        # check: overlay refuses lowers at >= FILESYSTEM_MAX_STACK_DEPTH.
        # Verify content reaches us through the full stack.
        merged_sha = machine.succeed(
            "sha256sum /tmp/pt-merged/big.bin | cut -d' ' -f1"
        ).strip()
        cache_sha = machine.succeed("cat /tmp/pt-big.sha").strip()
        small = machine.succeed("cat /tmp/pt-merged/small.txt").strip()
        ok = merged_sha == cache_sha and small == "hello passthrough"
        record("Q7", ok, f"merged_sha={merged_sha[:12]} cache_sha={cache_sha[:12]}")

    with subtest("Q8: BACKING_OPEN needs CAP_SYS_ADMIN; dup'd fd works"):
        probe = machine.succeed("cat /tmp/pt-probe")
        root_ok = "root_backing_open=ok" in probe
        unpriv_eperm = "unpriv_backing_open=EPERM" in probe
        record("Q8", root_ok and unpriv_eperm, probe.strip().replace("\n", " | "))

    with subtest("Q12: read() never reaches the FUSE server"):
        # Issue a fresh read that hasn't been page-cached yet.
        machine.succeed("echo 1 > /proc/sys/vm/drop_caches")
        machine.succeed("sha256sum /tmp/pt-merged/big.bin >/dev/null")
        fuse_log = machine.succeed("cat /tmp/pt-fuse.log")
        ok = "UNEXPECTED read upcall" not in fuse_log
        record("Q12", ok, "no read upcalls" if ok else "passthrough not engaged")

    with subtest("Q10: copy-up through overlay over passthrough lower"):
        # chmod triggers a full data copy-up (no metacopy with userxattr);
        # append re-opens the now-upper file. The copy-up reads the lower
        # through the passthrough fd to populate the upper. Issues several
        # opens of the lower in the same syscall — the FUSE server MUST
        # share one BackingId per ino across them or the kernel's
        # `fi->fb != fb` check returns EBUSY → user-visible EIO.
        rc, out = machine.execute("chmod 0644 /tmp/pt-merged/small.txt 2>&1")
        if rc != 0:
            print("Q10 chmod failed:", out.strip())
            print("Q10 dmesg-tail:", machine.succeed("dmesg | tail -10"))
        machine.succeed("echo -n ' + appended' >> /tmp/pt-merged/small.txt")
        upper = machine.succeed("cat /tmp/pt-up/small.txt")
        merged = machine.succeed("cat /tmp/pt-merged/small.txt")
        ok = rc == 0 and upper == "hello passthrough + appended" and merged == upper
        record("Q10", ok, f"chmod_rc={rc} upper={upper!r}")

    with subtest("Q11: cache files readonly to unpriv writers"):
        rc, out = machine.execute(
            "runuser -u riobuild -- bash -c "
            "'echo x > /tmp/pt-cache/big.bin' 2>&1"
        )
        ok = rc != 0 and ("Permission denied" in out or "permission denied" in out.lower())
        record("Q11", ok, f"rc={rc} out={out.strip()!r}")

    with subtest("Q9: reads survive FUSE server kill"):
        # Hold an fd open through the overlay, kill the FUSE server, then
        # read through the held fd. Passthrough installs the backing ext4
        # fd as the file's read path — server death is invisible to reads.
        # `exec 9<` opens (and FOPEN_PASSTHROUGH-replies) eagerly, so by
        # the time the kill fires, no FUSE request is pending.
        machine.succeed(
            "exec 9</tmp/pt-merged/big.bin && "
            "kill -9 $(cat /tmp/pt-fuse.pid) && sleep 0.5 && "
            "actual=$(cat <&9 | sha256sum | cut -d' ' -f1) && "
            "exec 9<&- && "
            "[ \"$actual\" = \"$(cat /tmp/pt-big.sha)\" ] && echo MATCH > /tmp/pt-q9 || echo MISMATCH > /tmp/pt-q9"
        )
        q9 = machine.succeed("cat /tmp/pt-q9").strip()
        record("Q9", q9 == "MATCH", q9)
        teardown_passthrough()

    print("\n──────── verdict ────────")
    for q in sorted(verdict):
        v, d = verdict[q]
        print(f"{q}: {v}  {d}")
    # Q4/Q6 are exploratory (may FAIL on some kernels without blocking the
    # design); Q1/Q2/Q3/Q5 hard-assert above. Q7-Q12 are P0578 gate
    # criteria for the §2 castore-FUSE design — any FAIL is a blocker.
    p0578_failed = [
        q for q, (v, _) in verdict.items()
        if v == "FAIL" and q in ("Q7", "Q8", "Q9", "Q10", "Q11", "Q12")
    ]
    assert not p0578_failed, f"P0578 gate failed: {p0578_failed}"
  '';
}
