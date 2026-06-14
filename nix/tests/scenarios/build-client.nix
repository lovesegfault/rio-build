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
#   cold-build    eval → upload (chunked dir sources + drv blobs) →
#                 digest submission → worker executes dep+consumer →
#                 event progression on stdout → fetched output content
#                 byte-checked → drv blobs tenant-bound in PG
#   warm-rerun    same attr again: ack table suppresses re-upload, the
#                 scheduler cache-hits — zero new builder executions
#   attach        `rio build --attach <id>` replays the completed
#                 build's event stream from seq 0
#   flake-mode    `rio build path:...#attr` (no --eval-file): the
#                 parseFlakeRef → lockFlake → callFlake path,
#                 hermetic flake (no inputs), distinct drv names so it
#                 never cache-hits the file-mode runs
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
  # lockFlake → callFlake is exercised. Hermetic (no inputs); bb and
  # src are staged INSIDE the flake dir so pure-eval can reference
  # them via `self`.
  flakeFixture = pkgs.writeText "flake.nix" ''
    {
      inputs = { };
      outputs = { self }:
        let
          bb = "''${self}/bb";
          src = "''${self}/src";
          sh = "''${bb}/bin/sh";
          bbx = "''${bb}/bin/busybox";
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
            ''${bbx} echo rio-bc-flake-dep-built >> $out/data
          ''' { };
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
  # warm rerun + attach + flake-mode (2 more builds). Generous for TCG.
  globalTimeout = 720 + common.covTimeoutHeadroom;

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
    client.succeed("find /tmp/work -exec touch -h -d '1 hour ago' {} +")

    rio_env = (
        "RIO_SCHEDULER_ADDR=control:9001 "
        "RIO_STORE_ADDR=control:9002 "
        "RIO_TENANT_TOKEN_PATH=/tmp/tenant.jwt "
        "RIO_EVAL_PARENT=${rioEvalBin} "
        "RIO_CAS_ROOT=/tmp/rio-cas "
        "${common.covShellEnv}"
    )

    def rio_build(args, log_suffix):
        rc, out = client.execute(
            f"timeout 300 env {rio_env} ${rio} build {args} "
            f"2>/tmp/rio-stderr-{log_suffix}.log"
        )
        if rc != 0:
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
            "consumer --eval-file ${fixtureNix} --out-link /tmp/result", "cold"
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
        # and --fetch (narHash-verified) brought it back.
        summary = client.succeed("cat /tmp/result/summary")
        for needle in ("rio-bc-src-v1", "rio-bc-dep-built", "rio-bc-consumer-built"):
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
        rc, out = rio_build("consumer --eval-file ${fixtureNix}", "warm")
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
    with subtest("flake-mode: rio build path:...#attr (no --eval-file)"):
        # Stage a hermetic flake (no inputs) with bb/src inside so
        # pure-eval can reference them via self. Distinct drv names
        # so this leg never cache-hits the file-mode runs.
        client.succeed("mkdir -p /tmp/work-flake/src")
        client.succeed("cp -r /tmp/work/bb /tmp/work-flake/bb")
        client.succeed("echo rio-bc-src-v1 > /tmp/work-flake/src/data.txt")
        client.succeed("cp ${flakeFixture} /tmp/work-flake/flake.nix")
        client.succeed("find /tmp/work-flake -exec touch -h -d '1 hour ago' {} +")
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

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
