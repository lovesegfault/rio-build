# xfstests ports against the castore-FUSE (ADR-022 §2).
#
# Architecture: the testScript is a thin harness — it builds the
# fixture through the gateway (full ingest path), assembles the
# production castore-FUSE mount (`spike_mountd_client serve-castore`,
# same harness as scenarios/castore-fuse.nix), and then runs
# `xfstests_runner` (rio-builder/src/bin/xfstests_runner/), which owns
# EVERY filesystem assertion as direct Rust syscall probes with
# deterministic errnos. No assertion logic lives in Python: shell
# probes proved unreliable (GNU rm prompts on write-protected files
# instead of failing) and cannot distinguish errnos exactly.
#
# Selection, tiering, and the old-FUSE assumptions that were dropped:
# rio-builder/tests/xfstests_port/PLAN.md. The runner's check names
# carry their xfstests origins (generic_257_*, generic_050_*, ...);
# `xfstests_runner --list` enumerates them.
#
# Oracle: the JSON manifest below — the same constants as the fixture
# derivation lib/derivations/xfstests-tree.nix, literal for literal.
# The manifest is the runner's only oracle; behavioral assertions
# always target the FUSE mountpoint (the system under test), never the
# backing cache or any host-side copy. Any drift between manifest and
# fixture fails the runner loudly with a content mismatch.
#
# The dep is built in-VM through the gateway, so the asserted tree took
# the full ingest path: NAR upload → castore Directory DAG/blobs → DAG
# prefetch → FUSE. A consumer build dispatched to the worker covers the
# overlay-lowerdir readdir leg; its output is handed to the runner.
{
  pkgs,
  common,
}:
let
  inherit (pkgs) lib;
  drvs = import ../lib/derivations.nix { inherit pkgs; };

  # Byte-distinct lookalike names (generic/453): NFC e-acute (c3 a9)
  # vs NFD e + combining acute (65 cc 81). Built via fromJSON \u
  # escapes so the byte sequences are explicit and no editor or
  # formatter can silently normalize one into the other. They MUST
  # match the octal printf escapes in xfstests-tree.nix
  # (\303\251 = c3 a9, \314\201 = cc 81).
  nfcName = builtins.fromJSON ''"caf\u00e9"'';
  nfdName = builtins.fromJSON ''"cafe\u0301"'';

  toolScript = "#!/bin/sh\necho tool-ok\n";

  # Expected-content manifest for xfstests_runner --manifest. Mirrors
  # lib/derivations/xfstests-tree.nix; keep the two in sync (the VM
  # test fails on any divergence, so drift cannot survive a run).
  manifest = pkgs.writeText "rio-xfstests-manifest.json" (
    builtins.toJSON {
      root_suffix = "-rio-xfstests-dep";
      fstype = "fuse.rio-castore";
      dirs = [
        "bin"
        "data"
        "dir200"
        "names"
        "links"
        "dup-a"
        "dup-b"
        "nest"
        "nest/p1"
        "nest/p2"
        "nest/p1/shared"
        "nest/p2/shared"
      ];
      files = [
        {
          path = "bin/tool";
          content = toolScript;
          executable = true;
        }
        {
          path = "data/tool.sh";
          content = toolScript;
          executable = false;
        }
        {
          path = "data/small.txt";
          content = "rio-xfstests-small\n";
          executable = false;
        }
        {
          path = "data/empty";
          content = "";
          executable = false;
        }
        {
          path = "names/a b";
          content = "space-name\n";
          executable = false;
        }
        {
          path = "names/${nfcName}";
          content = "nfc-content\n";
          executable = false;
        }
        {
          path = "names/${nfdName}";
          content = "nfd-content\n";
          executable = false;
        }
        {
          # NAME_MAX (255-byte) name.
          path = "names/${lib.strings.replicate 255 "n"}";
          content = "longname-content\n";
          executable = false;
        }
        {
          path = "dup-a/same.txt";
          content = "rio-xfstests-dedup\n";
          executable = false;
        }
        {
          path = "dup-b/same.txt";
          content = "rio-xfstests-dedup\n";
          executable = false;
        }
        {
          path = "nest/p1/shared/payload.txt";
          content = "rio-xfstests-nested-dedup\n";
          executable = false;
        }
        {
          path = "nest/p2/shared/payload.txt";
          content = "rio-xfstests-nested-dedup\n";
          executable = false;
        }
        {
          path = "nest/p1/only-p1.txt";
          content = "p1-marker\n";
          executable = false;
        }
        {
          path = "nest/p2/only-p2.txt";
          content = "p2-marker\n";
          executable = false;
        }
      ];
      symlinks = [
        {
          path = "links/rel";
          target = "../data/small.txt";
        }
        {
          path = "links/longtarget";
          target = lib.strings.replicate 900 "x";
        }
        {
          path = "links/dangling";
          target = "/rio-xfstests-no-such-target";
        }
        {
          path = "links/loop1";
          target = "loop2";
        }
        {
          path = "links/loop2";
          target = "loop1";
        }
      ]
      # 41-link chain: chain0..chain39 -> chain<i+1>, chain40 -> the
      # small file. chain0 exceeds MAXSYMLINKS=40 (ELOOP), chain1 does
      # not (resolves). Must match the while-loop in xfstests-tree.nix.
      ++ lib.genList (i: {
        path = "links/chain${toString i}";
        target = "chain${toString (i + 1)}";
      }) 40
      ++ [
        {
          path = "links/chain40";
          target = "../data/small.txt";
        }
      ];
      seq_dir = {
        path = "dir200";
        count = 200;
      };
      big_file = {
        path = "data/big.bin";
        size = 1300003;
        pattern_line = "rio-xfstests-payload-0123456789abcdef";
      };
    }
  );
