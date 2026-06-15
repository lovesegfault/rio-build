# ADR-024 P3 end-to-end join: the REAL `rio build` binary pair
# (coordinator + rio-eval) against the REAL in-VM cluster
# (postgres + store + scheduler + builder), no gateway, no ssh-ng.
#
# Everything else in the P3 chain is proven elsewhere: the coordinator
# against in-process services (rio-build-cli e2e suite), the eval
# parent against real libexpr (rio-eval-smoke, drvPath parity), the
# cluster services against each other (the other vm-* scenarios). The
# seam this scenario closes: real binary → external-door JWT auth →
# negotiate/upload → SubmitBuild → builder executes → BuildEvents
# stream → `--out-link` round-trips the output content.
#
# Subtests:
#   cold-build    eval → upload (chunked dir + single-file + symlink
#                 sources + drv blobs) → digest submission → worker
#                 executes dep+consumer → event progression on stdout →
#                 fetched output content byte-checked → drv blobs
#                 tenant-bound in PG
#   warm-rerun    same attr again: ack table suppresses re-upload, the
#                 scheduler cache-hits — zero new builder executions
#   attach        `rio build --attach <id>` replays the completed
#                 build's event stream from seq 0
#   flake-mode    `rio build path:...#attr` (no --file): the
#                 parseFlakeRef → lockFlake → callFlake path. The dep
#                 consumes two streamed (CAS-read) source roots: a
#                 fetched `path:` input referenced as a whole tree and
#                 a `builtins.toFile` single file. Distinct drv names
#                 so it never cache-hits the file-mode runs.
#   cached-failure-replay
#                 failing dep poisons ([poison] threshold=1 via env);
#                 three priming submissions exhaust the resubmit-reset
#                 budget; the 4th fail-fasts and the client must name
#                 the poisoned CULPRIT and replay its original log —
#                 default 20-line tail, full under -L, and the
#                 persisted reason text for a culprit that produced no
#                 output
#   rio-log       `rio log <drv>` (no --build) prints the culprit's full
#                 stored log on stdout; --build pins a build and
#                 --log-lines tails; an unknown drv exits non-zero
#
# Native-path purity: the client never gets an SSH key and the
# gateway's authorized_keys only holds the boot placeholder — nothing
# in this scenario CAN ride ssh-ng (asserted).
#
# Auth wiring: scheduler + store verify tenant JWTs against the
# lib/jwt-keys.nix test pubkey (RIO_JWT__KEY_PATH / [jwt] key_path in
# default.nix's fixture args); the testScript mints a JWT for the
# fixture-seeded 'vmtest' tenant's UUID with the matching seed. The
# defaultTenant narinfo trigger attributes every registered path to
# vmtest, so client uploads, builder castore reads (HMAC assignment
# token rung) and the client's --fetch read (JWT rung) all resolve to
# one tenant.
{
  pkgs,
  common,
  fixture,
  rioEval,
}:
let
  jwtKeys = import ../lib/jwt-keys.nix;

  rio = "${common.rio-workspace}/bin/rio";
  rioEvalBin = "${rioEval}/bin/rio-eval";
  fixtureNix = "${../lib/derivations/build-client.nix}";

  # Flake-mode fixture: same dep→consumer chain as fixtureNix, but
  # behind a flake `outputs` so the path that does parseFlakeRef →
  # lockFlake → callFlake is exercised. bb and src are staged INSIDE
  # the flake dir so pure-eval can reference them via `self`. Two
  # inputs reach the eval store as STREAMED ingests (no origin tree on
  # disk), so the dep exercises the coordinator's CAS-read source
  # upload for both root kinds: `patchesDir` is a fetched (`path:`)
  # non-flake input referenced as a whole tree (streamed directory
  # root), and `noteFile` is `builtins.toFile` text (streamed
  # single-file root — the same shape as a .patch copied out of a
  # lazily fetched nixpkgs input).
  flakeFixture = pkgs.writeText "flake.nix" ''
    {
      inputs.patches = {
        url = "path:/tmp/work-patches";
        flake = false;
      };
      outputs = { self, patches }:
        let
          bb = "''${self}/bb";
          src = "''${self}/src";
          sh = "''${bb}/bin/sh";
          bbx = "''${bb}/bin/busybox";
          noteFile = builtins.toFile "rio-bc-tofile-note" "rio-bc-tofile-v1\n";
          mkDrv = name: script: extra:
            derivation ({
              inherit name;
              system = "x86_64-linux";
              builder = sh;
              args = [ "-c" script ];
            } // extra);
          dep = mkDrv "rio-bc-flake-dep" '''
            ''${bbx} mkdir -p $out
            ''${bbx} cat ''${src}/data.txt > $out/data
            ''${bbx} cat $noteFile >> $out/data
            ''${bbx} cat $patchesDir/note.patch >> $out/data
            ''${bbx} echo rio-bc-flake-dep-built >> $out/data
          ''' { inherit noteFile; patchesDir = patches; };
        in {
          packages.x86_64-linux.consumer = mkDrv "rio-bc-flake-consumer" '''
            ''${bbx} mkdir -p $out
            ''${bbx} cat ''${dep}/data > $out/summary
            ''${bbx} echo rio-bc-flake-consumer-built >> $out/summary
          ''' { };
        };
    }
  '';

  # PyJWT for signing the test tenant token (same pattern as
  # substitute.nix). cryptography provides the ed25519 backend PyJWT
  # uses for EdDSA.
  pyWithJwt = pkgs.python3.withPackages (
    ps: with ps; [
      pyjwt
      cryptography
    ]
  );

  # Sign a JWT for the tenant UUID in argv[1] with the jwt-keys.nix
  # test seed. PyJWT's EdDSA wants a PEM private key, so wrap the raw
  # seed via cryptography.
  signJwt = pkgs.writeScript "sign-jwt" ''
    #!${pyWithJwt}/bin/python3
    import sys, time, base64, jwt
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    seed = base64.b64decode("${jwtKeys.seedB64}")
    sk = Ed25519PrivateKey.from_private_bytes(seed)
    now = int(time.time())
    claims = {"sub": sys.argv[1], "iat": now, "exp": now + 3600, "jti": "vm-bc-test"}
    print(jwt.encode(claims, sk, algorithm="EdDSA"))
  '';
in
pkgs.testers.runNixOSTest {
  name = "rio-build-client";
  skipTypeCheck = true;

  # ~90s boot + eval-parent cold start + 2 tiny builds at tick=2s +
  # warm rerun + attach + flake-mode (2 more builds) + the
  # cached-failure-replay leg (3 priming invocations of 2 failing
  # builds each, then 3 cheap fail-fast invocations). Generous for TCG.
  globalTimeout = 1200 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.mkBootstrap {
      inherit fixture;
      withSsh = false;
    }}

    # ══════════════════════════════════════════════════════════════════
    # Tenant JWT: the fixture seeded the 'vmtest' tenant row; mint a
    # token whose sub is its UUID (scheduler require_tenant and the
    # store's castore-door JWT rung both key on it).
    # ══════════════════════════════════════════════════════════════════
    tid = psql(control, "SELECT tenant_id FROM tenants WHERE tenant_name = 'vmtest'")
    assert tid, "vmtest tenant row missing — rio-seed-tenant did not run?"
    token = control.succeed(f"${signJwt} {tid}").strip()
    client.succeed(f"umask 077 && echo '{token}' > /tmp/tenant.jwt")

    # ══════════════════════════════════════════════════════════════════
    # Stage the eval inputs at the absolute paths the fixture file
    # references. busybox is copied OUT of the client store so the
    # eval parent ingests it as an ordinary directory source; backdate
    # past the racy-fingerprint slack (same as rio-eval-smoke).
    # ══════════════════════════════════════════════════════════════════
    client.succeed("mkdir -p /tmp/work/src /var/lib/rio/cov")
    client.succeed("echo rio-bc-src-v1 > /tmp/work/src/data.txt")
    client.succeed(
        "cp -r ${common.busybox}/. /tmp/work/bb && chmod -R u+w /tmp/work/bb"
    )
    # Single-file and symlink source roots consumed by the dep drv. The
    # symlink dangles on purpose: only its target string is read.
    client.succeed("echo rio-bc-note-v1 > /tmp/work/note.patch")
    client.succeed("ln -s rio-bc-symlink-target /tmp/work/link")
    client.succeed("find /tmp/work -exec touch -h -d '1 hour ago' {} +")

    rio_env = (
        "RIO_SCHEDULER_ADDR=control:9001 "
        "RIO_STORE_ADDR=control:9002 "
        "RIO_TENANT_TOKEN_PATH=/tmp/tenant.jwt "
        "RIO_EVAL_PARENT=${rioEvalBin} "
        "RIO_CAS_ROOT=/tmp/rio-cas "
        "${common.covShellEnv}"
    )

    def rio_build(args, log_suffix, dump_on_fail=True):
        rc, out = client.execute(
            f"timeout 300 env {rio_env} ${rio} build {args} "
            f"2>/tmp/rio-stderr-{log_suffix}.log"
        )
        if rc != 0 and dump_on_fail:
            print(f"--- rio stderr ({log_suffix}) ---")
            client.execute(f"cat /tmp/rio-stderr-{log_suffix}.log >&2")
            print("--- worker rio-builder journal tail ---")
            worker.execute("journalctl -u rio-builder --no-pager | tail -n 100 >&2")
            print("--- control rio-scheduler journal tail ---")
            control.execute("journalctl -u rio-scheduler --no-pager | tail -n 60 >&2")
            print("--- control rio-store journal tail ---")
            control.execute("journalctl -u rio-store --no-pager | tail -n 40 >&2")
        return rc, out

    # ══════════════════════════════════════════════════════════════════
    with subtest("cold-build: rio build end-to-end against the cluster"):
        rc, out = rio_build(
            "consumer -f ${fixtureNix} --out-link /tmp/result", "cold"
        )
        print(out)
        assert rc == 0, f"rio build failed (rc={rc}):\n{out}"

        # BuildEvent progression rendered on stderr (the status surface;
        # stdout carries result paths only): dispatch states for a
        # really-executed build, then the terminal completion line.
        err = client.succeed("cat /tmp/rio-stderr-cold.log")
        for needle in ("queued", "building", "built", "completed:"):
            assert needle in err, f"missing {needle!r} in client stderr:\n{err}"
        assert "consumer: built /nix/store/" in out, out
        assert "fetched to" in out, out

        # The out-link resolves into the client CAS (the native fetch
        # path), not the client's /nix/store.
        link = client.succeed("readlink /tmp/result").strip()
        assert "/tmp/rio-cas/fetched/" in link, f"out-link points at {link!r}"

        # Content round-trip: src marker flowed through the dep build,
        # the consumer read it through the castore lower on the worker,
        # and --fetch (narHash-verified) brought it back. The note and
        # symlink markers prove single-file and symlink source roots
        # were uploaded and materialized for the worker.
        summary = client.succeed("cat /tmp/result/summary")
        for needle in (
            "rio-bc-src-v1",
            "rio-bc-note-v1",
            "rio-bc-symlink-target",
            "rio-bc-dep-built",
            "rio-bc-consumer-built",
        ):
            assert needle in summary, (
                f"{needle!r} missing from fetched output:\n{summary}"
            )

        # Both drvs executed on the worker (no ssh-ng involved: the
        # client has no SSH key and the gateway only holds the boot
        # placeholder, so the native gRPC path is the only one open).
        n = journal_builds_succeeded(worker)
        assert n >= 2, f"expected >=2 worker builds, journal shows {n}"
        client.fail("test -f /root/.ssh/id_ed25519")

        # Drv blobs landed via PutDrvBlobs and are bound to the tenant.
        nblobs = int(psql(control, "SELECT count(*) FROM drv_blobs"))
        assert nblobs >= 2, f"expected >=2 drv_blobs rows, got {nblobs}"
        nbound = int(psql(
            control,
            f"SELECT count(*) FROM drv_blob_tenants WHERE tenant_id = '{tid}'",
        ))
        assert nbound >= 2, f"expected >=2 tenant-bound drv blobs, got {nbound}"

    # ══════════════════════════════════════════════════════════════════
    with subtest("warm-rerun: ack short-circuit + scheduler cache-hit, no re-execution"):
        before = journal_builds_succeeded(worker)
        rc, out = rio_build("consumer -f ${fixtureNix}", "warm")
        print(out)
        assert rc == 0, f"warm rio build failed (rc={rc}):\n{out}"
        assert "consumer: built /nix/store/" in out, out
        after = journal_builds_succeeded(worker)
        assert after == before, (
            f"warm resubmit re-executed builds: {before} -> {after}"
        )

    # ══════════════════════════════════════════════════════════════════
    with subtest("attach: replay a completed build's event stream"):
        bid = psql(
            control,
            "SELECT build_id FROM builds WHERE status = 'succeeded' "
            "ORDER BY submitted_at ASC LIMIT 1",
        )
        assert bid, "no succeeded builds row to attach to"
        rc, out = rio_build(f"--attach {bid}", "attach")
        print(out)
        assert rc == 0, f"--attach failed (rc={rc}):\n{out}"
        assert "completed" in out, f"attach output missing completion:\n{out}"

    # ══════════════════════════════════════════════════════════════════
    with subtest("flake-mode: rio build path:...#attr (no --file)"):
        # Stage the flake with bb/src inside so pure-eval can reference
        # them via self, plus the fetched `patches` input it consumes
        # (a path: input — its content reaches the eval store as a
        # streamed ingest and uploads via the coordinator's CAS-read
        # path). Distinct drv names so this leg never cache-hits the
        # file-mode runs.
        client.succeed("mkdir -p /tmp/work-flake/src /tmp/work-patches")
        client.succeed("cp -r /tmp/work/bb /tmp/work-flake/bb")
        client.succeed("echo rio-bc-src-v1 > /tmp/work-flake/src/data.txt")
        client.succeed("cp ${flakeFixture} /tmp/work-flake/flake.nix")
        client.succeed("echo rio-bc-fetched-note-v1 > /tmp/work-patches/note.patch")
        client.succeed(
            "find /tmp/work-flake /tmp/work-patches -exec touch -h -d '1 hour ago' {} +"
        )
        before = journal_builds_succeeded(worker)
        rc, out = rio_build(
            "path:/tmp/work-flake#packages.x86_64-linux.consumer "
            "--out-link /tmp/result-flake",
            "flake",
        )
        print(out)
        assert rc == 0, f"flake-mode rio build failed (rc={rc}):\n{out}"
        assert "consumer: built /nix/store/" in out, out
        summary = client.succeed("cat /tmp/result-flake/summary")
        for needle in (
            "rio-bc-src-v1",
            "rio-bc-tofile-v1",
            "rio-bc-fetched-note-v1",
            "rio-bc-flake-dep-built",
            "rio-bc-flake-consumer-built",
        ):
            assert needle in summary, (
                f"{needle!r} missing from flake-mode output:\n{summary}"
            )
        after = journal_builds_succeeded(worker)
        assert after >= before + 2, (
            f"expected >=2 new worker builds for flake leg, {before} -> {after}"
        )

    # ══════════════════════════════════════════════════════════════════
    with subtest("cached-failure-replay: fail-fast names the culprit and replays its log"):
        # Three priming submissions: each fails the loud and the silent
        # dep live (poison threshold 1 via RIO_POISON__THRESHOLD), and
        # each resubmission consumes one resubmit-reset cycle (limit 2).
        # The 4th submission of either root fail-fasts at merge.
        for i in range(3):
            rc, out = rio_build(
                "failingConsumer silentConsumer -f ${fixtureNix}",
                f"prime-{i}",
                dump_on_fail=False,
            )
            assert rc != 0, f"priming run {i} should fail:\n{out}"

        # 4th submission of the loud root: no execution runs; the client
        # must attribute the failure to the poisoned dep (not the
        # consumer it submitted) and replay the tail of its original log.
        rc, out = rio_build(
            "failingConsumer -f ${fixtureNix} --log-lines 20",
            "failfast",
            dump_on_fail=False,
        )
        assert rc != 0, f"fail-fast run should fail:\n{out}"
        err = client.succeed("cat /tmp/rio-stderr-failfast.log")
        assert "failed previously" in err, f"missing failure replay header:\n{err}"
        assert "rio-bc-fail-dep" in err, f"culprit dep not named:\n{err}"
        assert "rio-bc-fail-marker line 30" in err, f"tail content missing:\n{err}"
        assert "rio-bc-fail-marker line 5" not in err, (
            f"default 20-line tail must not include early lines:\n{err}"
        )

        # -L / --print-build-logs: the full original log.
        rc, out = rio_build(
            "failingConsumer -f ${fixtureNix} -L",
            "failfast-full",
            dump_on_fail=False,
        )
        assert rc != 0, f"-L fail-fast run should fail:\n{out}"
        err = client.succeed("cat /tmp/rio-stderr-failfast-full.log")
        assert "rio-bc-fail-marker line 5" in err, f"-L must replay the full log:\n{err}"

        # The silent culprit produced no log lines: the client prints the
        # persisted failure reason instead of a (nonexistent) tail.
        rc, out = rio_build(
            "silentConsumer -f ${fixtureNix}",
            "failfast-silent",
            dump_on_fail=False,
        )
        assert rc != 0, f"silent fail-fast run should fail:\n{out}"
        err = client.succeed("cat /tmp/rio-stderr-failfast-silent.log")
        assert "failed previously" in err, f"missing failure replay header:\n{err}"
        assert "rio-bc-fail-silent" in err, f"silent culprit not named:\n{err}"
        assert "rio-bc-fail-marker" not in err, (
            f"silent run must not replay the loud fixture's log:\n{err}"
        )

    # ══════════════════════════════════════════════════════════════════
    with subtest("rio log: stored-log read by drv path"):
        import re

        # The fail-fast stderr names the culprit's full drv path — the
        # form a user would copy into `rio log`.
        err = client.succeed("cat /tmp/rio-stderr-failfast.log")
        m = re.search(r"/nix/store/\S+-rio-bc-fail-dep\.drv", err)
        assert m, f"culprit drv path not found in fail-fast stderr:\n{err}"
        culprit_drv = m.group(0)

        # Drv-only form: the scheduler resolves the most recent execution
        # among this tenant's builds; the full raw log lands on stdout.
        rc, out = client.execute(f"env {rio_env} ${rio} log {culprit_drv}")
        assert rc == 0, f"rio log failed (rc={rc}):\n{out}"
        assert "rio-bc-fail-marker line 5" in out, f"full log missing early lines:\n{out}"
        assert "rio-bc-fail-marker line 30" in out, f"full log missing tail lines:\n{out}"

        # Pinned-build form + tail.
        bid = psql(
            control,
            "SELECT build_id FROM builds WHERE status = 'failed' "
            "AND error_summary LIKE '%rio-bc-fail-dep%' "
            "ORDER BY submitted_at DESC LIMIT 1",
        )
        assert bid, "no failed build naming the loud culprit"
        rc, out = client.execute(
            f"env {rio_env} ${rio} log {culprit_drv} --build {bid} --log-lines 5"
        )
        assert rc == 0, f"rio log --build failed (rc={rc}):\n{out}"
        assert "rio-bc-fail-marker line 30" in out, f"tail missing last line:\n{out}"
        assert "rio-bc-fail-marker line 20" not in out, (
            f"--log-lines 5 must not include early lines:\n{out}"
        )

        # A drv path nothing in this tenant ever built exits non-zero.
        rc, out = client.execute(
            f"env {rio_env} ${rio} log "
            "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-rio-bc-nope.drv"
        )
        assert rc != 0, f"bogus drv must exit non-zero:\n{out}"

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
