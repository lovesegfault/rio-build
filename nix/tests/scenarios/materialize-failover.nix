# Materialization under leader failover (substitution-replacement
# Phase B, design §8-B vm-materialization-failover; plan T-3.3).
#
# What the standalone scenarios cannot prove: materialization jobs are
# PG-authoritative state that survives the death of the scheduler leader
# that created them. Jobs are in flight when the leader dies; the
# standby's recovery + re-probe completes the build through jobs. The
# scenario asserts the client-visible outcome (build succeeded, all
# leaf paths present in the store, no stuck node) plus the job-row
# mechanism assertions.
#
# ── Recovery-limitation note (Wave-3-honest assertions) ──────────────
# The new leader's in-memory job view is NOT rebuilt at recovery on this
# tree (the Phase A "not recovery-safe" gap; Wave 4's T-4.3 adds the
# rebuild). What the failover subtest asserts is what the CURRENT
# implementation guarantees:
#   - job rows are never lost (PG-authoritative; count and job_ids
#     identical across the failover),
#   - in-flight executor reports against old exec_ids are consumed by
#     the new leader (the claims-floor fence admits them),
#   - unclaimed jobs become claimable again once the new leader's
#     dispatch probe re-probes their nodes (the create-job dedup re-feeds
#     the in-memory view from the existing PG rows),
#   - the build completes.
# T-4.3 extends this scenario's property to "claimable immediately after
# recovery, no re-probe needed" by rebuilding the view; the assertions
# here are written against the end state so that change only makes them
# pass sooner.
#
# Markers live at the wiring point (nix/tests/default.nix).
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) ns nsStore;

  rioCli = "${common.rio-workspace}/bin/rio-cli";
  grpcurl = "${pkgs.grpcurl}/bin/grpcurl";
  protoset = import ../lib/protoset.nix { inherit pkgs; };
  jwtKeys = import ../lib/jwt-keys.nix;

  pyWithJwt = pkgs.python3.withPackages (
    ps: with ps; [
      pyjwt
      cryptography
    ]
  );
  signJwt = pkgs.writeScript "sign-jwt-failover" ''
    #!${pyWithJwt}/bin/python3
    import sys, time, base64, jwt
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    seed = base64.b64decode("${jwtKeys.seedB64}")
    sk = Ed25519PrivateKey.from_private_bytes(seed)
    now = int(time.time())
    claims = {"sub": sys.argv[1], "iat": now, "exp": now + 3600, "jti": "vm-failover"}
    print(jwt.encode(claims, sk, algorithm="EdDSA"))
  '';

  # 10 independent substitutable leaves for the failover subtest.
  # Input-addressed (same busybox + system on upstream_v6 and client)
  # so out-paths match between the cache seed and the submission.
  leafCount = 10;
  failoverLeaves = pkgs.writeText "failover-leaves.nix" ''
    { busybox }:
    let
      sh = "''${busybox}/bin/sh";
      bb = "''${busybox}/bin/busybox";
      mkLeaf = i: derivation {
        name = "rio-failover-leaf-''${toString i}";
        system = builtins.currentSystem;
        builder = sh;
        args = [ "-c" "''${bb} echo failover-leaf-''${toString i}-v1 > $out" ];
      };
    in builtins.genList mkLeaf ${toString leafCount}
  '';
