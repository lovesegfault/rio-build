# P0567: the production rio-mountd daemon end-to-end.
#
# Boots the real `rio-mountd` binary against an XFS-with-prjquota
# staging loopback and drives it over the SOCK_SEQPACKET protocol with
# `spike_mountd_client` (the builder-side stand-in until P0559's
# in-process client exists). Carries the P0578 mountd-protocol subtests
# (v-vii, x-xv) that were deferred to P0567:
#
#   fd-handoff        Mount → fd over SCM_RIGHTS → unprivileged client serves FUSE
#   teardown          conn close → umount + staging removed + ids released
#   gid-gate          socket perms (0660 root:rio-builder) reject non-group connect
#   traversal-reject  Mount{"../escape"} → BadBuildId, no fs trace
#   one-mount         second Mount on one conn → AlreadyMounted
#   uid-bound         second conn from a live uid → dropped
#   build-id-unique   Mount{X} from uid B while uid A owns X → DuplicateBuildId
#   promote-verified  good content published 0444 exact bytes; corrupt rejected
#   bounded-copy      concurrent appender cannot grow the published entry
#   backing-broker    BackingOpen returns a usable backing_id
#   concurrency       BackingOpen replies arrive during a Promote copy
#   staging-quota     writes stop at the kernel project quota
#   orphan-scan       daemon restart reaps leftover mounts/staging/placeholders
#
# Perf numbers (BackingOpen RTT, Promote throughput — P0578 vi/vii/x)
# are printed as `PERF` lines for humans reading the CI log but NOT
# gated: this test runs under TCG on runners without /dev/kvm, where
# the production targets (p99 < 200 µs, ≥ 1 GiB/s) are off by 10-100×
# and a wall-clock gate would be permanently red (see
# .claude/rules/ci-failure-patterns.md "Wall-clock gate under load").
{
  pkgs,
  common,
}:
let
  # The client is codecov-ignored (bin/spike_*) and exercises nothing
  # the daemon side doesn't, so its profile goes to /tmp instead of the
  # collected cov dir — the env var only exists to stop the
  # instrumented binary from spraying "cannot write default.profraw"
  # warnings into output the subtests grep. `env` rather than a shell
  # assignment-prefix because `runuser --` execs without a shell.
  clientCov = pkgs.lib.optionalString common.coverage "env LLVM_PROFILE_FILE=/tmp/client-%m.profraw ";
