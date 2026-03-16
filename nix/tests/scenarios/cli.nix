# rio-cli smoke: AdminService round-trip via the binary.
#
# rio-cli had 0% coverage — it's never run by any test. The binary wraps
# CreateTenant/ListTenants/ClusterStatus/ListWorkers/ListBuilds; this
# test exercises all of them end-to-end against a live scheduler.
#
# k3s-full fixture: scheduler runs with mTLS on. rio-cli needs a client
# cert signed by the same CA. lifecycle.nix already uses the fixture's
# `pki` store-path attr for grpcurl (the certs are on disk in the VM via
# closure interpolation) — same trick here. No need to extract from the
# k8s Secret.
#
# r[verify sched.admin.create-tenant]
# r[verify sched.admin.list-tenants]
# r[verify sched.admin.list-workers]
# r[verify sched.admin.list-builds]
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) ns pki;

  # Store-path interpolation pulls the binary into the VM closure.
  # rio-cli is a Rust binary, linked to glibc (which the NixOS VM has).
  rioCli = "${common.rio-workspace}/bin/rio-cli";

  # mTLS client cert — controller's cert works (rio checks CA-signed,
  # not CN). SANs include `localhost` so port-forward's implicit
  # SNI=localhost verifies. Same cert lifecycle.nix uses for grpcurl.
  tlsEnv =
    "RIO_TLS__CERT_PATH=${pki}/rio-controller/tls.crt "
    + "RIO_TLS__KEY_PATH=${pki}/rio-controller/tls.key "
    + "RIO_TLS__CA_PATH=${pki}/ca.crt ";
in
pkgs.testers.runNixOSTest {
  name = "rio-cli";

  # Bring-up ~3-4min + a few CLI calls. No builds, no recovery.
  globalTimeout = 600;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

    start_all()
    ${fixture.waitReady}
    ${fixture.kubectlHelpers}
    ${fixture.sshKeySetup}

    # Port-forward the scheduler leader's gRPC port (9001). Unlike
    # lifecycle.nix's per-call port-forward, this stays up for the
    # whole test — all CLI calls go through localhost:19001. `trap`
    # kills it on script exit.
    leader = leader_pod()
    k3s_server.succeed(
        f"k3s kubectl -n ${ns} port-forward {leader} 19001:9001 "
        f">/tmp/pf-cli.log 2>&1 & echo $! > /tmp/pf-cli.pid"
    )
    # Port-forward needs a moment to bind. nc-probe (not sleep):
    # fails fast if the pf died (scheduler crashed, port in use).
    k3s_server.wait_until_succeeds(
        "${pkgs.netcat}/bin/nc -z localhost 19001", timeout=10
    )

    # One CLI invocation. Always returns stdout+stderr (2>&1) so error
    # messages show up in the test log on failure.
    def cli(args):
        return k3s_server.succeed(
            "${tlsEnv}"
            "RIO_SCHEDULER_ADDR=localhost:19001 "
            f"${rioCli} {args} 2>&1"
        )

    # ══════════════════════════════════════════════════════════════════
    # status — ClusterStatus + ListWorkers + ListBuilds
    # ══════════════════════════════════════════════════════════════════
    # The three AdminService RPCs fire in sequence (main.rs:131-170).
    # print_status formats as "workers: N total, ...". At least one
    # worker pod (default-workers-0) should be registered by now —
    # waitReady blocks until it's Ready.
    with subtest("cli status: ClusterStatus shows workers"):
        out = cli("status")
        print(f"cli status output:\n{out}")
        # print_status line 191: "workers: {total} total, {active} active, ..."
        assert "workers:" in out, (
            f"status output should contain 'workers:' summary line:\n{out!r}"
        )
        # Should report ≥1 total workers (default-workers-0 is up).
        # Parse the total count from "workers: N total".
        import re
        m = re.search(r'workers:\s+(\d+)\s+total', out)
        assert m and int(m.group(1)) >= 1, (
            f"expected ≥1 worker in status summary:\n{out!r}"
        )
        # ListWorkers detail line (main.rs:143): "  worker <id> [<status>] ..."
        assert "  worker " in out, (
            f"status should include per-worker detail lines:\n{out!r}"
        )

    # ══════════════════════════════════════════════════════════════════
    # create-tenant + list-tenants — CreateTenant round-trip
    # ══════════════════════════════════════════════════════════════════
    # print_tenant (main.rs:178) formats as:
    #   "tenant <name> (<uuid>)  gc_retention=<N>h  max_store=...  cache_token=..."
    with subtest("cli create-tenant: CreateTenant + ListTenants round-trip"):
        out = cli("create-tenant cli-smoke-tenant --gc-retention-hours=48")
        print(f"create-tenant output:\n{out}")
        # Creation echoes the tenant back via print_tenant.
        assert "tenant cli-smoke-tenant" in out, (
            f"create-tenant should echo the tenant name:\n{out!r}"
        )
        assert "gc_retention=48h" in out, (
            f"create-tenant should echo gc_retention_hours:\n{out!r}"
        )

        # ListTenants should include it now. Also proves the tenant was
        # actually persisted (not just echoed from the request).
        out = cli("list-tenants")
        print(f"list-tenants output:\n{out}")
        assert "cli-smoke-tenant" in out, (
            f"list-tenants should include the tenant we just created:\n{out!r}"
        )

    k3s_server.execute("kill $(cat /tmp/pf-cli.pid) 2>/dev/null || true")

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
