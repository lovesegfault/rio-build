# ADR-022 §2: the production castore-FUSE session against a real rio-store.
#
# The client VM doubles as the builder/mountd machine: it runs the real
# `rio-mountd` daemon and drives the production
# `castore_fuse::session::mount_and_serve` assembly through the
# `serve-castore` subcommand of `spike_mountd_client` — mountd fd
# handoff, Directory-DAG prefetch over the tenant-scoped
# DirectoryService, and the open() data path (whole-file ReadBlob,
# StatBlob+GetChunks streaming, shared-cache passthrough hits) against
# content that was ingested through the gateway (busybox seed + three
# in-VM builds).
#
# Subtest map (each one guards a real failure mode):
#   mount             mounts + prefetches + ready file; one root entry per --store-path
#   metadata          readdir/lookup/stat through FUSE match the original store path
#   small-read        ≤ threshold miss: whole-file ReadBlob, byte-identical, promoted 0444 root
#   streaming-read    > threshold miss: StatBlob+GetChunks fill, byte-identical, chunk cache fills
#   readlink          symlink targets through FUSE == the targets symlinkDrv wrote
#   negative-lookup   ENOENT outside the closure; the mount keeps answering
#   teardown-b1       SIGTERM → exit 0; mountd detaches the mount and reaps staging
#   missing-path      never-uploaded path fails fast: non-zero exit, no mount, no ready file
#   cache-hit         second build-id reads from the shared cache: no re-fetch, passthrough hits
#   teardown-final    second session and rio-mountd exit cleanly (SIGTERM, never -9)
#
# Auth: DirectoryService/ChunkService are tenant-scoped. The testScript
# creates a tenant row, inserts path_tenants rows (sha256 of the full
# store-path string) for every mounted path, and mints an HMAC
# assignment token (claims.tenant = that tenant) signed with the
# fixture's hmac.key — the same header the production builder sends.
# That explicit 'castore-vm' tenant only authorizes the serve-castore
# sessions; the in-VM seed builds dispatched to the worker ride the
# fixture's defaultTenant stopgap (see the wiring in default.nix).
{
  pkgs,
  common,
}:
let
  inherit (pkgs) lib;
  drvs = import ../lib/derivations.nix { inherit pkgs; };

  # Single small regular file at the NAR root (content "castore-small\n",
  # 14 bytes ≤ stream-threshold) — the whole-file ReadBlob → Promote
  # path, and the file-rooted RootNode resolution.
  smallDrv = drvs.mkTrivial { marker = "castore-small"; };

  # Directory with symlinks at known relative targets ("tool" inside
  # bin/, "bin/tool" at the NAR root). The readlink subtest compares the
  # FUSE-served targets against these constants, so it neither depends
  # on incidental closure layout (which applet symlinks busybox happens
  # to ship) nor needs a `find | head` pipe — which is racy under the
  # test driver's pipefail: head exits after the first line and find
  # gets EPIPE on the rest.
  symlinkDrv = drvs.mkCustom {
    name = "rio-test-castore-symlink";
    script = ''
      ''${busybox}/bin/busybox mkdir -p $out/bin
      echo castore-symlink-target > $out/bin/tool
      ''${busybox}/bin/busybox ln -s tool $out/bin/tool-link
      ''${busybox}/bin/busybox ln -s bin/tool $out/top-link
    '';
  };

  # serve-castore runs the rio-builder castore_fuse library code in this
  # client process — that is the whole point of the scenario's coverage
  # wiring. Drop its profraws where collectCoverage tars them. `env`
  # form so it works inside the `bash -c` wrapper unchanged. %m merges
  # the three invocations (b1, b2, missing) into one profile safely.
  castoreClientCov = lib.optionalString common.coverage "env LLVM_PROFILE_FILE=/var/lib/rio/cov/castore-client-%m.profraw ";