in
pkgs.testers.runNixOSTest {
  name = "rio-materialize-failover";
  skipTypeCheck = true;

  # k3s bring-up ~240 s + cache seed ~20 s + tenant/ssh wiring ~60 s +
  # failover subtest ~120 s (submit, kill, re-acquire, complete) +
  # builder-contention slack.
  globalTimeout = 1500 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.mkBootstrap {
      inherit fixture;
      withSsh = false;
      withSeed = false;
    }}

    # ══════════════════════════════════════════════════════════════════
    # Upstream cache: build + publish the leaves on upstream_v6
    # ══════════════════════════════════════════════════════════════════
    NIX = "nix --extra-experimental-features nix-command"

    upstream_v6.succeed(
        "nix-store --load-db < ${common.busyboxClosure}/registration"
    )
    upstream_v6.succeed(
        f"{NIX} key generate-secret --key-name failover-1 > /tmp/sec && "
        f"{NIX} key convert-secret-to-public < /tmp/sec > /tmp/pub"
    )
    cache_pubkey = upstream_v6.succeed("cat /tmp/pub").strip()

    upstream_v6.succeed(
        "nix-build --no-out-link --option substituters ''' "
        "--arg busybox '(builtins.storePath ${common.busybox})' "
        "${failoverLeaves} 2>&1"
    )
    seed_drv_lines = upstream_v6.succeed(
        "nix-instantiate "
        "--arg busybox '(builtins.storePath ${common.busybox})' "
        "${failoverLeaves} 2>/dev/null"
    ).split()
    leaf_outs = [
        p for p in upstream_v6.succeed(
            "nix-store -q --outputs " + " ".join(seed_drv_lines)
        ).splitlines()
        if "rio-failover-leaf-" in p
    ]
    assert len(leaf_outs) == ${toString leafCount}, (
        f"expected ${toString leafCount} leaf out-paths, got {len(leaf_outs)}"
    )
    leaves_sh = " ".join(leaf_outs)
    upstream_v6.succeed(f"{NIX} store sign --key-file /tmp/sec {leaves_sh}")
    upstream_v6.succeed(
        f"{NIX} copy --no-check-sigs "
        f"--to 'file:///srv?compression=none' {leaves_sh}"
    )
    h0 = leaf_outs[0].removeprefix("/nix/store/").split("-", 1)[0]
    upstream_v6.wait_for_open_port(8080)
    k3s_server.succeed(
        f"curl -sf 'http://upstream-v6:8080/{h0}.narinfo' "
        "| grep -q 'Sig: failover-1:'"
    )

    # The client instantiates the SAME closure (input-addressed -> same
    # drv + output paths) to construct the gRPC submissions.
    client_drvs = sorted(client.succeed(
        "nix-instantiate "
        "--arg busybox '(builtins.storePath ${common.busybox})' "
        "${failoverLeaves} 2>/dev/null"
    ).split())
    leaf_pairs = []
    for d in client_drvs:
        out_path = client.succeed(f"nix-store -q --outputs {d}").strip()
        leaf_pairs.append((d, out_path))
    assert len(leaf_pairs) == ${toString leafCount}
    failover_set = leaf_pairs[0:10]

    # ══════════════════════════════════════════════════════════════════
    # Tenant + upstream + helpers
    # ══════════════════════════════════════════════════════════════════
    pf_open(leader_pod(), 19001, 9001, tag="pf-sched")
    pf_open("svc/rio-store", 19002, 9002, ns="${nsStore}", tag="pf-store")
    k3s_server.succeed(
        "k3s kubectl -n ${ns} get secret rio-service-hmac "
        "-o jsonpath='{.data.service-hmac\\.key}' | base64 -d "
        "> /tmp/service-hmac.key"
    )
    CLI_ENV = (
        "${common.covShellEnv}"
        "RIO_SCHEDULER_ADDR=localhost:19001 "
        "RIO_STORE_ADDR=localhost:19002 "
        "RIO_SERVICE_HMAC_KEY_PATH=/tmp/service-hmac.key "
    )

    def cli(args):
        return k3s_server.succeed(f"{CLI_ENV}${rioCli} {args} 2>&1")

    out = cli("create-tenant failover-tenant")
    m = re.search(r"\(([0-9a-f-]{36})\)", out)
    assert m, f"create-tenant didn't echo a UUID:\n{out}"
    tid = m.group(1)
    tenant_jwt = k3s_server.succeed(f"${signJwt} {tid}").strip()

    out = cli(
        f"upstream add --tenant {tid} "
        "--url http://upstream-v6:8080 --priority 50 "
        f"--trusted-key '{cache_pubkey}' --sig-mode keep"
    )
    assert "added upstream http://upstream-v6:8080" in out, out

    def repoint_leader_pf():
        """Re-resolve the scheduler-leader port-forward (after failovers
        and scheduler rollouts the old forward goes stale)."""
        pf_close("pf-sched")
        pf_open(leader_pod(), 19001, 9001, tag="pf-sched")

    def submit_leaves(unit, pairs):
        """SubmitBuild for a set of independent leaves through the leader
        port-forward (jwtEnabled -> the request must carry the tenant
        JWT). Returns the new build's UUID."""
        n_before = int(psql_k8s(k3s_server, "SELECT count(*) FROM builds"))
        nodes = [
            {"drvPath": d, "drvHash": d,
             "system": "${pkgs.stdenv.hostPlatform.system}",
             "outputNames": ["out"], "expectedOutputPaths": [o]}
            for d, o in pairs
        ]
        payload = json.dumps({"nodes": nodes, "edges": []})
        k3s_server.succeed(f"cat > /tmp/dag-{unit}.json <<'EOF'\n{payload}\nEOF")
        k3s_server.succeed(
            f"systemd-run --unit=dag-{unit} sh -c "
            "'${grpcurl} -plaintext -max-time 600 "
            f'-H "x-rio-tenant-token: {tenant_jwt}" '
            "-protoset ${protoset}/rio.protoset "
            "-d @ "
            "localhost:19001 rio.scheduler.SchedulerService/SubmitBuild "
            f"< /tmp/dag-{unit}.json'"
        )
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${ns} exec rio-postgresql-0 -- "
            "env PGPASSWORD=rio psql -h 127.0.0.1 -U rio rio -qtAc "
            "'SELECT count(*) FROM builds'"
            f" | grep -qx {n_before + 1}",
            timeout=60,
        )
        return psql_k8s(
            k3s_server,
            "SELECT build_id FROM builds ORDER BY submitted_at DESC LIMIT 1",
        )

    def build_status(build_id):
        return psql_k8s(
            k3s_server, f"SELECT status FROM builds WHERE build_id = '{build_id}'"
        )

    def wait_build_status(build_id, want, timeout=180):
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${ns} exec rio-postgresql-0 -- "
            "env PGPASSWORD=rio psql -h 127.0.0.1 -U rio rio -qtAc "
            f"\"SELECT status FROM builds WHERE build_id = '{build_id}'\""
            f" | grep -qx {want}",
            timeout=timeout,
        )

    def jobs_for_set(pairs, where=""):
        """Count of materialization_jobs rows for a leaf set."""
        drvs = ", ".join(f"'{d}'" for d, _ in pairs)
        return int(psql_k8s(
            k3s_server,
            f"SELECT count(*) FROM materialization_jobs WHERE drv_hash IN ({drvs}){where}",
        ))

    def narinfo_for_set(pairs):
        outs = ", ".join(f"'{o}'" for _, o in pairs)
        return int(psql_k8s(
            k3s_server,
            f"SELECT count(*) FROM narinfo WHERE store_path IN ({outs})",
        ))

    def assert_outcome(tag, build_id, pairs):
        """The shared client-visible outcome triple (criterion 1): build
        succeeded, every leaf path present in the store, no node left in
        an in-flight state. Identical text for both flag states."""
        verdict = build_status(build_id)
        assert verdict == "succeeded", f"{tag}: build verdict {verdict!r}, expected succeeded"
        present = narinfo_for_set(pairs)
        assert present == len(pairs), (
            f"{tag}: store end-state: {present}/{len(pairs)} leaf paths present"
        )
        drvs = ", ".join(f"'{d}'" for d, _ in pairs)
        stuck = psql_k8s(
            k3s_server,
            "SELECT count(*) FROM derivations"
            f" WHERE drv_hash IN ({drvs})"
            " AND status NOT IN ('completed', 'skipped')",
        )
        assert stuck == "0", f"{tag}: {stuck} node(s) not terminal-completed"
        print(f"{tag}: outcome OK (succeeded, {present} paths present, no stuck nodes)")

    # ══════════════════════════════════════════════════════════════════
    # Subtest: substitution work survives leader failover
    # ══════════════════════════════════════════════════════════════════
    # 10 jobs created -> some claimed/fetching (netem-slowed upstream)
    # -> the leader dies -> the standby acquires -> job rows survive
    # (PG-authoritative) -> the build completes.
    with subtest("failover: in-flight substitution survives the leader's death"):
        # Slow the upstream so the work is reliably in flight when the
        # leader dies (each fetch ~2 RTTs x 750 ms).
        upstream_v6.succeed("tc qdisc replace dev eth1 root netem delay 750ms")

        repoint_leader_pf()
        old_leader = leader_pod()
        fo_build = submit_leaves("failover", failover_set)

        # All 10 jobs exist (created in the merge transaction).
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${ns} exec rio-postgresql-0 -- "
            "env PGPASSWORD=rio psql -h 127.0.0.1 -U rio rio -qtAc "
            "\"SELECT count(*) FROM materialization_jobs\" | grep -qx 10",
            timeout=60,
        )
        job_ids_before = psql_k8s(
            k3s_server,
            "SELECT job_id FROM materialization_jobs ORDER BY job_id",
        )
        unresolved_before = jobs_for_set(failover_set, " AND state = 'pending'")
        assert unresolved_before >= 1, (
            "non-vacuity: at least one job must still be unresolved when the "
            f"leader dies (got {unresolved_before} pending) — widen the netem "
            "delay if this fires"
        )

        # Kill the leader (force-delete: the le-scenario failover pattern).
        kubectl(f"delete pod {old_leader} --grace-period=0 --force")

        # The standby acquires the lease.
        k3s_server.wait_until_succeeds(
            "h=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
            "-o jsonpath='{.spec.holderIdentity}'); "
            f"test -n \"$h\" && test \"$h\" != '{old_leader}'",
            timeout=90,
        )

        # No job row was lost: same count, same job_ids.
        job_ids_after = psql_k8s(
            k3s_server,
            "SELECT job_id FROM materialization_jobs ORDER BY job_id",
        )
        assert job_ids_after == job_ids_before, (
            f"job rows changed across the failover:\n"
            f"before: {job_ids_before}\nafter: {job_ids_after}"
        )

        # Heal the upstream latency so the post-failover completion is
        # fast, and let the new leader finish the build.
        upstream_v6.succeed("tc qdisc del dev eth1 root || true")
        wait_build_status(fo_build, "succeeded", timeout=300)

        # Mechanism + shared outcome triple.
        resolved = jobs_for_set(failover_set, " AND state = 'resolved_success'")
        assert resolved == 10, (
            f"all 10 jobs must resolve successfully after the failover, got {resolved}"
        )
        assert_outcome("failover", fo_build, failover_set)

    k3s_server.execute("systemctl stop 'dag-*' 2>/dev/null || true")
    pf_close("pf-sched")
    pf_close("pf-store")

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
