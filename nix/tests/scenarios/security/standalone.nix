# security.standalone — HMAC/tenant/validation/rate-limit/quota/dual-mode
#
# Split out of scenarios/security.nix; see that file's header for the
# full subtest map and r[verify ...] marker placement (default.nix).
{
  pkgs,
  common,
  drvs,
}:
{ fixture }:
let
  inherit (fixture) gatewayHost;

  # ── Test derivations ────────────────────────────────────────────────
  # Distinct markers so each build creates a fresh `builds` row instead
  # of DAG-dedup reusing an earlier build's result.

  hmacDrv = drvs.mkTrivial { marker = "sec-hmac"; };

  # Tenant resolution: three drvs for three SSH keys (known / unknown /
  # empty comment). The unknown case is expect_fail so its drv is never
  # actually built, but a distinct marker still avoids any cache-hit
  # confusion if the rejection ever moved post-resolve.
  tenantKnownDrv = drvs.mkTrivial { marker = "sec-tenant-known"; };
  tenantUnknownDrv = drvs.mkTrivial { marker = "sec-tenant-unknown"; };
  tenantAnonDrv = drvs.mkTrivial { marker = "sec-tenant-anon"; };

  # jwt-dual-mode: two distinct drvs so each build inserts a fresh
  # builds row (DAG-dedup would reuse). "ssh" = fallback path (no JWT
  # config on gateway), "jwt" = would be the JWT-issue path once the
  # gateway fixture is extended with RIO_JWT__KEY_PATH. For now both go
  # through the fallback — the ISSUE-side of dual-mode is proven by the
  # rust tests (server.rs:jwt_issuance_tests); this VM subtest proves
  # the FALLBACK branch (signing_key=None) is reachable and correct.
  dualSshDrv = drvs.mkTrivial { marker = "sec-dual-ssh"; };
  dualJwtDrv = drvs.mkTrivial { marker = "sec-dual-jwt"; };

  # Rate limit: 4 distinct-marker drvs so each build is fresh (no DAG
  # dedup collapsing them into one SubmitBuild). The 4th must be
  # REJECTED by the rate limiter — distinct marker guarantees it
  # would have been a new build if it got through.
  rlDrv1 = drvs.mkTrivial { marker = "sec-rl-1"; };
  rlDrv2 = drvs.mkTrivial { marker = "sec-rl-2"; };
  rlDrv3 = drvs.mkTrivial { marker = "sec-rl-3"; };
  rlDrv4 = drvs.mkTrivial { marker = "sec-rl-4"; };

  # Quota gate: two distinct-marker drvs — reject (over-quota) + pass
  # (limit raised). Distinct markers so each attempt creates a fresh
  # DAG (no dedup masking the SubmitBuild call count).
  quotaRejectDrv = drvs.mkTrivial { marker = "sec-quota-reject"; };
  quotaPassDrv = drvs.mkTrivial { marker = "sec-quota-pass"; };

  # __noChroot derivation: REJECTED by gateway's translate::validate_dag.
  # Rejection is pre-SubmitBuild so scheduler never sees it. mkCustom
  # exposes extraAttrs precisely so __noChroot can be passed through.
  noChrootDrv = drvs.mkCustom {
    name = "rio-sec-nochroot";
    extraAttrs.__noChroot = true; # ← rejected
    script = "echo should-never-run > $out";
  };
