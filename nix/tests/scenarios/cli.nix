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
# sched.admin.{create-tenant,list-tenants,list-workers,list-builds,
# clear-poison} — verify markers at default.nix:vm-cli-k3s
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
  skipTypeCheck = true;

  # Bring-up ~3-4min + a few CLI calls. No builds, no recovery.
  globalTimeout = 600 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

    ${common.kvmCheck}
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
        ">/tmp/pf-cli.log 2>&1 & echo $! > /tmp/pf-cli.pid"
    )
    # Port-forward needs a moment to bind. nc-probe (not sleep):
    # fails fast if the pf died (scheduler crashed, port in use).
    k3s_server.wait_until_succeeds(
        "${pkgs.netcat}/bin/nc -z localhost 19001", timeout=10
    )

    # One CLI invocation. Always returns stdout+stderr (2>&1) so error
    # messages show up in the test log on failure. covShellEnv sets
    # LLVM_PROFILE_FILE in coverage mode (empty otherwise) — without
    # it, the instrumented rio-cli binary runs but flushes profraws
    # to the default `./default.profraw` (CWD of k3s_server's shell,
    # probably /) which collectCoverage's tar doesn't pick up.
    def cli(args):
        return k3s_server.succeed(
            "${common.covShellEnv}"
            "${tlsEnv}"
            "RIO_SCHEDULER_ADDR=localhost:19001 "
            f"${rioCli} {args} 2>&1"
        )

    # ══════════════════════════════════════════════════════════════════
    # status — ClusterStatus + ListWorkers + ListBuilds
    # ══════════════════════════════════════════════════════════════════
    # The three AdminService RPCs fire in sequence (main.rs:131-170).
    # print_status formats as "executors: N total, ...". At least one
    # builder pod (rio-builder-x86_64-0) should be registered by now —
    # waitReady blocks until it's Ready.
    with subtest("cli status: ClusterStatus shows executors"):
        out = cli("status")
        print(f"cli status output:\n{out}")
        # print_status line 97: "executors: {total} total, {active} active, ..."
        assert "executors:" in out, (
            f"status output should contain 'executors:' summary line:\n{out!r}"
        )
        # Should report ≥1 total executors (rio-builder-x86_64-0 is up).
        # Parse the total count from "executors: N total".
        import re
        m = re.search(r'executors:\s+(\d+)\s+total', out)
        assert m and int(m.group(1)) >= 1, (
            f"expected ≥1 executor in status summary:\n{out!r}"
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

    # ══════════════════════════════════════════════════════════════════
    # workers — standalone ListWorkers (detailed view) + --json
    # ══════════════════════════════════════════════════════════════════
    with subtest("cli workers: detailed view + --json is valid"):
        # Human output: print_worker multi-line format, "worker <id> [<status>]"
        # on line 1. Same ≥1-worker invariant as status — waitReady
        # guarantees rio-builder-x86_64-0 registered.
        out = cli("workers")
        print(f"cli workers output:\n{out}")
        assert "worker " in out and "[" in out, (
            f"workers should print per-worker blocks:\n{out!r}"
        )

        # --json: jq -e '.executors | length >= 1' exits nonzero if the
        # key is absent, not an array, or empty. Store-path jq pulls
        # it into the VM closure (same trick as netcat above).
        k3s_server.succeed(
            "${common.covShellEnv}"
            "${tlsEnv}"
            "RIO_SCHEDULER_ADDR=localhost:19001 "
            "${rioCli} workers --json "
            "| ${pkgs.jq}/bin/jq -e '.executors | length >= 1'"
        )

    # ══════════════════════════════════════════════════════════════════
    # builds — standalone ListBuilds (no build submitted here)
    # ══════════════════════════════════════════════════════════════════
    # cli.nix doesn't submit builds (globalTimeout=600 budget is for
    # bring-up + a few CLI calls, not a build). Assert the empty-state
    # path: exit 0, "(no builds — 0 total matching filter)". A
    # populated-state assertion lives in lifecycle.nix where builds
    # are actually submitted.
    with subtest("cli builds: empty-state exits 0"):
        out = cli("builds")
        print(f"cli builds output:\n{out}")
        assert "0 total" in out or "no builds" in out, (
            f"expected empty-state marker:\n{out!r}"
        )

        # --json: total_count=0, builds=[] is a valid object.
        k3s_server.succeed(
            "${common.covShellEnv}"
            "${tlsEnv}"
            "RIO_SCHEDULER_ADDR=localhost:19001 "
            "${rioCli} builds --json "
            "| ${pkgs.jq}/bin/jq -e '.total_count == 0 and (.builds | length == 0)'"
        )

    # ══════════════════════════════════════════════════════════════════
    # gc --dry-run — TriggerGC streaming
    # ══════════════════════════════════════════════════════════════════
    # dry_run=true means the store reports what it WOULD collect but
    # doesn't delete. Scheduler populates extra_roots from the actor
    # (empty here — no live builds) and proxies to the store. The
    # store's sweep on a fresh cluster with no paths should finish
    # near-instantly with an is_complete=true frame.
    #
    # The CLI warns to stderr if the stream closes without is_complete
    # — grep stderr to catch that (would indicate scheduler→store
    # proxy dropped the terminal frame).
    with subtest("cli gc --dry-run: stream drains to is_complete"):
        out = cli("gc --dry-run")
        print(f"cli gc output:\n{out}")
        assert "dry-run complete" in out, (
            f"expected is_complete terminal frame (scheduler→store proxy "
            f"may have dropped it):\n{out!r}"
        )
        # stderr is merged into `out` via 2>&1 in cli() — check the
        # warning didn't fire.
        assert "closed without is_complete" not in out, (
            f"GC stream closed dirty:\n{out!r}"
        )

    # ══════════════════════════════════════════════════════════════════
    # poison-clear — ClearPoison on a never-poisoned hash
    # ══════════════════════════════════════════════════════════════════
    # Per spec (r[sched.admin.clear-poison]): idempotent. Calling on a
    # non-poisoned/non-existent hash returns cleared=false WITHOUT
    # error. The CLI exits 0 and prints "not poisoned". (An empty hash
    # would be InvalidArgument, but a well-formed-but-unknown hash is
    # fine — the spec explicitly says so.)
    with subtest("cli poison-clear: idempotent on unknown hash"):
        fake_hash = "/nix/store/" + "0" * 32 + "-nothing.drv"
        out = cli(f"poison-clear {fake_hash}")
        print(f"cli poison-clear output:\n{out}")
        assert "not poisoned" in out, (
            f"expected idempotent no-op message for unknown hash:\n{out!r}"
        )

        # --json: cleared=false
        k3s_server.succeed(
            "${common.covShellEnv}"
            "${tlsEnv}"
            "RIO_SCHEDULER_ADDR=localhost:19001 "
            f"${rioCli} poison-clear {fake_hash} --json "
            "| ${pkgs.jq}/bin/jq -e '.cleared == false'"
        )

    # ══════════════════════════════════════════════════════════════════
    # logs — GetBuildLogs streaming (error-path: no active derivation)
    # ══════════════════════════════════════════════════════════════════
    # No build running → no ring buffer entry → server requires
    # build_id for S3 lookup. Without --build-id the server returns
    # NotFound ("no active ring buffer and build_id was not provided").
    # Deliberate error-path: proves the CLI surfaces the stream-open
    # gRPC Status correctly (not the same as stream-message errors).
    #
    # cli() uses k3s_server.succeed which asserts exit 0; for this
    # one call, use .fail() directly. 2>&1 captures the anyhow error
    # message so the assert can grep for the expected code.
    with subtest("cli logs: NotFound when no ring buffer + no build_id"):
        out = k3s_server.fail(
            "${common.covShellEnv}"
            "${tlsEnv}"
            "RIO_SCHEDULER_ADDR=localhost:19001 "
            "${rioCli} logs /nix/store/00000000000000000000000000000000-nothing.drv 2>&1"
        )
        print(f"cli logs (expected fail) output:\n{out}")
        assert "NotFound" in out or "not_found" in out.lower(), (
            f"expected NotFound gRPC code in error:\n{out!r}"
        )

    k3s_server.execute("kill $(cat /tmp/pf-cli.pid) 2>/dev/null || true")

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
