# Castore-e2e scenario: the ADR-022 castore-FUSE stack end-to-end on the
# k3s prod-parity fixture (P0560 §B, round 3b).
#
# What this covers that no other test does: real builds whose /nix/store
# lower is the per-build castore-FUSE mount, asserted at the level the
# ADR promises — streaming opens for >threshold inputs, passthrough on
# node-cache hits (zero read upcalls), cross-build dedup through
# /var/rio/{cache,chunks}, content-addressed inodes, infinite-TTL
# metadata, listxattr, the read-only cache boundary, and the fault
# matrix (corrupted chunk, store outage, mountd restart). The unit and
# vm-mountd suites prove the pieces; this proves the assembled stack on
# the production pod path (controller-spawned one-shot Jobs, mountd
# DaemonSet, HMAC assignment tokens, JWT-attributed sources).
#
# Fragment architecture: same shape as lifecycle.nix — this file returns
# { fragments, mkTest }; default.nix composes two tests
# (vm-castore-e2e-core / vm-castore-e2e-faults) out of one prelude +
# the fragments under ./castore-e2e/. The r[verify ...] markers live at
# the default.nix subtests entries (P0341 convention).
#
# Assertion doctrine (the design notes this scenario was built from):
#   - Worker pods are ONE-SHOT: per-build counters start at zero and the
#     pod exits right after completion. Every metric assertion is taken
#     DURING the build, against a sleep-tail in the drv script, gated on
#     the metric itself (wait_worker_metric) — never after completion,
#     never on wall-clock.
#   - The node caches are host-visible: /var/rio/{cache,chunks,staging}
#     on k3s-agent, sharded {root}/{2-hex}/{blake3-hex} with the digest
#     equal to plain BLAKE3 of the file bytes — so b3sum on the client
#     gives exact host-side paths to probe for "promoted" /
#     "not re-fetched" / "not poisoned".
#   - All builder pods are pinned to k3s-agent (vmtest-castore.yaml
#     poolDefaults.nodeSelector) so cache-sharing assertions are
#     same-node by construction; the store stays pinned to k3s-server.
#   - No latency gates anywhere. Latency claims from the plan (<500 ms
#     cold open, <50 ms streaming open) are printed for humans, not
#     asserted (vm-mountd precedent).
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) ns nsStore nsBuilders;
  protoset = import ../lib/protoset.nix { inherit pkgs; };
  jwtKeys = import ../lib/jwt-keys.nix;

  grpcurl = "${pkgs.grpcurl}/bin/grpcurl";
  grpcurlTls = "-plaintext";
  b3sum = "${pkgs.b3sum}/bin/b3sum";
  inherit (common) busybox;

  # GNU cp for the xattr-copy subtest: busybox cp does not issue
  # llistxattr, GNU `cp -a` probes it with size=0 then size>0 — the
  # exact branch r[builder.fs.listxattr-size-branch] is about.
  # pkgsStatic so the closure is self-contained (no glibc).
  coreutilsStatic = pkgs.pkgsStatic.coreutils;
  coreutilsClosure = pkgs.closureInfo { rootPaths = [ coreutilsStatic ]; };

  # Mint a tenant JWT signed with the lib/jwt-keys.nix test seed (the
  # same seed the fixture passes to the chart via jwt.signingSeed).
  # Same helper as lifecycle.nix — the prelude creates the vm-castore
  # tenant and attaches this token to every grpcurl-direct
  # SchedulerService call (SubmitBuild / CancelBuild).
  pyWithJwt = pkgs.python3.withPackages (
    ps: with ps; [
      pyjwt
      cryptography
    ]
  );
  signJwt = pkgs.writeScript "sign-jwt-castore" ''
    #!${pyWithJwt}/bin/python3
    import sys, time, base64, jwt
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    seed = base64.b64decode("${jwtKeys.seedB64}")
    sk = Ed25519PrivateKey.from_private_bytes(seed)
    now = int(time.time())
    claims = {"sub": sys.argv[1], "iat": now, "exp": now + 7200,
              "jti": "vm-castore"}
    print(jwt.encode(claims, sk, algorithm="EdDSA"))
  '';

  # ── Test derivations ──────────────────────────────────────────────────
  # All inputs are RUNTIME-generated files on the client (urandom), added
  # with `nix-store --add` and pushed through the gateway as the
  # vm-castore tenant, so every .nix below takes its inputs as ARGS at
  # nix-instantiate time (submit_drv extra_args / build extra_args).
  # Distinct names per subtest so DAG-dedup never turns a later build
  # into a cache hit of an earlier one.
  #
  # Sleep tails are `for … busybox sleep 5` loops, generously sized; the
  # test cancels the build as soon as it has scraped, so the tail is
  # rarely paid in full. The interesting reads happen BEFORE the
  # CASTORE-PROBE-READY marker so a metric gate on the read implies the
  # reads ran.

  # cold-read: streaming open of a 32 MiB input (> 8 MiB
  # stream_threshold_bytes default), plus a 4 KiB whole-file miss. The
  # cmp makes "the streamed bytes are correct" a build-script property
  # (dd output of the first 4 KiB must equal the separately-pushed
  # head4k file); the sentinel read AFTER it is how the test observes
  # the cmp passed — build-script stdout is not reliably visible in
  # `kubectl logs` (it flows through the LogService data plane, and the
  # pod log is debug-level executor tracing), so the host-visible cache
  # entry of a first-ever-read input is the structural "script got this
  # far" signal.
  coldDrv = pkgs.writeText "castore-cold.nix" ''
    { busybox, bigA, head4kA, sentinel }:
    derivation {
      name = "rio-castore-cold";
      system = builtins.currentSystem;
      builder = "''${busybox}/bin/sh";
      args = [ "-c" '''
        ''${busybox}/bin/busybox dd if=''${bigA} bs=4096 count=1 2>/dev/null \
          | ''${busybox}/bin/busybox cmp - ''${head4kA} || exit 1
        ''${busybox}/bin/busybox cat ''${sentinel} > /dev/null || exit 1
        echo CASTORE-PROBE-READY
        for i in $(''${busybox}/bin/busybox seq 1 36); do ''${busybox}/bin/busybox sleep 5; done
        echo done > $out
      ''' ];
    }
  '';

  # warm-read: second build of the same 32 MiB input on the same node —
  # must be a backing-cache hit served via passthrough (no read
  # upcalls, nothing fetched).
  warmDrv = pkgs.writeText "castore-warm.nix" ''
    { busybox, bigA }:
    derivation {
      name = "rio-castore-warm";
      system = builtins.currentSystem;
      builder = "''${busybox}/bin/sh";
      args = [ "-c" '''
        ''${busybox}/bin/busybox dd if=''${bigA} of=/dev/null bs=1M count=4 2>/dev/null || exit 1
        echo CASTORE-PROBE-READY
        for i in $(''${busybox}/bin/busybox seq 1 24); do ''${busybox}/bin/busybox sleep 5; done
        echo done > $out
      ''' ];
    }
  '';

  # passthrough-small: a ≤threshold (1 MiB) input opened twice — both
  # opens must be passthrough, zero read upcalls. The /var/rio/cache
  # write attempt sits BEFORE the warm_4k sentinel read: if the write
  # SUCCEEDS the script exits 9 and the sentinel never gets read, so
  # the warm_4k cache entry the test gates on doubles as the
  # cache-readonly "the sandbox could not create the file" proof
  # (plus the host probe in that subtest).
  ptSmallDrv = pkgs.writeText "castore-pt-small.nix" ''
    { busybox, small1m, warm4k }:
    derivation {
      name = "rio-castore-pt-small";
      system = builtins.currentSystem;
      builder = "''${busybox}/bin/sh";
      args = [ "-c" '''
        ''${busybox}/bin/busybox cat ''${small1m} > /dev/null || exit 1
        ''${busybox}/bin/busybox cat ''${small1m} > /dev/null || exit 1
        if echo poison > /var/rio/cache/ab/test 2>/dev/null; then exit 9; fi
        ''${busybox}/bin/busybox cat ''${warm4k} > /dev/null || exit 1
        echo CASTORE-PROBE-READY
        for i in $(''${busybox}/bin/busybox seq 1 24); do ''${busybox}/bin/busybox sleep 5; done
        echo ok > $out
      ''' ];
    }
  '';

  # cross-build-dedup: a DIFFERENT derivation re-reading inputs earlier
  # builds already promoted (head4k + big_a from cold-read) — every open
  # must be served from the node digest cache, nothing re-fetched. The
  # full big_a read doubles as the gate (objects_cache_bytes ≥ 32 MiB
  # only once its hit was recorded) without fetching anything.
  xdedupDrv = pkgs.writeText "castore-xdedup.nix" ''
    { busybox, head4kA, bigA }:
    derivation {
      name = "rio-castore-xdedup";
      system = builtins.currentSystem;
      builder = "''${busybox}/bin/sh";
      args = [ "-c" '''
        ''${busybox}/bin/busybox cmp ''${head4kA} ''${head4kA} || exit 1
        ''${busybox}/bin/busybox dd if=''${bigA} of=/dev/null bs=1M 2>/dev/null || exit 1
        echo CASTORE-PROBE-READY
        for i in $(''${busybox}/bin/busybox seq 1 18); do ''${busybox}/bin/busybox sleep 5; done
        echo ok > $out
      ''' ];
    }
  '';

  # inode-dedup: two store paths with byte-identical content must share
  # one inode (file_digest-keyed inodes, ADR-022 §2.3). Synchronous
  # ssh-ng build — the assertion IS the build succeeding.
  inoDrv = pkgs.writeText "castore-ino.nix" ''
    { busybox, dedupA, dedupB }:
    derivation {
      name = "rio-castore-ino";
      system = builtins.currentSystem;
      builder = "''${busybox}/bin/sh";
      args = [ "-c" '''
        ia=$(''${busybox}/bin/busybox stat -c %i ''${dedupA}) || exit 1
        ib=$(''${busybox}/bin/busybox stat -c %i ''${dedupB}) || exit 1
        [ "$ia" = "$ib" ] || { echo "inode mismatch: $ia vs $ib"; exit 1; }
        ''${busybox}/bin/busybox cmp ''${dedupA} ''${dedupB} || exit 1
        echo ok > $out
      ''' ];
    }
  '';

  # stat-dcache-absorbed: five full traversals of a 50-file tree. With
  # every castore TTL infinite + READDIRPLUS, lookups/getattrs fire
  # ~once per dentry — the upcall counters must stay near one-traversal
  # counts instead of 5×.
  dcacheDrv = pkgs.writeText "castore-dcache.nix" ''
    { busybox, metaTree, sentinel }:
    derivation {
      name = "rio-castore-dcache";
      system = builtins.currentSystem;
      builder = "''${busybox}/bin/sh";
      args = [ "-c" '''
        for pass in 1 2 3 4 5; do
          ''${busybox}/bin/busybox find ''${metaTree} -type f \
            -exec ''${busybox}/bin/busybox stat -c %s {} \; > /dev/null || exit 1
        done
        ''${busybox}/bin/busybox cat ''${sentinel} > /dev/null || exit 1
        echo CASTORE-PROBE-READY
        for i in $(''${busybox}/bin/busybox seq 1 24); do ''${busybox}/bin/busybox sleep 5; done
        echo ok > $out
      ''' ];
    }
  '';

  # xattr-copy: GNU `cp -a` of a file and of a directory out of the
  # castore lower. cp -a issues llistxattr (size probe, then fetch) on
  # every source inode; success ⇔ the size>0 branch returns an empty
  # list instead of EIO. Synchronous build.
  xattrDrv = pkgs.writeText "castore-xattr.nix" ''
    { busybox, coreutils, small1m, metaTree }:
    derivation {
      name = "rio-castore-xattr";
      system = builtins.currentSystem;
      builder = "''${busybox}/bin/sh";
      args = [ "-c" '''
        ''${coreutils}/bin/cp -a ''${small1m} $TMPDIR/file-copy || exit 1
        ''${busybox}/bin/busybox cmp ''${small1m} $TMPDIR/file-copy || exit 1
        ''${coreutils}/bin/cp -a ''${metaTree} $TMPDIR/tree-copy || exit 1
        echo ok > $out
      ''' ];
    }
  '';

  # chunk-cache-stream: full re-stream of the 32 MiB input AFTER the
  # test evicted its whole-file backing entry (chunks left in place) —
  # the fill must be served from /var/rio/chunks, not re-fetched.
  chunkhitDrv = pkgs.writeText "castore-chunkhit.nix" ''
    { busybox, bigA }:
    derivation {
      name = "rio-castore-chunkhit";
      system = builtins.currentSystem;
      builder = "''${busybox}/bin/sh";
      args = [ "-c" '''
        ''${busybox}/bin/busybox dd if=''${bigA} of=/dev/null bs=1M 2>/dev/null || exit 1
        echo CASTORE-PROBE-READY
        for i in $(''${busybox}/bin/busybox seq 1 24); do ''${busybox}/bin/busybox sleep 5; done
        echo ok > $out
      ''' ];
    }
  '';

  # chunk-warm (faults baseline): stream the faults-split 24 MiB input
  # clean, and pre-warm the small inputs mountd-restart later reads so
  # its post-restart opens are cache hits by construction.
  chunkWarmDrv = pkgs.writeText "castore-chunk-warm.nix" ''
    { busybox, bigF, small1m, warm4k }:
    derivation {
      name = "rio-castore-chunk-warm";
      system = builtins.currentSystem;
      builder = "''${busybox}/bin/sh";
      args = [ "-c" '''
        ''${busybox}/bin/busybox dd if=''${bigF} of=/dev/null bs=1M 2>/dev/null || exit 1
        ''${busybox}/bin/busybox cat ''${small1m} > /dev/null || exit 1
        ''${busybox}/bin/busybox cat ''${warm4k} > /dev/null || exit 1
        echo CASTORE-PROBE-READY
        for i in $(''${busybox}/bin/busybox seq 1 30); do ''${busybox}/bin/busybox sleep 5; done
        echo ok > $out
      ''' ];
    }
  '';

  # integrity-fail: re-stream the 24 MiB input after the test corrupted
  # one of its node-cache chunks (size-preserving). The streaming
  # fill's whole-file BLAKE3 must catch it: EIO to the reader, no
  # promote, integrity_fail_total == 1. The script never produces $out
  # on its own (the test cancels it after scraping) so corrupted bytes
  # can never become an output.
  corruptDrv = pkgs.writeText "castore-corrupt.nix" ''
    { busybox, bigF }:
    derivation {
      name = "rio-castore-corrupt";
      system = builtins.currentSystem;
      builder = "''${busybox}/bin/sh";
      args = [ "-c" '''
        ''${busybox}/bin/busybox dd if=''${bigF} of=/dev/null bs=1M 2>/dev/null
        echo DD-RC=$?
        echo CASTORE-PROBE-READY
        for i in $(''${busybox}/bin/busybox seq 1 36); do ''${busybox}/bin/busybox sleep 5; done
        exit 1
      ''' ];
    }
  '';

  # eio-circuit-breaker: mount + prefetch happen while the store is up;
  # the pre-sleep window gives the test time to scale the store to 0;
  # the six never-before-read inputs then all fail their fetches —
  # 5 consecutive failures open the breaker, every failed open is a
  # fast EIO (swallowed, the script itself never fails). The test
  # scrapes during the second tail and cancels.
  eioBreakerDrv = pkgs.writeText "castore-eio-breaker.nix" ''
    { busybox, e1, e2, e3, e4, e5, e6 }:
    derivation {
      name = "rio-castore-eio-breaker";
      system = builtins.currentSystem;
      builder = "''${busybox}/bin/sh";
      extras = "''${e1} ''${e2} ''${e3} ''${e4} ''${e5} ''${e6}";
      args = [ "-c" '''
        echo CASTORE-MOUNTED
        for i in $(''${busybox}/bin/busybox seq 1 24); do ''${busybox}/bin/busybox sleep 5; done
        for f in $extras; do ''${busybox}/bin/busybox cat $f > /dev/null || true; done
        echo CASTORE-EIO-DONE
        for i in $(''${busybox}/bin/busybox seq 1 36); do ''${busybox}/bin/busybox sleep 5; done
        echo ok > $out
      ''' ];
    }
  '';

  # eio-infra-retry: the derivation's BUILDER is a never-node-cached
  # script whose store-side chunk the test takes offline — the daemon's
  # execve gets EIO from the castore lower, nix-daemon reports
  # "executing '<closure root>': Input/output error", and the executor
  # must reclassify that MiscFailure as InfrastructureFailure
  # (r[builder.result.input-eio-is-infra]) so the scheduler re-queues
  # instead of poisoning. Once the test restores the chunk, the retry
  # must complete. busybox is referenced via the env attr so the
  # script's shebang interpreter is part of the input closure.
  eioInfraDrv = pkgs.writeText "castore-eio-infra.nix" ''
    { busybox, eioBuilder }:
    derivation {
      name = "rio-castore-eio-infra";
      system = builtins.currentSystem;
      builder = eioBuilder;
      bb = busybox;
      args = [ ];
    }
  '';

  # mountd-restart phase A: hold a passthrough fd across the mountd
  # restart, then keep reading from it and open another already-cached
  # digest. Both inputs are cache hits by construction (chunk-warm read
  # them), so no whole-file miss promote can land in the restart
  # window. The build must COMPLETE (the test waits for it instead of
  # cancelling).
  mountdRestartDrv = pkgs.writeText "castore-mountd-restart.nix" ''
    { busybox, small1m, warm4k }:
    derivation {
      name = "rio-castore-mountd-restart";
      system = builtins.currentSystem;
      builder = "''${busybox}/bin/sh";
      args = [ "-c" '''
        exec 3<''${small1m}
        ''${busybox}/bin/busybox dd of=/dev/null bs=64k count=1 <&3 2>/dev/null || exit 1
        echo CASTORE-FD-HELD
        for i in $(''${busybox}/bin/busybox seq 1 30); do ''${busybox}/bin/busybox sleep 5; done
        ''${busybox}/bin/busybox dd of=/dev/null bs=1M <&3 2>/dev/null || exit 1
        ''${busybox}/bin/busybox cat ''${warm4k} > /dev/null || exit 2
        echo ok > $out
      ''' ];
    }
  '';

  # mountd-restart phase D: a fresh never-seen input forces a full
  # miss → fetch → Promote against the RESTARTED daemon. Synchronous
  # build; success proves Mount/BackingOpen/Promote on the new socket.
  postRestartDrv = pkgs.writeText "castore-post-restart.nix" ''
    { busybox, postSeed }:
    derivation {
      name = "rio-castore-post-restart";
      system = builtins.currentSystem;
      builder = "''${busybox}/bin/sh";
      args = [ "-c" '''
        ''${busybox}/bin/busybox cat ''${postSeed} > /dev/null || exit 1
        echo ok > $out
      ''' ];
    }
  '';

  # ── testScript prelude ────────────────────────────────────────────────
  prelude = ''
    ${common.assertions}

    ${common.kvmCheck}
    start_all()
    ${fixture.waitReady}

    ${fixture.kubectlHelpers}

    import time

    # ── Castore metric/cache helpers ──────────────────────────────────
    # Builder pod metrics: 9093, mountd DaemonSet: 9095 (both ns
    # rio-builders), store: 9092 (ns rio-store). proxy_metrics is the
    # assertions.py apiserver pods/proxy scrape.

    def worker_metrics(pod):
        return proxy_metrics(k3s_server, pod, 9093, ns="${nsBuilders}")

    def mountd_pod(node="k3s-agent"):
        return kubectl(
            "get pod -l app.kubernetes.io/name=rio-mountd "
            f"--field-selector spec.nodeName={node} "
            "-o jsonpath='{.items[0].metadata.name}'",
            ns="${nsBuilders}",
        ).strip()

    def mountd_metrics(node="k3s-agent"):
        return proxy_metrics(k3s_server, mountd_pod(node), 9095, ns="${nsBuilders}")

    def store_metrics():
        pod = kubectl(
            "get pod -l app.kubernetes.io/name=rio-store "
            "--field-selector=status.phase=Running "
            "-o jsonpath='{.items[0].metadata.name}'",
            ns="${nsStore}",
        ).strip()
        return proxy_metrics(k3s_server, pod, 9092, ns="${nsStore}")

    def series(m, name, must=(), forbid=()):
        """Sum every series of `name` whose label string contains all the
        `must` substrings and none of the `forbid` ones. Missing → 0.0.
        Label-order independent (metrics-rs emits labels in insertion
        order, which is an implementation detail)."""
        total = 0.0
        for labels, val in m.get(name, {}).items():
            if all(s in labels for s in must) and not any(s in labels for s in forbid):
                total += val
        return total

    def fam(m, name):
        """All series of one metric family, for assertion messages."""
        return m.get(name, {})

    def dump_castore_diag(ctx, pod=None):
        """Best-effort failure diagnostics: builder pod + mountd + recent
        scheduler activity. Called from except-arms before re-raising."""
        k3s_server.execute(
            f"echo '=== DIAG[{ctx}]: builder pods ===' >&2; "
            "k3s kubectl -n ${nsBuilders} get pods,jobs -o wide >&2 2>&1 || true"
        )
        if pod is not None:
            k3s_server.execute(
                f"echo '=== DIAG[{ctx}]: logs {pod} ===' >&2; "
                f"k3s kubectl -n ${nsBuilders} logs {pod} --tail=120 2>&1 "
                "  | grep -vE '\"level\":\"DEBUG\"' | tail -80 >&2 || true"
            )
        k3s_server.execute(
            f"echo '=== DIAG[{ctx}]: mountd (agent) ===' >&2; "
            "k3s kubectl -n ${nsBuilders} logs -l app.kubernetes.io/name=rio-mountd "
            "  --tail=40 --prefix >&2 2>&1 || true; "
            f"echo '=== DIAG[{ctx}]: scheduler leader ===' >&2; "
            "leader=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
            "  -o jsonpath='{.spec.holderIdentity}') && "
            'k3s kubectl -n ${ns} logs "$leader" --since=4m '
            "  | grep -viE '\"level\":\"DEBUG\"|heartbeat' | tail -50 >&2 || true"
        )

    def wait_worker_metric(pod, cond, timeout=240, ctx=""):
        """Wait until the builder pod's /metrics satisfies a shell
        condition (a pipe fragment appended after `... | `). The
        structural "the build reached the interesting point" gate — no
        sleeps in test code, no log polling."""
        try:
            k3s_server.wait_until_succeeds(
                "k3s kubectl get --raw "
                "'/api/v1/namespaces/${nsBuilders}/pods/"
                f"{pod}:9093/proxy/metrics' | {cond}",
                timeout=timeout,
            )
        except Exception:
            dump_castore_diag(f"wait_worker_metric {ctx or cond}", pod=pod)
            raise

    def castore_pod():
        """The Running builder pod for the in-flight build. Builds in
        this scenario are serial and pinned to k3s-agent, so label-based
        resolution is unambiguous; assert the pin while at it."""
        pod = wait_worker_pod()
        node = kubectl(
            f"get pod {pod} -o jsonpath='{{.spec.nodeName}}'", ns="${nsBuilders}"
        ).strip()
        assert node == "k3s-agent", (
            f"executor pod {pod} scheduled on {node!r}, expected k3s-agent — "
            "the vmtest-castore.yaml poolDefaults.nodeSelector pin is not "
            "flowing into the Job pod spec"
        )
        return pod

    def wait_no_running_builders(timeout=240):
        k3s_server.wait_until_succeeds(
            "test -z \"$(k3s kubectl -n ${nsBuilders} get pod -l rio.build/pool "
            "--field-selector=status.phase=Running -o name 2>/dev/null)\"",
            timeout=timeout,
        )

    # ── Host-side cache probes (k3s-agent) ────────────────────────────
    # Layout: /var/rio/cache/<2-hex>/<blake3-hex> (whole files),
    # /var/rio/chunks/<2-hex>/<blake3-hex> (FastCDC chunks). The digest
    # is plain BLAKE3 of the content, so b3sum on the client gives the
    # exact path.

    def cache_path(hex_digest):
        return f"/var/rio/cache/{hex_digest[:2]}/{hex_digest}"

    def assert_cached(hex_digest, what, timeout=300):
        try:
            k3s_agent.wait_until_succeeds(
                f"test -e {cache_path(hex_digest)}", timeout=timeout
            )
        except Exception:
            dump_castore_diag(f"assert_cached {what}")
            k3s_agent.execute(
                "echo '=== /var/rio/cache ===' >&2; "
                "find /var/rio/cache -type f >&2 2>/dev/null | head -50; "
                "echo '=== /var/rio/staging ===' >&2; "
                "ls -la /var/rio/staging >&2 2>/dev/null || true"
            )
            raise

    def assert_not_cached(hex_digest, what):
        k3s_agent.succeed(f"! test -e {cache_path(hex_digest)}")
        print(f"castore: {what} not in node cache (as expected)")

    # ── PG helpers ────────────────────────────────────────────────────

    def drv_status(drv_path):
        return psql_k8s(
            k3s_server,
            f"SELECT status FROM derivations WHERE drv_path = '{drv_path}' LIMIT 1",
        )

    def wait_drv_status(drv_path, statuses, timeout=300, ctx=""):
        deadline = time.time() + timeout
        last = ""
        while time.time() < deadline:
            last = drv_status(drv_path)
            if last in statuses:
                print(f"castore: {ctx or drv_path} reached status {last!r}")
                return last
            time.sleep(5)
        dump_castore_diag(f"wait_drv_status {ctx}: {drv_path} stuck at {last!r}")
        raise AssertionError(
            f"{ctx}: derivation {drv_path} status {last!r} not in {statuses} "
            f"after {timeout}s"
        )

    def wait_nar_indexed(paths, timeout=300):
        """Gate on store.index.putpath-bg-warm: GetDirectory/StatBlob can
        only serve a pushed path once its NAR index row is committed."""
        in_list = ", ".join("'" + p + "'" for p in paths)
        deadline = time.time() + timeout
        n = "0"
        while time.time() < deadline:
            n = psql_k8s(
                k3s_server,
                "SELECT count(*) FROM manifests m JOIN narinfo n "
                "USING (store_path_hash) "
                f"WHERE n.store_path IN ({in_list}) AND m.nar_indexed",
            )
            if int(n) == len(paths):
                print(f"castore: nar_indexed for all {len(paths)} seeded paths")
                return
            time.sleep(3)
        raise AssertionError(
            f"nar-index gate: only {n}/{len(paths)} seeded paths indexed "
            f"after {timeout}s: {paths}"
        )

    # ── Seed helpers (client-side generate + JWT-attributed push) ─────

    def client_b3(path):
        return client.succeed(f"${b3sum} --no-names {path}").strip()

    def add_and_push(*local_paths):
        """nix-store --add each file/dir on the client, push the batch via
        ssh-ng (the JWT-attributed tenant push), wait for the NAR index.
        Returns the store paths in argument order."""
        paths = [
            client.succeed(f"nix-store --add {p}").strip() for p in local_paths
        ]
        client.succeed(
            "nix copy --no-check-sigs --to 'ssh-ng://k3s-server' " + " ".join(paths)
        )
        wait_nar_indexed(paths)
        return paths

    # ── Build drivers ─────────────────────────────────────────────────

    def submit_build_grpc(payload: dict, max_time: int = 5) -> str:
        """SubmitBuild via port-forward + grpcurl, as the vm-castore
        tenant. Returns buildId. The stream read is capped — the build
        will not finish inside max_time; grpcurl exits DeadlineExceeded
        and ok_nonzero swallows it (the build is persisted on receipt)."""
        out = pf_exec(leader_pod(), 9001,
            f"${grpcurl} ${grpcurlTls} -max-time {max_time} "
            f"-H 'x-rio-tenant-token: {tenant_jwt}' "
            f"-protoset ${protoset}/rio.protoset "
            f"-d '{json.dumps(payload)}' "
            f"localhost:__PORT__ rio.scheduler.SchedulerService/SubmitBuild",
            ok_nonzero=True)
        return _parse_submit_build_id(out)

    ${common.mkSubmitHelpers "k3s-server"}

    def submit_drv(drv_file, extra_args="", **req):
        """Async build driver for every sleep-tail subtest."""
        return submit_single_drv(drv_file, extra_args=extra_args, **req)

    def cancel_build(build_id, reason="castore-e2e: evidence captured"):
        payload = json.dumps({"buildId": build_id, "reason": reason})
        return pf_exec(leader_pod(), 9001,
            f"${grpcurl} ${grpcurlTls} -max-time 30 "
            f"-H 'x-rio-tenant-token: {tenant_jwt}' "
            f"-protoset ${protoset}/rio.protoset "
            f"-d '{payload}' "
            f"localhost:__PORT__ rio.scheduler.SchedulerService/CancelBuild",
            ok_nonzero=True)

    def finish_async(build_id, ctx=""):
        """Reclaim a sleep-tail build once its evidence is captured:
        cancel (tolerating an already-terminal build) and wait for the
        executor pod to leave Running so the next subtest starts with a
        quiet pool."""
        cancel_build(build_id)
        wait_no_running_builders()
        print(f"castore: {ctx or build_id} reclaimed (cancelled + pool quiet)")

    # ── Tenant + JWT + SSH + seed ─────────────────────────────────────
    # Tenancy is mandatory end-to-end on the castore stack: the SSH key
    # comment names the tenant (gateway mints the session JWT for the
    # seed push → path_tenants attribution), and the scheduler's HMAC
    # assignment tokens carry the tenant for every castore RPC.
    tenant_id = psql_k8s(k3s_server,
        "INSERT INTO tenants (tenant_name) VALUES ('vm-castore') "
        "RETURNING tenant_id"
    )
    tenant_jwt = k3s_server.succeed(f"${signJwt} {tenant_id}").strip()
    print(f"castore: tenant vm-castore={tenant_id}")

    ${fixture.sshKeySetupFor "vm-castore"}
    ${common.seedBusybox "k3s-server"}

    # Shared small seeds (used by both splits): a 1 MiB and a 4 KiB
    # input. Generated fresh per run so node caches can never be warm
    # from a previous life.
    client.succeed(
        "dd if=/dev/urandom of=/tmp/castore-small-1m.bin bs=1M count=1 2>/dev/null && "
        "dd if=/dev/urandom of=/tmp/castore-warm-4k.bin bs=4096 count=1 2>/dev/null"
    )
    b3_small1m = client_b3("/tmp/castore-small-1m.bin")
    b3_warm4k = client_b3("/tmp/castore-warm-4k.bin")
    p_small1m, p_warm4k = add_and_push(
        "/tmp/castore-small-1m.bin", "/tmp/castore-warm-4k.bin"
    )

    # Seed-attribution gate (the tenancy recipe): the pushed closure must
    # have path_tenants rows for vm-castore, or every castore mount of it
    # returns NotFound and the whole scenario dies at the first build.
    seed_rows = int(psql_k8s(k3s_server,
        "SELECT count(*) FROM path_tenants pt JOIN tenants t USING (tenant_id) "
        "WHERE t.tenant_name = 'vm-castore'"
    ))
    assert seed_rows >= 3, (
        f"seeded paths have only {seed_rows} path_tenants rows for vm-castore — "
        "the gateway did not attach the tenant session JWT to the seed push "
        "(jwtEnabled missing on the fixture?) or the store did not verify it"
    )
    print(f"castore: seed attributed to vm-castore ({seed_rows} path_tenants rows)")

    # ── Synchronous build helper (ssh-ng, self-verifying) ─────────────
    ${common.mkBuildHelperV2 {
      gatewayHost = "k3s-server";
      dumpLogsExpr = ''dump_all_logs([], kube_node=k3s_server, kube_namespace="${ns}")'';
    }}
  '';

  # ── Fragments + composition ───────────────────────────────────────────
  scope = {
    inherit
      pkgs
      common
      ns
      nsStore
      nsBuilders
      busybox
      b3sum
      coreutilsStatic
      coreutilsClosure
      coldDrv
      warmDrv
      ptSmallDrv
      xdedupDrv
      inoDrv
      dcacheDrv
      xattrDrv
      chunkhitDrv
      chunkWarmDrv
      corruptDrv
      eioBreakerDrv
      eioInfraDrv
      mountdRestartDrv
      postRestartDrv
      ;
  };
  fragments = builtins.mapAttrs (_: f: f scope) (common.importDir ./castore-e2e);

  mkTest = common.mkFragmentTest {
    scenario = "castore-e2e";
    inherit prelude fragments fixture;
    defaultTimeout = 1800;
    chains = [
      # Seed fragments must precede every subtest that reads what they
      # generate (presence + ordering are both enforced).
      {
        before = "seed-core";
        after = "cold-read";
        msg = "cold-read reads big_a/head4k — seed-core must run first";
      }
      {
        before = "seed-core";
        after = "inode-dedup";
        msg = "inode-dedup reads the dedup pair — seed-core must run first";
      }
      {
        before = "seed-core";
        after = "stat-dcache-absorbed";
        msg = "stat-dcache reads meta_tree — seed-core must run first";
      }
      {
        before = "seed-core";
        after = "xattr-copy";
        msg = "xattr-copy reads meta_tree/coreutils — seed-core must run first";
      }
      # Node-cache state dependencies inside core.
      {
        before = "cold-read";
        after = "warm-read";
        msg = "warm-read asserts a cache hit of what cold-read promoted";
      }
      {
        before = "cold-read";
        after = "cross-build-dedup";
        msg = "cross-build-dedup re-reads cold-read's head4k promote";
      }
      {
        before = "cold-read";
        after = "chunk-cache-stream";
        msg = "chunk-cache-stream evicts the backing entry cold-read promoted";
      }
      {
        before = "passthrough-small";
        after = "cache-readonly";
        msg = "cache-readonly asserts on passthrough-small's pod logs";
      }
      # Faults split.
      {
        before = "seed-faults";
        after = "chunk-warm";
        msg = "chunk-warm streams big_f — seed-faults must run first";
      }
      {
        before = "seed-faults";
        after = "eio-circuit-breaker";
        msg = "eio-circuit-breaker reads the eio extras — seed-faults must run first";
      }
      {
        before = "seed-faults";
        after = "eio-infra-retry";
        msg = "eio-infra-retry uses the seeded eio builder — seed-faults must run first";
      }
      {
        before = "chunk-warm";
        after = "integrity-fail";
        msg = "integrity-fail corrupts a chunk chunk-warm promoted";
      }
      {
        before = "chunk-warm";
        after = "mountd-restart";
        msg = "mountd-restart needs the inputs chunk-warm pre-warmed";
      }
      {
        name = "mountd-restart";
        last = true;
        msg = "mountd-restart bounces the mountd DaemonSet — keep it last";
      }
    ];
  };
in
{
  inherit fragments mkTest;
}