in
pkgs.testers.runNixOSTest {
  name = "rio-security";
  skipTypeCheck = true;
  # Boot + worker registration (~60s) + ~9 builds (~30s each: hmac +
  # tenant-resolve×3 + jwt-dual-mode×2 + rate-limit×3) + 3 gateway
  # restarts (tenant keys, rate-limit config, rate-limit teardown) +
  # metric scrapes. Margin for CI jitter.
  globalTimeout = 900 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

    ${common.kvmCheck}
    start_all()
    ${fixture.waitReady}

    store_url = "ssh-ng://${gatewayHost}"

    ${common.mkBuildHelperV2 {
      inherit gatewayHost;
      dumpLogsExpr = "dump_all_logs([${gatewayHost}, worker])";
    }}


    # ══════════════════════════════════════════════════════════════════
    # SSH + tenant key setup
    # ══════════════════════════════════════════════════════════════════
    # sshKeySetup creates id_ed25519 with -C "" (empty comment =
    # single-tenant mode, tenant_id NULL — no rejection). seedBusybox
    # and the HMAC build use this default key via ~/.ssh/config.

    ${common.sshKeySetup gatewayHost}

    # ── THREE additional SSH keys with different comments ─────────────
    # For tenant resolution. Gateway matches by key_data, reads the
    # MATCHED entry's comment as tenant name. All three + the default
    # id_ed25519 go in authorized_keys.
    client.succeed("ssh-keygen -t ed25519 -N ''' -C 'team-test' -f /root/.ssh/id_team_test")
    client.succeed("ssh-keygen -t ed25519 -N ''' -C 'unknown-team' -f /root/.ssh/id_unknown")
    client.succeed("ssh-keygen -t ed25519 -N ''' -C ''' -f /root/.ssh/id_anon")

    # id_ed25519 already in authorized_keys from sshKeySetup; append
    # the three tenant keys. Gateway reads the file at startup →
    # restart required.
    tenant_keys = client.succeed(
        "cat /root/.ssh/id_team_test.pub /root/.ssh/id_unknown.pub /root/.ssh/id_anon.pub"
    )
    ${gatewayHost}.succeed(f"cat >> /var/lib/rio/gateway/authorized_keys << 'EOF'\n{tenant_keys}\nEOF")
    ${gatewayHost}.succeed("systemctl restart rio-gateway")
    ${gatewayHost}.wait_for_unit("rio-gateway.service")
    ${gatewayHost}.wait_for_open_port(2222)

    # ── Seed busybox (exercises gateway PutPath → HMAC bypass) ────────
    ${common.seedBusybox gatewayHost}

    # ══════════════════════════════════════════════════════════════════
    # Section: HMAC
    # ══════════════════════════════════════════════════════════════════

    with subtest("hmac-positive: verifier loaded + build succeeds with token"):
        # r[verify sec.authz.service-token]
        # seedBusybox's nix-copy → gateway wopAddToStoreNar → gateway
        # PutPath to store with x-rio-service-token header → bypass
        # path at put_path.rs. With RIO_SERVICE_HMAC_KEY_PATH set on
        # gateway and store, the metric increments only when the store
        # verifies the service-HMAC signature. So this proves BOTH
        # (a) gateway service_signer loaded and (b) store verifier
        # accepted the token. Without this assertion, a broken
        # verifier-load (wrong env var, config wiring bug) would let
        # the build below pass silently via the assignment-token path.
        assert_metric_ge(
            ${gatewayHost}, 9092,
            "rio_store_service_token_accepted_total", 1.0,
            labels='{caller="rio-gateway"}',
        )
        print("service-token PASS: rio_store_service_token_accepted_total{caller=rio-gateway} >= 1")

        # Positive path: scheduler has HMAC signer, store has HMAC
        # verifier. A successful build proves the full token flow:
        #   1. scheduler signs Claims{worker_id, drv_hash,
        #      expected_outputs, expiry} at dispatch
        #   2. worker receives token in WorkAssignment
        #   3. worker forwards token as x-rio-assignment-token gRPC
        #      metadata on PutPath
        #   4. store verifies signature + expiry + output path ∈
        #      expected_outputs
        #
        # Negative path (PutPath without token → PERMISSION_DENIED)
        # needs crafting a raw PutPath gRPC stream with NAR chunks —
        # complex via grpcurl. Covered by unit tests in
        # rio-store/src/grpc/put_path.rs hmac module.
        # Baseline BEFORE the build. seedBusybox (L197) already did a
        # PutPath — ≥1 here would be satisfied by the seed alone. If
        # the HMAC token were silently rejected and the build succeeded
        # via some other bypass, the old assert passes anyway. Delta
        # proves THIS build's PutPath hit result=created.
        store_before = scrape_metrics(${gatewayHost}, 9092)
        try:
            out_hmac = client.succeed(
                "nix-build --no-out-link "
                f"--store '{store_url}' "
                "--arg busybox '(builtins.storePath ${common.busybox})' "
                "${hmacDrv}"
            ).strip()
        except Exception:
            dump_all_logs([${gatewayHost}, worker])
            raise
        assert out_hmac.startswith("/nix/store/"), (
            f"HMAC-signed build should succeed: {out_hmac!r}"
        )
        assert "rio-test-sec-hmac" in out_hmac, f"wrong drv name: {out_hmac!r}"

        # PutPath succeeded (token accepted). result="created" means
        # the path was NEW (not a cache hit) — so the HMAC check
        # actually ran (cache hits short-circuit before HMAC).
        store_after = scrape_metrics(${gatewayHost}, 9092)
        pp_before = metric_value(store_before,
            "rio_store_put_path_total", '{result="created"}') or 0.0
        pp_after = metric_value(store_after,
            "rio_store_put_path_total", '{result="created"}') or 0.0
        assert pp_after > pp_before, (
            f"HMAC build should increment put_path_total{{result=created}}; "
            f"before={pp_before}, after={pp_after}"
        )
        print(f"hmac-positive PASS: build with HMAC token succeeded, output {out_hmac}")

    # ══════════════════════════════════════════════════════════════════
    # Section: tenant resolution (phase4 Section A)
    # ══════════════════════════════════════════════════════════════════
    # SSH key comment → gateway captures tenant NAME → scheduler
    # resolves to UUID via `tenants` table → `builds.tenant_id` row
    # has the resolved UUID. Unknown tenant → InvalidArgument.
    # Empty comment → single-tenant mode (tenant_id IS NULL).

    def build_drv(identity_file, drv_path, expect_fail=False):
        """Build via ssh-ng using the given identity file (selects the
        matching authorized_keys entry → tenant). Thin wrapper over
        build() — identity_file becomes ?ssh-key= in the store URL."""
        return build(
            drv_path,
            store_url=f"ssh-ng://root@${gatewayHost}?ssh-key={identity_file}",
            expect_fail=expect_fail,
        )

    # Row-count check: COUNT(*) after each case. ORDER BY…LIMIT 1
    # alone can't distinguish Case 3's NULL row from a Case-2 leak
    # (if rejection ever moved post-insert, it would also produce a
    # NULL row since unknown-team never resolves to a UUID).
    def build_count():
        return int(psql(${gatewayHost}, "SELECT COUNT(*) FROM builds"))

    with subtest("tenant-resolve: known / unknown / empty-comment"):
        # ── Pre-seed the tenants table ────────────────────────────────
        # INSERT…RETURNING via psql() helper (-qtA: suppress status,
        # tuples-only, unaligned).
        tenant_uuid = psql(
            ${gatewayHost},
            "INSERT INTO tenants (tenant_name) VALUES ('team-test') RETURNING tenant_id",
        )
        ${gatewayHost}.log(f"seeded tenant team-test = {tenant_uuid}")

        # hmac-positive above ran one build via the default id_ed25519
        # (empty comment → NULL tenant). Capture the count NOW so case
        # deltas are relative to this baseline.
        initial_count = build_count()
        ${gatewayHost}.log(f"initial builds count: {initial_count}")

        # ── Case 1: key comment 'team-test' → resolved UUID ───────────
        out = build_drv("/root/.ssh/id_team_test", "${tenantKnownDrv}")
        assert out.startswith("/nix/store/"), (
            f"known-tenant build should succeed: {out!r}"
        )
        assert "rio-test-sec-tenant-known" in out, (
            f"output path should contain drv marker: {out!r}"
        )
        assert build_count() == initial_count + 1, (
            "case 1 should insert exactly one build"
        )
        db_tenant = psql(
            ${gatewayHost},
            "SELECT tenant_id FROM builds ORDER BY submitted_at DESC LIMIT 1",
        )
        assert db_tenant == tenant_uuid, (
            f"builds.tenant_id should match seeded UUID: "
            f"expected {tenant_uuid}, got {db_tenant!r}"
        )
        print(f"tenant case 1 PASS: known tenant resolved to {db_tenant}")

        # ── Case 2: key comment 'unknown-team' → InvalidArgument ──────
        out = build_drv("/root/.ssh/id_unknown", "${tenantUnknownDrv}", expect_fail=True)
        assert "unknown tenant: unknown-team" in out, (
            f"error should include the tenant name "
            f"(proves comment was captured+propagated), got: {out!r}"
        )
        assert build_count() == initial_count + 1, (
            "case 2 rejection is pre-insert: count unchanged"
        )
        print("tenant case 2 PASS: unknown tenant rejected pre-insert")

        # ── Case 3: empty comment → tenant_id IS NULL ─────────────────
        out = build_drv("/root/.ssh/id_anon", "${tenantAnonDrv}")
        assert out.startswith("/nix/store/"), (
            f"anon build should succeed: {out!r}"
        )
        assert "rio-test-sec-tenant-anon" in out, (
            f"output path should contain drv marker: {out!r}"
        )
        assert build_count() == initial_count + 2, (
            "case 3 should insert one more build"
        )
        db_tenant = psql(
            ${gatewayHost},
            "SELECT COALESCE(tenant_id::text, 'NULL') FROM builds "
            "ORDER BY submitted_at DESC LIMIT 1",
        )
        assert db_tenant == "NULL", (
            f"empty-comment key → tenant_id IS NULL, got {db_tenant!r}"
        )
        print("tenant case 3 PASS: empty comment = single-tenant mode (NULL)")

    # ══════════════════════════════════════════════════════════════════
    # Section: gateway validation
    # ══════════════════════════════════════════════════════════════════

    with subtest("gateway-validate: __noChroot derivation rejected"):
        # nix-build a derivation with __noChroot=true → gateway's
        # translate::validate_dag rejects with "sandbox escape" error
        # BEFORE SubmitBuild. The scheduler never sees it, so no TLS
        # on the scheduler path is exercised here — but that's fine,
        # this section is about gateway-side validation not scheduling.
        #
        # Capture builds count BEFORE the attempt. validate_dag is
        # gateway-only (DerivationNode doesn't carry env — the scheduler
        # couldn't check __noChroot even if it wanted to). If the count
        # bumps, the gateway let it through.
        count_before = build_count()
        result = client.fail(
            "nix-build --no-out-link "
            f"--store '{store_url}' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "${noChrootDrv} 2>&1"
        )
        assert ("sandbox escape" in result or "noChroot" in result), (
            f"expected __noChroot rejection, got: {result[:500]}"
        )
        # Pre-SubmitBuild rejection: builds table untouched. This is the
        # load-bearing half — client.fail() + error-message check alone
        # can't distinguish "rejected at gateway" from "scheduler rejected
        # it too" (if the check ever moved post-SubmitBuild).
        assert build_count() == count_before, (
            f"__noChroot rejection must be pre-SubmitBuild; "
            f"builds count changed {count_before} → {build_count()}"
        )
        print("gateway-validate PASS: __noChroot rejected at gateway, scheduler unreached")

    # ══════════════════════════════════════════════════════════════════
    # Section: rate limit (per-tenant, r[gw.rate.per-tenant])
    # ══════════════════════════════════════════════════════════════════
    # Configure per_minute=2 burst=3 via a systemd drop-in, restart the
    # gateway, fire 4 rapid builds from the same tenant SSH key. The
    # first 3 fall inside the burst bucket; the 4th is rejected with
    # "rate limit" in the error body. Verify the 4th did NOT reach the
    # scheduler (builds row count unchanged) — same pre-SubmitBuild
    # proof shape as gateway-validate above.
    #
    # Placed after gateway-validate: the restart loses the gateway's
    # in-memory state (metrics counters, rate-limiter buckets), and
    # the burst=3 config would interfere with earlier subtests if
    # applied sooner. Teardown at the end removes the config + restarts
    # so the jwt-dual-mode subtests run unconstrained.

    with subtest("rate-limit: 4th rapid build from same tenant rejected"):
        # Drop-in: figment's env layer reads RIO_RATE_LIMIT__PER_MINUTE
        # → rate_limit.per_minute. Both fields must be set (no
        # compiled-in default — r[gw.rate.per-tenant] says
        # workload-dependent). per_minute=2 → one token every 30s;
        # with burst=3, 3 rapid builds drain the bucket and the 4th
        # needs ~30s before the next token — well outside the test's
        # submit loop.
        ${gatewayHost}.succeed(
            "mkdir -p /run/systemd/system/rio-gateway.service.d && "
            "printf '[Service]\\nEnvironment=RIO_RATE_LIMIT__PER_MINUTE=2\\nEnvironment=RIO_RATE_LIMIT__BURST=3\\n' "
            "> /run/systemd/system/rio-gateway.service.d/ratelimit.conf && "
            "systemctl daemon-reload && systemctl restart rio-gateway"
        )
        ${gatewayHost}.wait_for_unit("rio-gateway.service")
        ${gatewayHost}.wait_for_open_port(2222)

        # Confirm the limiter is actually enabled (not silently
        # disabled by a config parse miss). The "per-tenant rate
        # limiting enabled" info log from main.rs is the discriminator
        # — without it, the test's 3 successes below prove nothing
        # (disabled limiter also passes).
        ${gatewayHost}.wait_until_succeeds(
            "journalctl -u rio-gateway --since '-10s' | "
            "grep -q 'per-tenant rate limiting enabled'"
        )

        count_before = build_count()

        # 3 rapid builds under id_team_test (known tenant, resolved
        # UUID). Each distinct marker → distinct DAG → fresh
        # SubmitBuild (no dedup). All 3 within burst.
        #
        # We don't `succeed` on these — the build itself may fail
        # for unrelated reasons (worker timeout, etc). What matters
        # for rate-limiting is the SUBMIT path: each build_drv
        # that doesn't hit "rate limit" in its output is one
        # consumed token. Using succeed+out would couple this
        # subtest to the worker's health; instead, check for ABSENCE
        # of the rate-limit error.
        for i, drv in enumerate(["${rlDrv1}", "${rlDrv2}", "${rlDrv3}"], start=1):
            out = client.succeed(
                "nix-build --no-out-link "
                "--store 'ssh-ng://root@${gatewayHost}?ssh-key=/root/.ssh/id_team_test' "
                "--arg busybox '(builtins.storePath ${common.busybox})' "
                f"{drv} 2>&1 || true"
            )
            assert "rate limit" not in out, (
                f"build #{i} unexpectedly rate-limited "
                f"(burst=3 should allow it): {out[:300]}"
            )
            ${gatewayHost}.log(f"rate-limit: build #{i} submitted (within burst)")

        # 4th build — should be rejected. burst=3 drained, per_minute=2
        # means the next token is ~30s away. The nix-build will fail
        # with the gateway's STDERR_ERROR containing "rate limit".
        out = client.fail(
            "nix-build --no-out-link "
            "--store 'ssh-ng://root@${gatewayHost}?ssh-key=/root/.ssh/id_team_test' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "${rlDrv4} 2>&1"
        )
        assert "rate limit" in out, (
            f"4th build should be rate-limited; got: {out[:500]}"
        )
        # The wait-hint should mention the tenant name (proves the
        # limiter keyed on tenant_name, not a generic counter).
        assert "team-test" in out, (
            f"rate-limit error should name the tenant: {out[:500]}"
        )
        # Pre-SubmitBuild rejection: scheduler never saw the 4th. The
        # first 3 may or may not have succeeded (worker-dependent), so
        # we check DELTA <= 3, not == 3. The load-bearing half is the
        # 4th's builds-count being unchanged from count-after-third.
        count_after = build_count()
        assert count_after <= count_before + 3, (
            f"at most 3 builds should reach scheduler; "
            f"count {count_before} → {count_after}"
        )
        print(
            "rate-limit PASS: 3 submits within burst, 4th rejected "
            "pre-SubmitBuild with tenant-named error"
        )

        # Teardown: remove the drop-in so subsequent subtests aren't
        # rate-limited by the leftover burst=3.
        ${gatewayHost}.succeed(
            "rm -f /run/systemd/system/rio-gateway.service.d/ratelimit.conf && "
            "systemctl daemon-reload && systemctl restart rio-gateway"
        )
        ${gatewayHost}.wait_for_unit("rio-gateway.service")
        ${gatewayHost}.wait_for_open_port(2222)

    # ══════════════════════════════════════════════════════════════════
    # Section: jwt-dual-mode (TAIL — serial after P0255's quota fragment)
    # ══════════════════════════════════════════════════════════════════
    # Dual-mode PERMANENT. The gateway's signing_key is None in this
    # fixture (no RIO_JWT__KEY_PATH set → JwtConfig::default() →
    # server.rs auth_publickey takes the signing_key=None arm →
    # jwt_token stays None → handler/build.rs skips the
    # x-rio-tenant-token header → scheduler reads
    # SubmitBuildRequest.tenant_name). This is the SSH-comment
    # fallback branch. That it WORKS is what tenant-resolve above
    # already proved; this subtest pins it SPECIFICALLY as the
    # dual-mode fallback (not just "tenant resolution works").
    #
    # The JWT-issue branch (signing_key=Some → ResolveTenant RPC →
    # mint → header inject) is covered unit-side by:
    #   - scheduler/grpc/tests.rs::test_resolve_tenant_rpc (the RPC)
    #   - server.rs::jwt_issuance_tests (mint + token contents)
    #   - jwt_interceptor.rs tests (verify + hot-swap)
    # WONTFIX(P0349): extending the standalone fixture with
    # RIO_JWT__KEY_PATH was descoped — the JWT-issue branch is now
    # covered by the k3s jwt-mount-present subtest (lifecycle.nix,
    # P0357) + the rust unit tests above. The FALLBACK branch is the
    # PERMANENT path that must never break — proving it here under
    # a real gateway+scheduler+PG is the load-bearing half.

    with subtest("jwt-dual-mode: fallback branch reachable, same tenant both builds"):
        # Precondition: gateway has NO JWT config. Verify by checking
        # the new metric stays at 0 (describe_metrics registered it,
        # but no mint-degrade ever fires because the signing_key=None
        # arm never attempts a mint). If this ever bumps, the fixture
        # grew JWT config and this subtest's premise is wrong.
        degraded = metric_value(
            scrape_metrics(${gatewayHost}, 9090),
            "rio_gateway_jwt_mint_degraded_total",
        )
        assert degraded is None or degraded == 0.0, (
            f"precondition: fixture has no JWT config, mint-degrade "
            f"should never fire. Got {degraded} — did fixture grow "
            f"RIO_JWT__KEY_PATH?"
        )

        count_before = build_count()

        # Two builds, SAME SSH key (id_team_test, comment 'team-test'),
        # different drvs. Both go through the fallback branch → both
        # get SubmitBuildRequest.tenant_name='team-test' → both
        # resolve to the SAME tenant UUID (team-test was seeded at
        # tenant-resolve case 1 above — tenant_uuid holds it).
        out_ssh = build_drv("/root/.ssh/id_team_test", "${dualSshDrv}")
        assert "rio-test-sec-dual-ssh" in out_ssh, f"wrong drv: {out_ssh!r}"
        out_jwt = build_drv("/root/.ssh/id_team_test", "${dualJwtDrv}")
        assert "rio-test-sec-dual-jwt" in out_jwt, f"wrong drv: {out_jwt!r}"

        assert build_count() == count_before + 2, (
            "both builds should insert (distinct drvs, no DAG-dedup)"
        )

        # Both rows have the SAME tenant_id = team-test's UUID.
        # LIMIT 2 ordered by time → the two builds we just did.
        # array_agg so one psql() call gets both rows; parsing a
        # 2-row output is brittle with psql -qtA.
        both = psql(
            ${gatewayHost},
            "SELECT array_agg(DISTINCT tenant_id::text) FROM "
            "(SELECT tenant_id FROM builds ORDER BY submitted_at DESC LIMIT 2) t",
        )
        # array_agg(DISTINCT ...) with one distinct value → {uuid}.
        # With two distinct → {uuid1,uuid2}. Strip braces + check.
        assert both.strip("{}") == tenant_uuid, (
            f"both dual-mode builds should resolve to team-test's "
            f"UUID {tenant_uuid}; got array {both!r} (distinct values "
            f"would mean the two branches diverge)"
        )

        # And it's the SAME UUID as tenant-resolve case 1's build —
        # proving the fallback path produces the same attribution
        # whether JWT is disabled-by-config or would-be-disabled-by-
        # mint-failure (both land in builds.tenant_id via tenant_name).
        print(
            f"jwt-dual-mode PASS: fallback branch → both builds "
            f"resolved to {tenant_uuid} (same as tenant-resolve case 1)"
        )

    # ══════════════════════════════════════════════════════════════════
    # Section: quota gate (per-tenant, r[store.gc.tenant-quota-enforce])
    # ══════════════════════════════════════════════════════════════════
    # TAIL-append: placed LAST so the teardown restart is the final
    # gateway state change. Earlier subtests (tenant-resolve case 1,
    # jwt-dual-mode) populated path_tenants for team-test — the quota
    # SUM has non-zero bytes to compare against. Without that, a
    # limit of 1 byte would still pass (0 > 1 is false), and the test
    # would prove nothing about the gate.
    #
    # Precondition-guard: assert team-test has ≥1 path_tenants row
    # with non-zero nar_size BEFORE lowering the limit. This is the
    # "proves nothing" guard — if prior subtests didn't populate
    # path_tenants, the Over verdict is unreachable.

    with subtest("quota-exceeded: over gc_max_store_bytes rejected pre-SubmitBuild"):
        # Precondition: team-test owns paths with non-zero total. The
        # gate is `used > limit`; with used=0, setting limit=1 still
        # passes (0 > 1 = false). tenant-resolve case 1 built
        # sec-tenant-known under id_team_test → completion hook upserted
        # (store_path_hash, team-test-uuid) → this SUM is non-zero.
        used_before = int(psql(
            ${gatewayHost},
            f"SELECT COALESCE(SUM(n.nar_size), 0)::bigint "
            f"FROM narinfo n JOIN path_tenants pt USING (store_path_hash) "
            f"WHERE pt.tenant_id = '{tenant_uuid}'"
        ))
        assert used_before > 0, (
            f"precondition: team-test must have ≥1 byte of path_tenants "
            f"usage for the Over verdict to be reachable (got {used_before}). "
            f"Did tenant-resolve case 1 build complete + upsert?"
        )
        ${gatewayHost}.log(
            f"quota: team-test uses {used_before} bytes — setting limit=1"
        )

        # Set limit=1 byte → any non-zero usage is over. Restart the
        # gateway to flush its 30s QuotaCache (otherwise a stale
        # Unlimited/Under entry from prior builds would hide the gate).
        psql(
            ${gatewayHost},
            f"UPDATE tenants SET gc_max_store_bytes = 1 "
            f"WHERE tenant_id = '{tenant_uuid}'"
        )
        ${gatewayHost}.succeed("systemctl restart rio-gateway")
        ${gatewayHost}.wait_for_unit("rio-gateway.service")
        ${gatewayHost}.wait_for_open_port(2222)

        count_before = build_count()

        # Over-quota → STDERR_ERROR "over store quota". client.fail
        # captures stderr (nix-build exits non-zero on STDERR_ERROR).
        out = client.fail(
            "nix-build --no-out-link "
            "--store 'ssh-ng://root@${gatewayHost}?ssh-key=/root/.ssh/id_team_test' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "${quotaRejectDrv} 2>&1"
        )
        assert "over store quota" in out, (
            f"over-quota build should be rejected with 'over store quota': "
            f"{out[:500]}"
        )
        assert "team-test" in out, (
            f"quota error should name the tenant: {out[:500]}"
        )
        # Pre-SubmitBuild rejection: scheduler never saw it. Same
        # proof shape as gateway-validate + rate-limit above.
        assert build_count() == count_before, (
            f"quota rejection must be pre-SubmitBuild; "
            f"builds count changed {count_before} → {build_count()}"
        )
        print(
            "quota-exceeded PASS: over gc_max_store_bytes rejected "
            "pre-SubmitBuild with tenant-named error"
        )

        # Positive control: raise limit above used → build passes.
        # Without this, a gate that always rejects (e.g., classify
        # bug mapping every non-None limit to Over) would green-pass
        # the negative case. Restart again to flush the cached Over
        # verdict.
        psql(
            ${gatewayHost},
            f"UPDATE tenants SET gc_max_store_bytes = {used_before * 1000 + 1} "
            f"WHERE tenant_id = '{tenant_uuid}'"
        )
        ${gatewayHost}.succeed("systemctl restart rio-gateway")
        ${gatewayHost}.wait_for_unit("rio-gateway.service")
        ${gatewayHost}.wait_for_open_port(2222)

        out = client.succeed(
            "nix-build --no-out-link "
            "--store 'ssh-ng://root@${gatewayHost}?ssh-key=/root/.ssh/id_team_test' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "${quotaPassDrv} 2>&1 || true"
        )
        assert "over store quota" not in out, (
            f"under-quota build must NOT be rejected: {out[:500]}"
        )
        assert build_count() > count_before, (
            "positive control: under-quota build should reach scheduler"
        )
        print("quota-exceeded positive-control PASS: limit raised → build accepted")

        # Teardown: clear the limit so future subtests aren't gated.
        psql(
            ${gatewayHost},
            f"UPDATE tenants SET gc_max_store_bytes = NULL "
            f"WHERE tenant_id = '{tenant_uuid}'"
        )

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
