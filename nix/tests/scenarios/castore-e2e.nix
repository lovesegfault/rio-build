# P0560 cutover gate: real scheduler-dispatched builds whose inputs are
# served by the per-build castore-FUSE lower (rio-mountd fd handoff →
# Directory-DAG prefetch → overlay lowerdir), on a worker running the
# reworked NixOS module (rio-mountd as a host systemd service).
#
# Subtests (each guards a real failure mode):
#   mountd-running             broker socket + /var/rio layout up before any build
#   cold-build                 inputs materialize ONLY through the castore stack
#   streaming-large-input      >threshold input filled the node chunk cache intact
#   warm-build                 dep closure reused from the node cache, no re-promotes
#   store-outage-infra-retry   store unreachable → drv waits, never poisoned
#   mountd-restart             broker restart: cache survives, next build succeeds
#   teardown-clean             no leftover mounts, staging residue, or connections
#
# The worker runs with RIO_STREAM_THRESHOLD=65536 (default.nix fixture)
# so the 300 KiB dep exercises the streaming-open path without needing
# a multi-MiB input under TCG.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) gatewayHost;
  drvs = import ../lib/derivations.nix { inherit pkgs; };
in
pkgs.testers.runNixOSTest {
  name = "rio-castore-e2e";
  skipTypeCheck = true;
  # ~6 tiny builds + one deliberately-failed dispatch + re-dispatch +
  # mountd restart; generous for TCG runners.
  globalTimeout = 900 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.mkBootstrap {
      inherit fixture gatewayHost;
      withSeed = true;
    }}

    ${common.mkBuildHelperV2 {
      inherit gatewayHost;
      dumpLogsExpr = "dump_all_logs([${gatewayHost}, worker])";
    }}

    MOUNTD_PORT = 9095
    BIGDEP_BYTES = "307200"

    def cache_files():
        return int(worker.succeed("find /var/rio/cache -type f | wc -l").strip())

    def chunk_files():
        return int(worker.succeed("find /var/rio/chunks -type f | wc -l").strip())

    def promote_bytes():
        m = scrape_metrics(worker, MOUNTD_PORT)
        return metric_value(m, "rio_mountd_promote_bytes_total") or 0.0

    def store_connect_retries():
        # The builder's startup gRPC connect loop logs this on every
        # failed attempt; during the outage subtest the delta proves the
        # worker actually observed the blocked store route. journald
        # survives the one-shot restarts.
        rc, out = worker.execute(
            "journalctl -u rio-builder --no-pager | grep -c 'connect failed; retrying'"
        )
        return int(out.strip() or "0")

    def check_summary(out_path, name):
        # The consumer wrote (1) the small dep's marker, (2) the byte
        # count of the large dep as read THROUGH the castore lower,
        # (3) its own name. Wrong/missing values mean the lower served
        # wrong bytes — the one failure mode no metric would catch.
        summary = client.succeed(
            f"nix store cat --store 'ssh-ng://${gatewayHost}' {out_path}/summary"
        )
        for needle in ("castore-dep-marker", BIGDEP_BYTES, name):
            assert needle in summary, (
                f"{name}: expected {needle!r} in build output summary; got:\n{summary}"
            )

    # ══════════════════════════════════════════════════════════════════
    with subtest("mountd-running: broker + /var/rio layout present on the worker"):
        worker.succeed("test -S /run/rio-mountd.sock")
        worker.succeed(
            "test -d /var/rio/castore -a -d /var/rio/staging"
            " -a -d /var/rio/cache -a -d /var/rio/chunks"
        )
        worker.wait_until_succeeds(
            f"curl -sf http://localhost:{MOUNTD_PORT}/metrics >/dev/null", timeout=15
        )

    # ══════════════════════════════════════════════════════════════════
    with subtest("cold-build: inputs materialize through the castore stack"):
        out1 = build("${drvs.castoreE2e}", attr="consumer1", capture_stderr=False).strip()
        assert out1.startswith("/nix/store/"), f"unexpected output: {out1!r}"
        check_summary(out1, "rio-castore-consumer1")
        # dep + bigdep + consumer1 all executed on this worker.
        n = journal_builds_succeeded(worker)
        assert n >= 3, f"expected >=3 successful builds in the worker journal, got {n}"
        # The inputs were fetched via castore open() and promoted into
        # the mountd-owned node cache — there is no other path that
        # populates /var/rio/cache.
        assert cache_files() > 0, "no entries in /var/rio/cache after the cold build"
        pb = promote_bytes()
        assert pb > 0, f"rio_mountd_promote_bytes_total = {pb} after the cold build"

    # ══════════════════════════════════════════════════════════════════
    with subtest("streaming-large-input: >threshold input filled the chunk cache"):
        # Only the streaming-open path (size > RIO_STREAM_THRESHOLD)
        # touches /var/rio/chunks; the whole-file path never does. Zero
        # entries here means the threshold dispatch regressed to
        # whole-file fetches. Content integrity is the BIGDEP_BYTES
        # check in check_summary above.
        nchunks = chunk_files()
        assert nchunks > 0, (
            "no entries in /var/rio/chunks — the streaming-open path did not run "
            "for the 300 KiB input (threshold not honored?)"
        )

    # ══════════════════════════════════════════════════════════════════
    with subtest("warm-build: shared node cache reused, no new promotes or chunk fetches"):
        cache_before = cache_files()
        chunks_before = chunk_files()
        promote_before = promote_bytes()

        out2 = build("${drvs.castoreE2e}", attr="consumer2", capture_stderr=False).strip()
        check_summary(out2, "rio-castore-consumer2")

        # The warm build's only genuinely-new input is consumer2's own
        # .drv — a ~1.6 KiB text file nix-daemon reads through the
        # castore lower, so its first open promotes it into the node
        # cache. The dep closure (busybox + dep marker + 300 KiB bigdep)
        # must NOT be re-fetched: a re-fetch would re-promote at least
        # the whole-file members (busybox alone is >1 MiB), so cap the
        # promote delta at the worker's 64 KiB stream threshold —
        # generous for a .drv, far below any dep-closure member.
        promote_delta = promote_bytes() - promote_before
        assert promote_delta < 65536, (
            f"warm build re-promoted {promote_delta} bytes "
            f"({promote_before} -> {promote_bytes()}) — the dep closure was "
            "re-fetched instead of reusing the shared node cache"
        )
        # Exactly one new backing-cache entry: consumer2's .drv. The dep
        # closure is content-addressed, so a re-fetch would not add
        # entries (promote_delta above catches that); anything >1 here
        # means the warm build pulled content the cold build never saw.
        cache_delta = cache_files() - cache_before
        assert cache_delta == 1, (
            f"warm build added {cache_delta} backing-cache entries "
            f"({cache_before} -> {cache_files()}); expected exactly one (its own .drv)"
        )
        # Only >threshold content fills /var/rio/chunks, and the warm
        # build introduces none — any growth means the streaming path
        # re-fetched bigdep's chunks instead of hitting the node cache.
        assert chunk_files() == chunks_before, (
            f"warm build grew the chunk cache ({chunks_before} -> {chunk_files()} entries)"
        )

    # ══════════════════════════════════════════════════════════════════
    with subtest("store-outage-infra-retry: fetch failure re-queues, never poisons"):
        # Block ONLY the worker's route to rio-store (control:9002) —
        # the control plane keeps working, so the client can submit and
        # the scheduler accepts the DAG. The builder is one-shot and
        # restarts between builds: with the store unreachable, the fresh
        # instance wedges in its startup store-connect retry loop (the
        # pod-not-Ready analogue) instead of registering, so the drv
        # must WAIT — neither complete, nor fail permanently, nor be
        # poisoned — until the route returns. A misclassification on
        # any attempt that does run would poison the derivation and the
        # client build below would never succeed.
        for ipt in ("iptables", "ip6tables"):
            worker.succeed(f"{ipt} -I OUTPUT -p tcp --dport 9002 -j REJECT")
        retries_before = store_connect_retries()

        client.succeed("rm -f /tmp/outage.rc /tmp/outage.log")
        # Same background shape as castore-fuse.nix's serve helper: the
        # subshell's fds are redirected away from the test driver so the
        # call returns immediately while the build keeps running.
        client.succeed(
            "( nix-build --no-out-link --store 'ssh-ng://${gatewayHost}' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "${drvs.castoreE2e} -A consumer3 "
            ">/tmp/outage.log 2>&1; echo $? >/tmp/outage.rc ) >/dev/null 2>&1 &"
        )

        # The worker must actually observe the outage (otherwise this
        # subtest silently degrades into a plain warm build): the
        # restarted builder logs store connect retries while the route
        # is blocked.
        worker.wait_until_succeeds(
            "[ \"$(journalctl -u rio-builder --no-pager"
            f" | grep -c 'connect failed; retrying')\" -gt {retries_before} ]",
            timeout=180,
        )
        # And the consumer cannot have completed without its inputs'
        # store route — the submitted build is being held back.
        client.fail("test -f /tmp/outage.rc")

        # Restore the route: the builder's next retry succeeds, it
        # registers, the scheduler dispatches, and the SAME client
        # invocation completes.
        for ipt in ("iptables", "ip6tables"):
            worker.succeed(f"{ipt} -D OUTPUT -p tcp --dport 9002 -j REJECT")

        client.wait_until_succeeds("test -f /tmp/outage.rc", timeout=300)
        rc = client.succeed("cat /tmp/outage.rc").strip()
        if rc != "0":
            print(client.succeed("cat /tmp/outage.log"))
            dump_all_logs([${gatewayHost}, worker])
        assert rc == "0", (
            "build submitted during the store outage never recovered "
            f"(rc={rc}) — store unavailability during input materialization must "
            "be absorbed as an infrastructure condition, not poison the derivation"
        )
        out3 = client.succeed(
            "grep -o '/nix/store/[^ ]*rio-castore-consumer3[^ ]*' /tmp/outage.log | tail -n1"
        ).strip()
        check_summary(out3, "rio-castore-consumer3")

    # ══════════════════════════════════════════════════════════════════
    with subtest("mountd-restart: broker restart between builds, cache survives"):
        cache_before = cache_files()
        worker.succeed("systemctl restart rio-mountd.service")
        worker.wait_for_unit("rio-mountd.service")
        worker.wait_until_succeeds("test -S /run/rio-mountd.sock", timeout=15)
        # The shared node cache is mountd-owned state that must survive
        # a broker restart (a DS rollout must not re-cool every node).
        assert cache_files() == cache_before, (
            f"mountd restart changed the backing cache ({cache_before} -> {cache_files()})"
        )
        out4 = build("${drvs.castoreE2e}", attr="consumer4", capture_stderr=False).strip()
        check_summary(out4, "rio-castore-consumer4")

    # ══════════════════════════════════════════════════════════════════
    with subtest("teardown-clean: no leftover per-build mounts, staging, or connections"):
        # Per-build teardown (overlay down → session drop → mountd
        # conn-close reap) must leave nothing behind once the builds
        # are done; leaks here are stale FUSE mounts + unbounded disk
        # on long-lived nodes.
        worker.wait_until_succeeds(
            '[ -z "$(ls -A /var/rio/castore)" ] && [ -z "$(ls -A /var/rio/staging)" ]',
            timeout=60,
        )
        worker.succeed("! findmnt -rn -t fuse.rio-castore")
        # The gauge MUST be present and 0 — consumer4's connection (after
        # the mountd restart) registered it, so an absent series here
        # means mountd never saw a connection and the check would be
        # tautological, not that teardown is clean.
        worker.wait_until_succeeds(
            f"c=$(curl -sf localhost:{MOUNTD_PORT}/metrics"
            " | awk '/^rio_mountd_connections_current/ {print $2}');"
            ' [ "$c" = "0" ]',
            timeout=30,
        )

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