in
{
  # NixOS module merged into the standalone fixture's client VM
  # (extraClientModules): the client doubles as the FUSE/mountd machine.
  fuseClientModule = {
    # rio-mountd needs an XFS-with-prjquota staging loopback and the
    # fuse passthrough machinery; the unprivileged serve-castore user
    # must have rio-builder (gid 990) as its PRIMARY group to pass the
    # daemon's SO_PEERCRED gid gate (production pod fsGroup analogue).
    boot.kernelModules = [
      "fuse"
      "loop"
    ];
    boot.supportedFilesystems = [ "xfs" ];
    users = {
      groups.rio-builder.gid = 990;
      users.builder = {
        isNormalUser = true;
        uid = 1000;
        group = "rio-builder";
      };
    };
    environment.systemPackages = [
      common.rio-workspace
      pkgs.xfsprogs
      pkgs.util-linux
      pkgs.b3sum
      pkgs.curl
    ];
    # mkClientNode sets 1024 MiB / default disk; this node additionally
    # runs rio-mountd + the (instrumented, in coverage mode) FUSE client
    # and hosts the 1 GiB staging image.
    virtualisation.memorySize = lib.mkForce 2048;
    virtualisation.diskSize = 4096;
  };

  mkTest =
    { fixture }:
    let
      inherit (fixture) gatewayHost;
    in
    pkgs.testers.runNixOSTest {
      name = "rio-castore-fuse";
      skipTypeCheck = true;
      # Boot (3 VMs) + seed + three ssh-ng builds + DAG prefetch + chunk
      # streaming under TCG + a second mount + coverage collection.
      globalTimeout = 1200 + common.covTimeoutHeadroom;

      inherit (fixture) nodes;

      testScript = ''
        import base64
        import hashlib
        import hmac
        import time

        ${common.mkBootstrap {
          inherit fixture gatewayHost;
          withSeed = true;
        }}

        ${common.mkBuildHelperV2 {
          inherit gatewayHost;
          dumpLogsExpr = "dump_all_logs([${gatewayHost}, worker])";
        }}

        # Values shared across subtests/phases.
        STAGING_QUOTA = 268435456
        BIGBLOB_SIZE = 307200
        METRICS_HOST = "127.0.0.1"
        METRICS_PORT = 9095

        # ══ FUSE/mountd machine setup (client doubles as the builder node) ══
        client.succeed("mkdir -p /var/rio/castore /var/rio/staging /var/rio/cache /var/rio/chunks")
        # Profraw drop dir (collectCoverage tars it). serve-castore runs as
        # the unprivileged builder uid, so it must be able to write here.
        client.succeed("mkdir -p /var/lib/rio/cov && chmod 0777 /var/lib/rio/cov")
        client.succeed("truncate -s 1G /var/rio-staging.img && mkfs.xfs -q /var/rio-staging.img")
        client.succeed("mount -o loop,prjquota /var/rio-staging.img /var/rio/staging")

        # Raw-exec rio-mountd (no systemd unit) — same shape as vm-mountd.
        # covShellEnv sets LLVM_PROFILE_FILE in coverage mode (empty
        # otherwise). SIGTERM at the end (never -9) flushes its profraws.
        client.succeed(
            "${common.covShellEnv}"
            "${common.rio-workspace}/bin/rio-mountd --socket /run/rio-mountd.sock"
            " --castore-dir /var/rio/castore --staging-dir /var/rio/staging"
            " --cache-dir /var/rio/cache --chunks-dir /var/rio/chunks"
            f" --staging-quota-bytes {STAGING_QUOTA} --allowed-gid 990"
            f" --metrics-addr {METRICS_HOST}:{METRICS_PORT}"
            " >>/var/log/rio-mountd.log 2>&1 & echo $! > /run/rio-mountd.pid"
        )
        client.wait_until_succeeds("test -S /run/rio-mountd.sock", timeout=15)

        # ══ Content: everything mounted below was ingested THROUGH the rio
        # store (busybox via the gateway seed, the two builds via ssh-ng),
        # so the NAR index + castore DAG exist server-side. ══
        busybox_path = "${common.busybox}"
        small_path = build("${smallDrv}")
        big_path = build("${drvs.bigblob}")
        link_path = build("${symlinkDrv}")
        assert small_path.startswith("/nix/store/"), f"unexpected build output {small_path!r}"
        assert big_path.startswith("/nix/store/"), f"unexpected build output {big_path!r}"
        assert link_path.startswith("/nix/store/"), f"unexpected build output {link_path!r}"
        mounted_paths = [busybox_path, small_path, big_path, link_path]
        print(f"castore-fuse: mounting {mounted_paths}")

        def basename(p):
            return p.removeprefix("/nix/store/")

        # ══ Tenancy: DirectoryService resolves the caller's tenant from the
        # assignment token and joins path_tenants on sha256(store_path). ══
        tenant_id = psql(
            ${gatewayHost},
            "INSERT INTO tenants (tenant_name) VALUES ('castore-vm') RETURNING tenant_id",
        )
        assert tenant_id, "tenant INSERT returned no tenant_id"
        for p in mounted_paths:
            path_hash = hashlib.sha256(p.encode()).hexdigest()
            psql(
                ${gatewayHost},
                "INSERT INTO path_tenants (store_path_hash, tenant_id) "
                f"VALUES (decode('{path_hash}', 'hex'), '{tenant_id}') "
                "ON CONFLICT DO NOTHING",
            )

        # Assignment token: base64url(claims).base64url(hmac_sha256(key, claims)),
        # signed with the fixture's ASSIGNMENT key (hmac.key, not the
        # service key). Claim shape mirrors rio_auth::hmac::AssignmentClaims
        # (deny_unknown_fields). The verifier byte-trims one trailing
        # newline from the key file, so do the same before signing.
        key = ${gatewayHost}.succeed("cat ${fixture.hmacKeys}/hmac.key").encode()
        for suffix in (b"\r\n", b"\n"):
            if key.endswith(suffix):
                key = key[: -len(suffix)]
                break
        claims = json.dumps(
            {
                "executor_id": "vm-castore",
                "drv_hash": "vm-test",
                "expected_outputs": [],
                "is_ca": False,
                "expiry_unix": int(time.time()) + 6 * 3600,
                "tenant": tenant_id,
            },
            separators=(",", ":"),
        ).encode()
        sig = hmac.new(key, claims, hashlib.sha256).digest()

        def b64url(data):
            return base64.urlsafe_b64encode(data).rstrip(b"=").decode()

        token = b64url(claims) + "." + b64url(sig)
        client.succeed(f"printf '%s' '{token}' > /tmp/assignment.token")

        # ══ serve-castore helpers ═════════════════════════════════════════
        SERVE = (
            "${castoreClientCov}${common.rio-workspace}/bin/spike_mountd_client"
            " --socket /run/rio-mountd.sock serve-castore"
            " --store-addr ${gatewayHost}:9002"
            " --assignment-token-file /tmp/assignment.token"
            " --cache-dir /var/rio/cache --chunks-dir /var/rio/chunks"
            " --staging-root /var/rio/staging"
            " --stream-threshold 65536"
        )

        def serve_castore(build_id, tag, paths):
            # Background the production session as the unprivileged builder
            # uid. The wrapper subshell records the exit status so the
            # teardown subtests can assert a clean (0) exit after SIGTERM.
            args = " ".join(f"--store-path {p}" for p in paths)
            cmd = f"{SERVE} --build-id {build_id} {args} --ready-file /tmp/{tag}.ready"
            client.succeed(
                "runuser -u builder -- bash -c "
                f"'( {cmd} >/tmp/{tag}.log 2>&1; echo $? >/tmp/{tag}.exit ) >/dev/null 2>&1 &'"
            )

        def wait_ready(tag):
            # Ready file = mounted + prefetched + serving. If the process
            # dies first, surface its log instead of a bare timeout.
            client.wait_until_succeeds(
                f"test -f /tmp/{tag}.ready -o -f /tmp/{tag}.exit", timeout=300
            )
            rc, _ = client.execute(f"test -f /tmp/{tag}.ready")
            if rc != 0:
                log = client.succeed(f"cat /tmp/{tag}.log")
                raise Exception(f"serve-castore [{tag}] exited before ready:\n{log}")

        def stop_serve(tag):
            # SIGTERM by comm name: the bash wrapper's command line also
            # contains the build id, so a -f match would signal the wrapper
            # instead of the client. Returns the recorded exit status.
            client.succeed("pkill -TERM -u builder spike_mountd || true")
            client.wait_until_succeeds(f"test -f /tmp/{tag}.exit", timeout=60)
            return client.succeed(f"cat /tmp/{tag}.exit").strip()

        def listing(root):
            # Type + relative path for everything; size + mode only for
            # regular files (castore dirs report size 0); target for
            # symlinks. Sorted so the comparison is order-independent.
            return client.succeed(
                f"cd {root} && find . -mindepth 1 "
                "\\( -type f -printf 'f %P %s %m\\n' \\) -o "
                "\\( -type l -printf 'l %P %l\\n' \\) -o "
                "\\( -type d -printf 'd %P\\n' \\) | sort"
            )

        def mountd_metrics():
            return scrape_metrics(client, METRICS_PORT, host=METRICS_HOST)

        def wait_mountd_idle():
            # The daemon has finished tearing down every registered
            # connection (uid + build_id released) — same gate vm-mountd
            # uses before reusing a uid. A failed scrape counts as
            # not-idle so an unreachable exporter cannot pass the gate.
            client.wait_until_succeeds(
                f"c=$(curl -sf {METRICS_HOST}:{METRICS_PORT}/metrics"
                " | awk '/^rio_mountd_connections_current/ {print $2}');"
                ' [ -n "$c" ] && [ "$c" = "0" ]',
                timeout=30,
            )

        def cache_snapshot():
            # (cache entries, chunk entries, promoted bytes, BackingOpen
            # count) — the four counters the cache-hit phase compares
            # before/after the warm reads.
            m = mountd_metrics()
            return (
                int(client.succeed("find /var/rio/cache -type f | wc -l").strip()),
                int(client.succeed("find /var/rio/chunks -type f | wc -l").strip()),
                metric_value(m, "rio_mountd_promote_bytes_total") or 0.0,
                metric_value(
                    m,
                    "rio_mountd_request_seconds_count",
                    labels='{op="backing_open"}',
                )
                or 0.0,
            )

        # ══ Phase A: first build-id — cold reads through every data path ═══
        with subtest("mount: serve-castore mounts, prefetches, writes ready file"):
            serve_castore("b1", "b1", mounted_paths)
            wait_ready("b1")
            ready = client.succeed("cat /tmp/b1.ready")
            assert f"quota={STAGING_QUOTA}" in ready, f"ready file: {ready!r}"
            fstype = client.succeed("findmnt -rn -o FSTYPE /var/rio/castore/b1").strip()
            assert fstype == "fuse.rio-castore", f"fstype={fstype}"
            entries = set(client.succeed("ls /var/rio/castore/b1").split())
            expected = {basename(p) for p in mounted_paths}
            assert entries == expected, f"mount root {entries!r} != {expected!r}"
            # The unprivileged builder uid (the one that will run the build)
            # can traverse it too.
            client.succeed("runuser -u builder -- ls /var/rio/castore/b1")

        with subtest("metadata: readdir/lookup/stat match the original store path"):
            orig = listing(busybox_path)
            fuse = listing(f"/var/rio/castore/b1/{basename(busybox_path)}")
            assert orig == fuse, (
                "castore-FUSE metadata diverges from the original store path\n"
                f"--- original ---\n{orig}\n--- fuse ---\n{fuse}"
            )

        with subtest("small-read: whole-file ReadBlob path is byte-identical and promoted"):
            fuse_small = f"/var/rio/castore/b1/{basename(small_path)}"
            content = client.succeed(f"cat {fuse_small}")
            assert content == "castore-small\n", f"small file content {content!r}"
            small_digest = client.succeed(f"b3sum --no-names {fuse_small}").strip()
            # The whole-file miss path promotes into the shared node cache
            # before open() returns; the entry is mountd-owned and read-only
            # to builders, named by the file digest.
            cache_entry = f"/var/rio/cache/{small_digest[:2]}/{small_digest}"
            st = client.succeed(f"stat -c '%a %U' {cache_entry}").strip()
            assert st == "444 root", f"promoted cache entry is {st}"
            # The unprivileged builder uid can read the shared cache but
            # must not be able to modify it: no overwriting a promoted
            # entry, no creating new files alongside it.
            client.fail(f"runuser -u builder -- sh -c 'echo x > {cache_entry}'")
            client.fail(
                "runuser -u builder -- sh -c "
                f"'echo x > /var/rio/cache/{small_digest[:2]}/builder-injected'"
            )

        with subtest("streaming-read: StatBlob+GetChunks path is byte-identical"):
            # bigblob: 300 KiB of zeros, > --stream-threshold (65536).
            fuse_blob = f"/var/rio/castore/b1/{basename(big_path)}/blob"
            size = client.succeed(f"stat -c %s {fuse_blob}").strip()
            assert size == str(BIGBLOB_SIZE), f"bigblob size through FUSE is {size}"
            big_digest = client.succeed(f"b3sum --no-names {fuse_blob}").strip()
            want_zeros = client.succeed(
                f"head -c {BIGBLOB_SIZE} /dev/zero | b3sum --no-names"
            ).strip()
            assert big_digest == want_zeros, "bigblob bytes corrupted through the streaming path"
            # The real busybox binary (~1.4 MiB) through the same path,
            # compared against the original bytes in the client's local store.
            fuse_bb = f"/var/rio/castore/b1/{basename(busybox_path)}/bin/busybox"
            bb_digest = client.succeed(f"b3sum --no-names {fuse_bb}").strip()
            bb_orig = client.succeed("b3sum --no-names ${common.busybox}/bin/busybox").strip()
            assert bb_digest == bb_orig, "busybox binary corrupted through the streaming path"
            # Streaming sources chunks via GetChunks and PromoteChunks-es the
            # misses into the node chunk cache; the whole-file path never
            # touches /var/rio/chunks, so entries here prove the chunk path ran.
            nchunks = int(client.succeed("find /var/rio/chunks -type f | wc -l").strip())
            assert nchunks > 0, "no chunk-cache entries — the streaming fill did not run"
            # The completed fills are promoted (asynchronously) into the
            # shared cache; wait for them so the cache-hit phase below is
            # deterministic.
            for d in (big_digest, bb_digest):
                client.wait_until_succeeds(
                    f"test -f /var/rio/cache/{d[:2]}/{d}", timeout=120
                )

        with subtest("readlink: symlink target through FUSE matches the original"):
            # symlinkDrv was built and ingested through the gateway above,
            # so the expected targets are constants the scenario controls —
            # no dependence on busybox's applet layout or on which machine
            # has the original path in its local store. (Every busybox
            # applet symlink target is still compared FUSE-vs-original by
            # the metadata subtest's listing().)
            link_root = f"/var/rio/castore/b1/{basename(link_path)}"
            for rel, want in (("bin/tool-link", "tool"), ("top-link", "bin/tool")):
                ftype = client.succeed(f"stat -c %F {link_root}/{rel}").strip()
                assert ftype == "symbolic link", f"{rel} through FUSE is {ftype!r}"
                target = client.succeed(f"readlink {link_root}/{rel}").strip()
                assert target == want, f"readlink {rel}: {target!r} != {want!r}"
            # The relative target resolves inside the same mount to the
            # file content the derivation wrote.
            content = client.succeed(f"cat {link_root}/top-link")
            assert content == "castore-symlink-target\n", f"resolved content {content!r}"

        with subtest("negative-lookup: ENOENT outside the closure, mount stays healthy"):
            bb_root = f"/var/rio/castore/b1/{basename(busybox_path)}"
            rc, out = client.execute(f"cat {bb_root}/no-such-entry 2>&1")
            assert rc != 0 and "No such file" in out, f"rc={rc} out={out!r}"
            client.fail("test -e /var/rio/castore/b1/not-a-mounted-path")
            content = client.succeed(f"cat /var/rio/castore/b1/{basename(small_path)}")
            assert content == "castore-small\n", "mount wedged after negative lookups"

        with subtest("teardown-b1: SIGTERM exits 0, mountd detaches mount and staging"):
            rc = stop_serve("b1")
            log = client.succeed("cat /tmp/b1.log")
            assert rc == "0", f"serve-castore exit status {rc} after SIGTERM; log:\n{log}"
            client.wait_until_succeeds(
                "! mountpoint -q /var/rio/castore/b1"
                " && test ! -e /var/rio/castore/b1"
                " && test ! -e /var/rio/staging/b1",
                timeout=30,
            )
            # uid + build_id released — the same uid mounts again below.
            wait_mountd_idle()

        # ══ Negative: a path that was never uploaded to the rio-store ══════
        with subtest("missing-path: never-uploaded path fails fast, no mount left behind"):
            client.succeed("mkdir -p /tmp/nu && echo rio-castore-never-uploaded-v1 > /tmp/nu/payload")
            missing_path = client.succeed("nix-store --add /tmp/nu/payload").strip()
            assert missing_path.startswith("/nix/store/"), f"unexpected: {missing_path!r}"
            serve_castore("b-missing", "missing", [missing_path])
            client.wait_until_succeeds("test -f /tmp/missing.exit", timeout=120)
            rc = client.succeed("cat /tmp/missing.exit").strip()
            log = client.succeed("cat /tmp/missing.log")
            assert rc != "0", f"serve-castore against a never-uploaded path exited 0:\n{log}"
            assert "QueryPathInfo" in log, f"expected a root-node resolution failure, log:\n{log}"
            client.succeed("test ! -e /tmp/missing.ready")
            client.wait_until_succeeds(
                "test ! -e /var/rio/castore/b-missing && test ! -e /var/rio/staging/b-missing",
                timeout=15,
            )

        # ══ Phase B: second build-id — everything served from the shared cache ══
        with subtest("cache-hit: second build-id reads via the shared cache, no re-fetch"):
            cache_before, chunks_before, promote_before, backing_before = cache_snapshot()
            # Anchor the no-re-promote comparison: phase A must have
            # promoted real bytes (small file + the two streaming fills),
            # otherwise a renamed/missing metric would make the equality
            # below vacuously true.
            assert promote_before > 0, (
                f"rio_mountd_promote_bytes_total = {promote_before} after phase A"
            )

            serve_castore("b2", "b2", mounted_paths)
            wait_ready("b2")
            base = "/var/rio/castore/b2"
            content = client.succeed(f"cat {base}/{basename(small_path)}")
            assert content == "castore-small\n", f"warm small-file content {content!r}"
            warm_blob = client.succeed(f"b3sum --no-names {base}/{basename(big_path)}/blob").strip()
            assert warm_blob == big_digest, "bigblob bytes differ on the warm read"
            warm_bb = client.succeed(
                f"b3sum --no-names {base}/{basename(busybox_path)}/bin/busybox"
            ).strip()
            assert warm_bb == bb_digest, "busybox bytes differ on the warm read"

            cache_after, chunks_after, promote_after, backing_after = cache_snapshot()
            assert promote_after == promote_before, (
                f"warm reads re-promoted content ({promote_before} -> {promote_after} bytes) "
                "instead of hitting the shared cache"
            )
            assert cache_after == cache_before, (
                f"warm reads grew the backing cache ({cache_before} -> {cache_after} entries)"
            )
            assert chunks_after == chunks_before, (
                f"warm reads grew the chunk cache ({chunks_before} -> {chunks_after} entries)"
            )
            assert backing_after > backing_before, (
                "no BackingOpen during the warm reads — cache hits should be passthrough "
                f"(count {backing_before} -> {backing_after})"
            )

        with subtest("teardown-final: second session and rio-mountd exit cleanly"):
            rc = stop_serve("b2")
            assert rc == "0", f"second serve-castore exit status {rc} after SIGTERM"
            client.wait_until_succeeds(
                "test ! -e /var/rio/castore/b2 && test ! -e /var/rio/staging/b2",
                timeout=30,
            )
            # SIGTERM (never -9): the daemon returns from main so its atexit
            # profraw flush runs before collectCoverage tars the cov dir.
            client.succeed("kill $(cat /run/rio-mountd.pid)")
            client.wait_until_succeeds(
                "! kill -0 $(cat /run/rio-mountd.pid) 2>/dev/null", timeout=15
            )

        ${common.collectCoverage fixture.pyNodeVars}
      '';
    };
}
