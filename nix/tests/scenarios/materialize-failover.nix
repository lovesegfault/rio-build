# Materialization under leader failover + the AS-6 mixed-flag windows
# (substitution-replacement Phase B, design §8-B vm-materialization-failover;
# plan T-3.3).
#
# What the standalone scenarios cannot prove: materialization jobs are
# PG-authoritative state that survives the death of the scheduler leader
# that created them, and the AS-6 deployment-ordering hazard windows
# (scheduler-on/store-off and scheduler-off/store-on) are visible waits
# or no-ops — never strands, never wrongful failures.
#
# ── Both flag states (review eq-1: the OQ7 failover comparison) ──────
#   vm-materialization-failover-k3s       flag-ON: jobs in flight when
#                                         the leader dies; the standby's
#                                         recovery + re-probe completes
#                                         the build through jobs.
#   vm-materialization-failover-walk-k3s  flag-OFF oracle: walks in
#                                         flight when the leader dies;
#                                         the as-built recovery reset +
#                                         re-probe completes the build
#                                         through walks.
# Both attrs assert the SAME client-visible outcome (build succeeded,
# all leaf paths present in the store, no stuck node); the mechanism
# assertions differ per branch (job rows vs walk metrics).
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
  materializationEnabled ? true,
}:
let
  inherit (pkgs) lib;
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

  # 14 independent substitutable leaves: 10 for the failover subtest,
  # 2 for mixed-flag-no-strand, 2 for store-only-noop. Input-addressed
  # (same busybox + system on upstream_v6 and client) so out-paths match
  # between the cache seed and the submission.
  leafCount = 14;
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
  # Distinct derivation names per flag state so the two check attrs never
  # collide and CI logs name the right one.
  name = "rio-materialize-failover" + lib.optionalString (!materializationEnabled) "-walk";
  skipTypeCheck = true;

  # k3s bring-up ~240 s + cache seed ~20 s + tenant/ssh wiring ~60 s +
  # failover subtest ~120 s (submit, kill, re-acquire, complete) +
  # (flag-on only) two mixed-flag subtests with one store rollout and one
  # scheduler rollout each ~120-180 s. The walk attr runs only the
  # failover subtest.
  globalTimeout = 1500 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.mkBootstrap {
      inherit fixture;
      withSsh = false;
      withSeed = false;
    }}

    MATERIALIZATION_ENABLED = ${if materializationEnabled then "True" else "False"}

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
    # Slices per subtest.
    failover_set = leaf_pairs[0:10]
    mixed_set = leaf_pairs[10:12]
    noop_set = leaf_pairs[12:14]

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
    # Subtest 1: substitution work survives leader failover
    # ══════════════════════════════════════════════════════════════════
    # Flag-on: 10 jobs created -> some claimed/fetching (netem-slowed
    # upstream) -> the leader dies -> the standby acquires -> job rows
    # survive (PG-authoritative) -> the build completes.
    # Flag-off: the same submission runs as 10 walks -> the leader dies
    # mid-walk -> the as-built recovery reset returns the nodes to
    # dispatchability -> re-probe re-spawns the walks -> completes.
    with subtest("failover: in-flight substitution survives the leader's death"):
        # Slow the upstream so the work is reliably in flight when the
        # leader dies (each fetch ~2 RTTs x 750 ms).
        upstream_v6.succeed("tc qdisc replace dev eth1 root netem delay 750ms")

        repoint_leader_pf()
        old_leader = leader_pod()
        fo_build = submit_leaves("failover", failover_set)

        if MATERIALIZATION_ENABLED:
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
        else:
            # Flag-off: the walks are in flight (some node Substituting).
            k3s_server.wait_until_succeeds(
                "k3s kubectl -n ${ns} exec rio-postgresql-0 -- "
                "env PGPASSWORD=rio psql -h 127.0.0.1 -U rio rio -qtAc "
                "\"SELECT count(*) FROM derivations WHERE status = 'substituting'\""
                " | grep -qvx 0",
                timeout=60,
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

        if MATERIALIZATION_ENABLED:
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

        # Mechanism per branch + shared outcome triple.
        if MATERIALIZATION_ENABLED:
            resolved = jobs_for_set(failover_set, " AND state = 'resolved_success'")
            assert resolved == 10, (
                f"all 10 jobs must resolve successfully after the failover, got {resolved}"
            )
        else:
            jobs = int(psql_k8s(k3s_server, "SELECT count(*) FROM materialization_jobs"))
            assert jobs == 0, f"flag-off deployment created {jobs} materialization job(s)"
        assert_outcome("failover", fo_build, failover_set)

    # The mixed-flag subtests are flag-state tests by construction (they
    # set the flags themselves), so they run only in the flag-on attr —
    # the -walk oracle exists for the OQ7 failover comparison above.
    if MATERIALIZATION_ENABLED:
        # ══════════════════════════════════════════════════════════════
        # Subtest 2: AS-6 mixed window — creation on, executor off
        # ══════════════════════════════════════════════════════════════
        # Simulates the rollout race the chart's AND-guard cannot close
        # (pod generations diverge during a rollout): the scheduler
        # creates jobs but no store replica claims them. The window must
        # be a visible wait — never a strand, never a failure — and must
        # drain the moment the store executor comes back.
        with subtest("mixed-flag: scheduler-on/store-off is a visible wait, never a strand"):
            kubectl(
                "set env deployment/rio-store RIO_MATERIALIZATION__ENABLED=false",
                ns="${nsStore}",
            )
            kubectl(
                "rollout status deployment/rio-store --timeout=180s",
                ns="${nsStore}",
            )

            repoint_leader_pf()
            mixed_build = submit_leaves("mixed", mixed_set)

            # Jobs are created (scheduler side is on)...
            k3s_server.wait_until_succeeds(
                "k3s kubectl -n ${ns} exec rio-postgresql-0 -- "
                "env PGPASSWORD=rio psql -h 127.0.0.1 -U rio rio -qtAc "
                "\"SELECT count(*) FROM materialization_jobs"
                " WHERE drv_hash IN ("
                + ", ".join(f"'{d}'" for d, _ in mixed_set)
                + ")\" | grep -qx 2",
                timeout=60,
            )
            # ... and stay pending for a full observation window: nothing
            # claims them, the build waits, nothing fails.
            k3s_server.sleep(45)
            pending = jobs_for_set(mixed_set, " AND state = 'pending'")
            assert pending == 2, (
                f"with the store executor off, both jobs must stay pending: {pending}"
            )
            status = build_status(mixed_build)
            assert status == "active", (
                f"the mixed-flag window must be a wait, not a failure: {status!r}"
            )
            # The backlog is operator-visible (the §2.6 re-sourced
            # substituting bucket counts pending unclaimed jobs).
            rc, raw = k3s_server.execute(f"{CLI_ENV}${rioCli} status --json 2>&1")
            backlog = -1
            try:
                backlog = int(json.loads(raw[raw.find("{"):])["substituting_derivations"])
            except (ValueError, KeyError):
                print(f"mixed-flag: rio-cli status unparseable (rc={rc}): {raw[:200]!r}")
            assert backlog >= 2, (
                f"the pending-job backlog must be visible to operators "
                f"(substituting_derivations >= 2), got {backlog}"
            )

            # Close the window: the store executor comes back and the
            # jobs drain.
            kubectl(
                "set env deployment/rio-store RIO_MATERIALIZATION__ENABLED=true",
                ns="${nsStore}",
            )
            kubectl(
                "rollout status deployment/rio-store --timeout=180s",
                ns="${nsStore}",
            )
            wait_build_status(mixed_build, "succeeded", timeout=300)
            assert_outcome("mixed-flag", mixed_build, mixed_set)
            print("mixed-flag PASS: visible wait, drained when the executor returned")

        # ══════════════════════════════════════════════════════════════
        # Subtest 3: AS-6 other direction — executor on, creation off
        # ══════════════════════════════════════════════════════════════
        # The store executor polls a flag-off scheduler: lists return
        # empty, nothing is claimed, and the WALK serves the build (the
        # as-built path). The state is a no-op, not a hazard.
        with subtest("store-only-noop: scheduler-off/store-on serves builds via the walk"):
            jobs_before_noop = int(psql_k8s(
                k3s_server, "SELECT count(*) FROM materialization_jobs"
            ))
            kubectl(
                "set env deployment/rio-scheduler RIO_MATERIALIZATION__ENABLED=false",
                ns="${ns}",
            )
            kubectl(
                "rollout status deployment/rio-scheduler --timeout=180s",
                ns="${ns}",
            )
            # The rollout replaced both replicas; wait for a leader.
            k3s_server.wait_until_succeeds(
                "k3s kubectl -n ${ns} get lease rio-scheduler-leader "
                "-o jsonpath='{.spec.holderIdentity}' | grep -q rio-scheduler",
                timeout=90,
            )
            repoint_leader_pf()

            noop_build = submit_leaves("noop", noop_set)
            wait_build_status(noop_build, "succeeded", timeout=300)

            # Zero new jobs were created (the scheduler is flag-off); the
            # walk did the work.
            jobs_after_noop = int(psql_k8s(
                k3s_server, "SELECT count(*) FROM materialization_jobs"
            ))
            assert jobs_after_noop == jobs_before_noop, (
                f"a flag-off scheduler must create no jobs: "
                f"{jobs_before_noop} -> {jobs_after_noop}"
            )
            assert_outcome("store-only-noop", noop_build, noop_set)

            # Restore the deployed default for any later use.
            kubectl(
                "set env deployment/rio-scheduler RIO_MATERIALIZATION__ENABLED=true",
                ns="${ns}",
            )
            kubectl(
                "rollout status deployment/rio-scheduler --timeout=180s",
                ns="${ns}",
            )
            print("store-only-noop PASS: walk served the build, zero jobs created")

    k3s_server.execute("systemctl stop 'dag-*' 2>/dev/null || true")
    pf_close("pf-sched")
    pf_close("pf-store")

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