in
pkgs.testers.runNixOSTest {
  name = "mountd";
  globalTimeout = 900 + common.covTimeoutHeadroom;

  nodes.machine = _: {
    virtualisation.memorySize = 2048;
    # 1 GiB staging XFS image + ~110 MiB of generated content copied
    # into the cache + the rio-workspace closure.
    virtualisation.diskSize = 4096;
    boot.kernelModules = [
      "fuse"
      "loop"
    ];
    boot.supportedFilesystems = [ "xfs" ];
    # The builder uids get rio-builder (gid 990) as their primary
    # group, mirroring the runAsGroup the controller stamps on executor
    # pods; group membership is what lets connect(2) pass the socket's
    # 0660 root:rio-builder permission check. `outsider` is not in
    # rio-builder: it proves that DAC check rejects everyone else.
    users = {
      groups.rio-builder.gid = 990;
      users = {
        build1 = {
          isNormalUser = true;
          uid = 1000;
          group = "rio-builder";
        };
        build2 = {
          isNormalUser = true;
          uid = 2000;
          group = "rio-builder";
        };
        outsider = {
          isNormalUser = true;
          uid = 3000;
          group = "users";
        };
      };
    };
    environment.systemPackages = [
      common.rio-workspace
      pkgs.xfsprogs
      pkgs.util-linux
      pkgs.b3sum
      pkgs.curl
    ];
  };

  testScript = ''
    import re

    machine.start()
    machine.wait_for_unit("multi-user.target")

    # ── Setup: dirs + XFS staging loopback with project quotas ─────────
    machine.succeed("mkdir -p /var/rio/castore /var/rio/staging /var/rio/cache /var/rio/chunks")
    # Profraw drop dir for coverage mode (collectCoverage tars it). No
    # systemd unit means no covTmpfiles rule, so create it here.
    machine.succeed("mkdir -p /var/lib/rio/cov")
    machine.succeed("truncate -s 1G /var/rio-staging.img && mkfs.xfs -q /var/rio-staging.img")
    machine.succeed("mount -o loop,prjquota /var/rio-staging.img /var/rio/staging")

    def start_mountd(quota_bytes):
        # Direct exec rather than a systemd unit so the staging quota can
        # differ between the crash/quota phase and the main phase. The
        # stale socket from a killed instance is removed first so the
        # readiness wait below cannot pass against the old inode.
        # covShellEnv sets LLVM_PROFILE_FILE in coverage mode (empty
        # otherwise); %p keeps the per-instance profraws distinct.
        machine.succeed("rm -f /run/rio-mountd.sock")
        machine.succeed(
            "${common.covShellEnv}"
            "${common.rio-workspace}/bin/rio-mountd --socket /run/rio-mountd.sock"
            " --castore-dir /var/rio/castore --staging-dir /var/rio/staging"
            " --cache-dir /var/rio/cache --chunks-dir /var/rio/chunks"
            f" --staging-quota-bytes {quota_bytes} --allowed-gid 990"
            " --metrics-addr 127.0.0.1:9095"
            " >>/var/log/rio-mountd.log 2>&1 & echo $! > /run/rio-mountd.pid"
        )
        machine.wait_until_succeeds("test -S /run/rio-mountd.sock", timeout=15)

    def stop_mountd():
        # SIGTERM, not SIGKILL: the daemon returns from main so atexit
        # handlers run (LLVM profraw flush in coverage mode). Wait for
        # the process to exit so the next instance can bind the socket
        # and the metrics port.
        machine.succeed("kill $(cat /run/rio-mountd.pid)")
        machine.wait_until_succeeds(
            "! kill -0 $(cat /run/rio-mountd.pid) 2>/dev/null", timeout=15
        )

    CLIENT = "${clientCov}${common.rio-workspace}/bin/spike_mountd_client --socket /run/rio-mountd.sock"

    def client(user, args):
        return f"runuser -u {user} -- {CLIENT} {args}"

    def serve(user, build_id, tag):
        # Background a `serve` client and wait for its ready file: the
        # FUSE handshake is complete and the mountpoint is safe to touch.
        machine.succeed(
            f"runuser -u {user} -- bash -c "
            f"'{CLIENT} serve --build-id {build_id} --ready-file /tmp/{tag}.ready "
            f">/tmp/{tag}.log 2>&1 & echo $! > /tmp/{tag}.pid'"
        )
        machine.wait_until_succeeds(f"test -f /tmp/{tag}.ready", timeout=30)

    def wait_idle():
        # Every registered connection has been torn down (uids and
        # build_ids released). The gauge is absent before the first
        # connection and reads 0 after the last teardown; rejected
        # connections never increment it.
        machine.wait_until_succeeds(
            "c=$(curl -sf 127.0.0.1:9095/metrics"
            " | awk '/^rio_mountd_connections_current/ {print $2}');"
            ' [ -z "$c" ] || [ "$c" = "0" ]',
            timeout=30,
        )

    def result(out, key):
        # Extract `key=value` from a client RESULT/PERF line.
        m = re.search(rf"{key}=(\S+)", out)
        assert m, f"no {key}= in client output:\n{out}"
        return m.group(1)

    # ═══ Phase 1: crash recovery + staging quota (8 MiB instances) ═════
    # Runs first so the instance that gets SIGKILLed (the crash
    # simulation) is a short-lived one. The instance serving the whole
    # protocol phase below exits last via SIGTERM and so keeps its
    # coverage profraw.

    # ── Daemon crash: orphan reaping on the next incarnation ───────────
    with subtest("orphan-scan: restart reaps mounts, staging, placeholders"):
        start_mountd(8 * 1024 * 1024)
        serve("build1", "b-orphan", "orphan")
        # Kill the daemon first so it cannot tear down, then the client.
        machine.succeed("kill -9 $(cat /run/rio-mountd.pid)")
        machine.succeed("kill -9 $(cat /tmp/orphan.pid) || true")
        machine.succeed("test -d /var/rio/staging/b-orphan")
        machine.succeed("mkdir -p /var/rio/cache/zz && touch /var/rio/cache/zz/0000.promoting")
        start_mountd(8 * 1024 * 1024)
        machine.succeed(
            "test ! -e /var/rio/castore/b-orphan"
            " && test ! -e /var/rio/staging/b-orphan"
            " && test ! -e /var/rio/cache/zz/0000.promoting"
        )

    with subtest("staging-quota: kernel stops writes at the project quota"):
        out = machine.succeed(
            client("build1", "fill-staging --build-id b-fill --staging-root /var/rio/staging --give-up-mib 64")
        )
        print(out)
        written = int(result(out, "written"))
        # The quota is 8 MiB; XFS accounts in filesystem blocks so allow
        # one block-reservation of slack, but 2x the quota means the
        # limit is not being enforced.
        assert written <= 16 * 1024 * 1024, f"wrote {written} bytes past an 8 MiB quota"
        wait_idle()

    stop_mountd()

    # ═══ Phase 2: protocol + broker (one 256 MiB instance) ═════════════
    start_mountd(256 * 1024 * 1024)

    # ── fd-handoff: Mount → SCM_RIGHTS → unprivileged FUSE server ──────
    with subtest("fd-handoff: builder serves FUSE on the handed-off fd"):
        serve("build1", "b-alpha", "alpha")
        fstype = machine.succeed("findmnt -rn -o FSTYPE /var/rio/castore/b-alpha").strip()
        assert fstype == "fuse.rio-castore", f"fstype={fstype}"
        # The mount works for the build uid and (allow_other) for root.
        machine.succeed("runuser -u build1 -- ls /var/rio/castore/b-alpha")
        machine.succeed("ls /var/rio/castore/b-alpha")
        # Staging dir: 0700, owned by the connection's peer uid.
        st = machine.succeed("stat -c '%a %U' /var/rio/staging/b-alpha").strip()
        assert st == "700 build1", f"staging dir is {st}"
        ready = machine.succeed("cat /tmp/alpha.ready")
        assert "quota=268435456" in ready, f"ready file: {ready!r}"

    # ── teardown: conn close undoes everything Mount set up ────────────
    with subtest("teardown: close → umount, staging removed, ids released"):
        machine.succeed("kill $(cat /tmp/alpha.pid)")
        machine.wait_until_succeeds(
            "! mountpoint -q /var/rio/castore/b-alpha"
            " && test ! -e /var/rio/castore/b-alpha"
            " && test ! -e /var/rio/staging/b-alpha",
            timeout=30,
        )
        wait_idle()
        # Same uid, same build_id, immediately reusable — and a second
        # Mount on one connection is AlreadyMounted (one-mount).
        machine.succeed(client("build1", "double-mount --build-id b-alpha"))
        machine.succeed("test ! -e /var/rio/castore/b-alpha-second")
        wait_idle()

    # ── gid gate ───────────────────────────────────────────────────────
    with subtest("gid-gate: socket DAC rejects a non-group connect"):
        # outsider is not in rio-builder: connect() itself fails on the
        # 0660 root:rio-builder socket inode. This file-permission check
        # is mountd's only access control — there is no peer-credential
        # check after connect.
        rc, out = machine.execute(client("outsider", "expect-rejected") + " 2>&1")
        assert rc != 0 and "ermission denied" in out, f"rc={rc} out={out!r}"

    # ── build_id validation ────────────────────────────────────────────
    # The full rejection matrix lives in the build_id_validation unit
    # test; this proves one rejection arrives as a typed error over the
    # wire and leaves no filesystem trace.
    with subtest("traversal-reject: non-component build_ids are refused"):
        machine.succeed(
            client("build1", "expect-mount-err --build-id '../escape' --expect BadBuildId")
        )
        machine.succeed("test ! -e /var/rio/escape")
        wait_idle()

    # ── uid-bound + build-id-unique (one holder proves both) ───────────
    with subtest("uid-bound + build-id-unique"):
        serve("build1", "shared", "shared")
        # Second connection from the same uid: dropped without a reply.
        machine.succeed(client("build1", "expect-rejected"))
        # Different uid, same build_id: typed rejection.
        machine.succeed(
            client("build2", "expect-mount-err --build-id shared --expect DuplicateBuildId")
        )
        # The holder's staging dir is untouched by the failed claim.
        st = machine.succeed("stat -c '%a %U' /var/rio/staging/shared").strip()
        assert st == "700 build1", f"holder staging dir is {st}"
        machine.succeed("kill $(cat /tmp/shared.pid)")
        machine.wait_until_succeeds("test ! -e /var/rio/castore/shared", timeout=30)
        wait_idle()

    # ── Promote: integrity boundary for the shared cache ───────────────
    with subtest("promote-verified: good content published 0444, exact bytes"):
        out = machine.succeed(
            client("build1", "promote --build-id b-promo --staging-root /var/rio/staging --size-mib 8")
        )
        print(out)
        digest = result(out, "digest")
        published = f"/var/rio/cache/{digest[:2]}/{digest}"
        st = machine.succeed(f"stat -c '%a %U' {published}").strip()
        assert st == "444 root", f"published entry is {st}"
        machine.succeed(f'[ "$(b3sum --no-names {published})" = "{digest}" ]')
        # Reused as the BackingOpen target below: a real published cache
        # entry opened the way the castore-FUSE will open it.
        backing_file = published
        wait_idle()

    with subtest("promote-verified: mismatched content rejected, cache untouched"):
        out = machine.succeed(
            client(
                "build1",
                "promote --build-id b-corrupt --staging-root /var/rio/staging --size-mib 4 --corrupt",
            )
        )
        print(out)
        assert result(out, "kind") == "DigestMismatch"
        claimed = result(out, "digest")
        machine.succeed(f"test ! -e /var/rio/cache/{claimed[:2]}/{claimed}")
        machine.succeed(f"test ! -e /var/rio/cache/{claimed[:2]}/{claimed}.promoting")
        wait_idle()

    with subtest("promote-bounded-copy: concurrent appender cannot grow the copy"):
        out = machine.succeed(
            client(
                "build1",
                "append-promote --build-id b-append --staging-root /var/rio/staging --size-mib 32",
            )
        )
        print(out)
        digest = result(out, "digest")
        published = f"/var/rio/cache/{digest[:2]}/{digest}"
        if "append_promote=ok" in out:
            # Published exactly the hashed bytes, regardless of how much
            # the appender added afterwards.
            size = machine.succeed(f"stat -c %s {published}").strip()
            assert size == str(32 * 1024 * 1024), f"published {size} bytes"
            machine.succeed(f'[ "$(b3sum --no-names {published})" = "{digest}" ]')
        else:
            # Rejected: nothing visible in the cache.
            machine.succeed(f"test ! -e {published}")
        wait_idle()

    # ── BackingOpen broker ─────────────────────────────────────────────
    with subtest("backing-broker: BackingOpen against the kept /dev/fuse dup"):
        # The backing file is a real published cache entry, opened
        # read-only by the build uid the way the castore-FUSE will.
        out = machine.succeed(
            client(
                "build2",
                f"backing-bench --build-id b-backing --backing-file {backing_file} --iters 2000",
            )
        )
        print(out)
        wait_idle()

    with subtest("concurrency: Promote does not serialize ahead of BackingOpen"):
        out = machine.succeed(
            client(
                "build1",
                "concurrency --build-id b-conc --staging-root /var/rio/staging"
                f" --backing-file {backing_file} --promote-mib 64 --iters 100",
            )
        )
        print(out)
        # Structural, not wall-clock: at least one BackingOpen reply
        # arrived before the Promote reply, so the copy loop ran on the
        # blocking pool instead of blocking the connection task. (In
        # practice all 100 land first; >=1 is the timing-independent
        # floor that distinguishes concurrent from serialized.)
        before = int(result(out, "backing_before_promote"))
        assert before >= 1, f"all backing replies arrived after the promote ({before})"
        wait_idle()

    # ── Metrics exporter sanity ────────────────────────────────────────
    # metrics-rs only renders a series after its first emission, so this
    # must run on the instance that served the promote subtests above.
    with subtest("metrics: request histogram and promote counters exported"):
        metrics = machine.succeed("curl -sf 127.0.0.1:9095/metrics")
        for name in [
            "rio_mountd_request_seconds",
            "rio_mountd_promote_bytes_total",
            "rio_mountd_promote_reject_total",
            "rio_mountd_connections_current",
        ]:
            assert name in metrics, f"{name} missing from /metrics"

    # collectCoverage below only stops systemd-managed rio services;
    # the raw-exec'd daemon needs an explicit SIGTERM to flush its
    # profraws before the cov dir is tarred.
    stop_mountd()
    ${common.collectCoverage "machine"}
  '';
}