in
{
  mkTest =
    { fixture }:
    let
      inherit (fixture) gatewayHost;
    in
    pkgs.testers.runNixOSTest {
      name = "rio-castore-xfstests";
      skipTypeCheck = true;
      # Boot (3 VMs) + seed + the dep/consumer builds + DAG prefetch +
      # the runner (cold streaming fill of the 1.3 MiB blob, ~250 small
      # reads/stats, 8-way concurrent readers).
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

        STAGING_QUOTA = 268435456
        STREAM_THRESHOLD = 65536

        # ══ Fixture build (full ingest path) + the overlay-lowerdir leg ══
        # The consumer dispatches to the worker and lists the dep through
        # overlay-over-castore; the runner owns the assertions on its
        # output. Python only extracts the dep path it needs to assemble
        # the mount.
        consumer_out = build("${drvs.xfstestsTree}", attr="consumer")
        client.succeed(
            f"nix store cat --store 'ssh-ng://${gatewayHost}' {consumer_out}"
            " > /tmp/xfs-consumer.txt"
        )
        dep_path = client.succeed("head -n1 /tmp/xfs-consumer.txt").strip()
        print(f"xfstests dep path: {dep_path}")

        # ══ FUSE/mountd machine setup (client doubles as the builder node) ══
        client.succeed("mkdir -p /var/rio/castore /var/rio/staging /var/rio/cache /var/rio/chunks")
        client.succeed("truncate -s 1G /var/rio-staging.img && mkfs.xfs -q /var/rio-staging.img")
        client.succeed("mount -o loop,prjquota /var/rio-staging.img /var/rio/staging")
        client.succeed(
            "${common.rio-workspace}/bin/rio-mountd --socket /run/rio-mountd.sock"
            " --castore-dir /var/rio/castore --staging-dir /var/rio/staging"
            " --cache-dir /var/rio/cache --chunks-dir /var/rio/chunks"
            f" --staging-quota-bytes {STAGING_QUOTA} --allowed-gid 990"
            " >>/var/log/rio-mountd.log 2>&1 & echo $! > /run/rio-mountd.pid"
        )
        client.wait_until_succeeds("test -S /run/rio-mountd.sock", timeout=15)

        # ══ Tenancy: DirectoryService resolves the caller's tenant from the
        # assignment token and joins path_tenants on sha256(store_path). ══
        tenant_id = psql(
            ${gatewayHost},
            "INSERT INTO tenants (tenant_name) VALUES ('castore-xfs') RETURNING tenant_id",
        )
        if not tenant_id:
            raise Exception("tenant INSERT returned no tenant_id")
        path_hash = hashlib.sha256(dep_path.encode()).hexdigest()
        psql(
            ${gatewayHost},
            "INSERT INTO path_tenants (store_path_hash, tenant_id) "
            f"VALUES (decode('{path_hash}', 'hex'), '{tenant_id}') "
            "ON CONFLICT DO NOTHING",
        )

        # Assignment token: base64url(claims).base64url(hmac_sha256(key, claims)),
        # signed with the fixture's ASSIGNMENT key — same shape as
        # scenarios/castore-fuse.nix and rio_auth::hmac::AssignmentClaims.
        key = ${gatewayHost}.succeed("cat ${fixture.hmacKeys}/hmac.key").encode()
        for suffix in (b"\r\n", b"\n"):
            if key.endswith(suffix):
                key = key[: -len(suffix)]
                break
        claims = json.dumps(
            {
                "executor_id": "vm-xfstests",
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

        # ══ serve-castore: the production session, one build-id "xfs" ══════
        SERVE = (
            "${common.rio-workspace}/bin/spike_mountd_client"
            " --socket /run/rio-mountd.sock serve-castore"
            " --store-addr ${gatewayHost}:9002"
            " --assignment-token-file /tmp/assignment.token"
            " --cache-dir /var/rio/cache --chunks-dir /var/rio/chunks"
            " --staging-root /var/rio/staging"
            f" --stream-threshold {STREAM_THRESHOLD}"
            f" --build-id xfs --store-path {dep_path}"
            " --ready-file /tmp/xfs.ready"
        )

        with subtest("mount: serve-castore mounts the xfstests dep"):
            client.succeed(
                "runuser -u builder -- bash -c "
                f"'( {SERVE} >/tmp/xfs.log 2>&1; echo $? >/tmp/xfs.exit ) >/dev/null 2>&1 &'"
            )
            client.wait_until_succeeds(
                "test -f /tmp/xfs.ready -o -f /tmp/xfs.exit", timeout=300
            )
            rc, _ = client.execute("test -f /tmp/xfs.ready")
            if rc != 0:
                log = client.succeed("cat /tmp/xfs.log")
                raise Exception(f"serve-castore exited before ready:\n{log}")

        # ══ The ports: every filesystem assertion lives in the Rust runner.
        # Check names + origins: `xfstests_runner --list`; failures print
        # as `FAIL <name> [<origin>]: <error>` lines below. ══════════════════
        with subtest("xfstests runner: ported checks against the live mount"):
            rc, out = client.execute(
                "${common.rio-workspace}/bin/xfstests_runner"
                " --mount /var/rio/castore/xfs"
                " --manifest ${manifest}"
                " --cache-dir /var/rio/cache"
                " --consumer-output /tmp/xfs-consumer.txt"
                " --probe-uid 1000 --probe-gid 990"
                " --second-uid 1001 --second-gid 991",
                timeout=None,
            )
            print(out)
            if rc != 0:
                serve_log = client.succeed("cat /tmp/xfs.log")
                print(f"=== serve-castore log ===\n{serve_log}\n=== end ===")
                dump_all_logs([${gatewayHost}, worker])
                raise Exception(f"xfstests runner exited {rc} (= number of failed checks)")

        # ══ Teardown (harness lifecycle): SIGTERM must exit 0 and reap the
        # mount + staging dir — and in coverage mode it is what flushes the
        # serve-castore profraws. ════════════════════════════════════════════
        with subtest("teardown: SIGTERM exits 0, mountd reaps the mount, daemon exits cleanly"):
            client.succeed("pkill -TERM -u builder spike_mountd || true")
            client.wait_until_succeeds("test -f /tmp/xfs.exit", timeout=60)
            rc = client.succeed("cat /tmp/xfs.exit").strip()
            if rc != "0":
                log = client.succeed("cat /tmp/xfs.log")
                raise Exception(f"serve-castore exit status {rc} after SIGTERM; log:\n{log}")
            client.wait_until_succeeds(
                "! mountpoint -q /var/rio/castore/xfs"
                " && test ! -e /var/rio/castore/xfs"
                " && test ! -e /var/rio/staging/xfs",
                timeout=30,
            )
            client.succeed("kill $(cat /run/rio-mountd.pid)")
            client.wait_until_succeeds(
                "! kill -0 $(cat /run/rio-mountd.pid) 2>/dev/null", timeout=15
            )

        ${common.collectCoverage fixture.pyNodeVars}
      '';
    };
}
