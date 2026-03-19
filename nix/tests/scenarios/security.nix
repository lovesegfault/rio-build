# Security scenario: mTLS + HMAC + tenant resolution + gateway validation.
#
# Ports phase3b sections T (mTLS) + B (HMAC) + G (gateway-validate), plus
# phase4 section A (tenant resolution), onto the standalone fixture.
#
# r[verify gw.jwt.dual-mode]
# jwt-dual-mode subtest: proves both branches of the PERMANENT
# dual-mode are reachable. SSH-comment branch (signing_key=None —
# the fixture's default) → tenant identity via
# SubmitBuildRequest.tenant_name. The JWT-issue branch is proven
# compile-side by server.rs:resolve_and_mint + jwt_issuance_tests;
# this VM subtest pins the FALLBACK branch under a real
# gateway+scheduler+PG end-to-end.
#
# r[verify sec.boundary.grpc-hmac]
# mTLS-reject/-accept + HMAC-verifier prove both halves of the trust
# boundary: TLS terminates at the gRPC port, HMAC gates PutPath.
#
# r[verify store.tenant.narinfo-filter]
# cache-auth-tenant-filter subtest: tenant A → 404 on tenant B's path,
# tenant B → 200 on own. The 200 control guards against JOIN-matches-
# nothing (the 404 alone proves nothing if the filter always misses).
#
# r[verify gw.reject.nochroot]
# gateway-validate subtest: nix-build a .drv with __noChroot=true via
# ssh-ng://. Gateway rejects with "sandbox escape" pre-SubmitBuild;
# builds row count unchanged proves scheduler never saw it. Exercises
# the validate_dag path (translate.rs:301) — client uploads the .drv to
# the store via wopAddToStoreNar, then wopBuildPathsWithResults triggers
# BFS → drv_cache populated → validate_dag fires on the env entry.
#
# r[verify gw.rate.per-tenant]
# rate-limit subtest: configure per_minute=2 burst=3 via systemd
# drop-in, fire 4 rapid builds from the same tenant SSH key → 4th
# gets STDERR_ERROR with "rate limit" body. builds row count unchanged
# on the 4th proves the scheduler never saw it (same pre-SubmitBuild
# gate as gateway-validate).
#
# Caller (default.nix) constructs the fixture with:
#   fixture = standalone {
#     workers = { worker = { maxBuilds = 1; }; };
#     withPki = true;
#     extraPackages = [ pkgs.grpcurl pkgs.grpc-health-probe pkgs.postgresql ];
#   };
#
# withPki=true → fixture.pki is a store path to lib/pki.nix output
# (ca.crt, server.crt/key, client.crt/key, gateway.crt/key, hmac.key).
# The fixture wires RIO_TLS__* + RIO_HMAC_KEY_PATH via extraServiceEnv;
# gateway gets CN=rio-gateway cert (store PutPath HMAC bypass).
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) gatewayHost pki;
  drvs = import ../lib/derivations.nix { inherit pkgs; };

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

  # __noChroot derivation: REJECTED by gateway's translate::validate_dag.
  # Rejection is pre-SubmitBuild so scheduler never sees it. Not using
  # drvs.mkTrivial — that factory doesn't expose arbitrary env attrs,
  # and __noChroot is the whole point.
  noChrootDrv = pkgs.writeText "sec-nochroot.nix" ''
    { busybox }:
    derivation {
      name = "rio-sec-nochroot";
      system = "x86_64-linux";
      __noChroot = true;  # ← rejected
      builder = "''${busybox}/bin/sh";
      args = [ "-c" "echo should-never-run > $out" ];
    }
  '';

  # Coverage mode: graceful-stop + profraw tar + copy_from_vm. Additive so
  # normal-mode CI budget is unchanged.
  covTimeoutHeadroom = if common.coverage then 300 else 0;
in
pkgs.testers.runNixOSTest {
  name = "rio-security";
  skipTypeCheck = true;
  # Boot + worker registration (~60s) + ~9 builds (~30s each: hmac +
  # tenant-resolve×3 + jwt-dual-mode×2 + rate-limit×3) + 3 gateway
  # restarts (tenant keys, rate-limit config, rate-limit teardown) +
  # metric scrapes. Margin for CI jitter.
  globalTimeout = 900 + covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

    ${common.kvmPreopen}
    start_all()
    ${fixture.waitReady}

    store_url = "ssh-ng://${gatewayHost}"

    # ══════════════════════════════════════════════════════════════════
    # Section: mTLS
    # ══════════════════════════════════════════════════════════════════
    # Run FIRST, before any builds — minimal state, pure connectivity
    # probes. The worker's Register RPC (inside fixture.waitReady's
    # workers_active wait) already went over mTLS, so reaching this
    # point IS implicit mTLS-positive proof. The explicit probes here
    # make the CLIENT cert verification path + plaintext rejection
    # visible in CI logs.

    with subtest("mtls-reject: plaintext connect to TLS port fails"):
        # grpcurl -plaintext against the scheduler's main port (9001,
        # mTLS). The TCP connection succeeds but the TLS handshake
        # fails — the server expects a ClientHello, gets a plaintext
        # HTTP/2 preface. grpcurl surfaces this as "connection reset"
        # or a tls-related error. We only check that it FAILS; the
        # exact error text varies by grpcurl/tonic version.
        #
        # -max-time 5: don't hang if something's misconfigured. The
        # failure is immediate (handshake), not a timeout.
        result = ${gatewayHost}.fail(
            "grpcurl -plaintext -max-time 5 localhost:9001 "
            "grpc.health.v1.Health/Check 2>&1"
        )
        # CRITICAL: the error must NOT be "connection refused". That
        # would mean the port isn't listening at all — a different
        # failure mode (service down, wrong port) that this subtest
        # would otherwise mask as a pass. Any of "tls" / "EOF" /
        # "connection reset" / "handshake" is fine.
        assert "refused" not in result.lower(), (
            f"port should be OPEN (TLS rejects, not refused): {result[:200]}"
        )
        print("mtls-reject PASS: plaintext rejected on mTLS port 9001")

    with subtest("mtls-reject-grpc: TLS-on-but-no-client-cert handshake fails"):
        # The plaintext-reject above proves "no TLS at all → rejected".
        # THIS proves "TLS handshake started, server cert verified, but
        # no client cert presented → rejected". Distinct failure mode:
        # a server configured for TLS-but-not-MUTUAL-TLS would accept
        # here. That's the mTLS-vs-TLS distinction this subtest pins.
        #
        # -cacert only (server cert verified against our CA) — no
        # -cert/-key (no client cert presented). tonic's mTLS config
        # sends a CertificateRequest in the handshake; with no client
        # cert to send, the handshake aborts before any HTTP/2 preface.
        # Health/Check: no protoset needed (grpcurl bundles those
        # descriptors), and the handshake fails before any RPC
        # resolution anyway.
        result = ${gatewayHost}.fail(
            "grpcurl -cacert ${pki}/ca.crt -max-time 5 "
            "localhost:9002 grpc.health.v1.Health/Check 2>&1"
        )
        # Same guard as the plaintext case: "refused" = port not
        # listening = wrong failure (service down, not mTLS working).
        assert "refused" not in result.lower(), (
            f"port should be OPEN (mTLS rejects no-cert, not refused): {result[:200]}"
        )
        # Scheduler too — both mTLS-gated services.
        result = ${gatewayHost}.fail(
            "grpcurl -cacert ${pki}/ca.crt -max-time 5 "
            "localhost:9001 grpc.health.v1.Health/Check 2>&1"
        )
        assert "refused" not in result.lower(), (
            f"port should be OPEN (mTLS rejects no-cert, not refused): {result[:200]}"
        )
        print("mtls-reject-grpc PASS: no-client-cert rejected on 9001 + 9002")

    with subtest("mtls-accept: connect with valid client cert succeeds"):
        # Use the DEDICATED client cert (CN=rio-worker), not server.crt.
        # Reusing server.crt would work (same CA) but wouldn't prove
        # the CLIENT cert verification path — a client-cert-only check
        # might reject CN=control. client.crt has CN=rio-worker (the
        # actual worker identity); using it proves client-auth truly
        # validates against the CA, not server identity.
        #
        # -tls-server-name localhost: grpc-health-probe doesn't derive
        # SNI from -addr the way tonic does; set explicitly to match a
        # SAN (localhost is in the server cert, see lib/pki.nix).
        ${gatewayHost}.succeed(
            "grpc-health-probe -addr localhost:9001 "
            "-tls -tls-ca-cert ${pki}/ca.crt "
            "-tls-client-cert ${pki}/client.crt "
            "-tls-client-key ${pki}/client.key "
            "-tls-server-name localhost"
        )
        # Same for the store.
        ${gatewayHost}.succeed(
            "grpc-health-probe -addr localhost:9002 "
            "-tls -tls-ca-cert ${pki}/ca.crt "
            "-tls-client-cert ${pki}/client.crt "
            "-tls-client-key ${pki}/client.key "
            "-tls-server-name localhost"
        )
        print("mtls-accept PASS: client cert (CN=rio-worker) accepted on 9001 + 9002")

    with subtest("mtls-health-shared: plaintext health port spawned when TLS on"):
        # The scheduler/store spawn a SECOND plaintext server on
        # health_addr (9101/9102) when TLS is enabled. K8s gRPC
        # readinessProbes can't do mTLS — this port is for them. It
        # serves ONLY grpc.health.v1.Health, sharing the HealthReporter
        # with the main port via health_service.clone().
        #
        # ADAPTATION NOTE (vs phase3b): phase3b proves the reporter is
        # SHARED by probing NOT_SERVING during the scheduler's standby
        # window (lease config + missing kubeconfig → is_leader=false
        # → set_not_serving). The standalone fixture has no k8s, so no
        # lease config → scheduler uses always_leader() →
        # recovery_complete=true from boot → SERVING immediately. The
        # NOT_SERVING half of the shared-reporter proof is deferred to
        # a k3s-fixture variant (scenarios/lifecycle.nix).
        #
        # What this subtest DOES prove: the plaintext port is BOUND and
        # serves the NAMED service. If the second server didn't spawn,
        # or didn't register the named service, the probe fails. That
        # the named-service probe succeeds is weak evidence of shared
        # reporter (both servers register the same name → same
        # HealthReporter instance, not two independent registrations).
        ${gatewayHost}.succeed(
            "grpc-health-probe -addr localhost:9101 "
            "-service rio.scheduler.SchedulerService"
        )
        # Default service (always SERVING once bound) — proves port up.
        ${gatewayHost}.succeed("grpc-health-probe -addr localhost:9101")
        ${gatewayHost}.succeed("grpc-health-probe -addr localhost:9102")
        print("mtls-health-shared PASS: plaintext health ports bound + named service registered")

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
        # seedBusybox's nix-copy → gateway wopAddToStoreNar → gateway
        # PutPath to store via mTLS with CN=rio-gateway → bypass path
        # at put_path.rs. The metric ONLY increments when
        # hmac_verifier.is_some() (the early-return on None never
        # reaches the bypass branch). So this proves BOTH (a) verifier
        # loaded (HmacVerifier::load succeeded with RIO_HMAC_KEY_PATH)
        # and (b) the CN check ran. Without this assertion, a broken
        # verifier-load (wrong env var, config wiring bug →
        # verifier=None) would let the build below pass silently.
        assert_metric_ge(
            ${gatewayHost}, 9092,
            "rio_store_hmac_bypass_total", 1.0,
            labels='{cn="rio-gateway"}',
        )
        print("hmac-verifier PASS: rio_store_hmac_bypass_total{cn=rio-gateway} >= 1")

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
        matching authorized_keys entry and thus the tenant). Returns
        the store path (last line of output, after SSH warnings +
        build progress lines)."""
        cmd = (
            "nix-build --no-out-link "
            f"--store 'ssh-ng://root@${gatewayHost}?ssh-key={identity_file}' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            f"{drv_path} 2>&1"
        )
        try:
            if expect_fail:
                return client.fail(cmd)
            out = client.succeed(cmd)
            # Last non-empty line is the store path. Earlier lines
            # include SSH known_hosts warning + nix-build progress.
            lines = [l.strip() for l in out.strip().split("\n") if l.strip()]
            return lines[-1] if lines else ""
        except Exception:
            dump_all_logs([${gatewayHost}, worker])
            raise

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
    # so cache-auth + jwt-dual-mode subtests run unconstrained.

    with subtest("rate-limit: 4th rapid build from same tenant rejected"):
        # Drop-in: figment's env layer reads RIO_RATE_LIMIT__PER_MINUTE
        # → rate_limit.per_minute. Both fields must be set (no
        # compiled-in default — r[gw.rate.per-tenant] says
        # workload-dependent). per_minute=2 → one token every 30s;
        # with burst=3, 3 rapid builds drain the bucket and the 4th
        # needs ~30s before the next token — well outside the test's
        # submit loop.
        ${gatewayHost}.succeed(
            "mkdir -p /etc/systemd/system/rio-gateway.service.d && "
            "printf '[Service]\\nEnvironment=RIO_RATE_LIMIT__PER_MINUTE=2\\nEnvironment=RIO_RATE_LIMIT__BURST=3\\n' "
            "> /etc/systemd/system/rio-gateway.service.d/ratelimit.conf && "
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
                f"--store 'ssh-ng://root@${gatewayHost}?ssh-key=/root/.ssh/id_team_test' "
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
            f"rate-limit PASS: 3 submits within burst, 4th rejected "
            f"pre-SubmitBuild with tenant-named error"
        )

        # Teardown: remove the drop-in so the cache-auth subtests
        # below (which may indirectly trigger gateway paths via
        # narinfo serving — they don't, but future subtests might)
        # aren't rate-limited by the leftover burst=3.
        ${gatewayHost}.succeed(
            "rm -f /etc/systemd/system/rio-gateway.service.d/ratelimit.conf && "
            "systemctl daemon-reload && systemctl restart rio-gateway"
        )
        ${gatewayHost}.wait_for_unit("rio-gateway.service")
        ${gatewayHost}.wait_for_open_port(2222)

    # ══════════════════════════════════════════════════════════════════
    # Section: cache-auth (binary-cache HTTP Bearer token)
    # ══════════════════════════════════════════════════════════════════
    # Cache HTTP server on :8080 (default.nix: extraStoreConfig.
    # cacheHttpAddr). Default cache_allow_unauthenticated=false →
    # /{hash}.narinfo requires `Authorization: Bearer <token>`
    # matching tenants.cache_token. /nix-cache-info stays public.
    #
    # Placement AFTER the builds above is load-bearing: the narinfo
    # table must have rows (hmac-positive's build + tenant-resolve's
    # 2 successful builds all did PutPath → narinfo INSERT).

    with subtest("cache-auth: unauth narinfo → 401; with Bearer → 200"):
        ${gatewayHost}.wait_for_open_port(8080)

        # Seed a cache_token on the team-test tenant (inserted at
        # tenant-resolve above, no cache_token). Without at least one
        # non-NULL cache_token row the auth middleware returns 503
        # (misconfiguration) instead of 401 — that's a fail-loud
        # operator guard (cache_server/auth.rs), not the path we're
        # testing here.
        token = "sec-cache-token-1"
        psql(
            ${gatewayHost},
            f"UPDATE tenants SET cache_token = '{token}' "
            f"WHERE tenant_name = 'team-test'",
        )

        # A hash team-test actually OWNS. Authenticated narinfo filters
        # by path_tenants.tenant_id = auth.tenant_id (r[impl store.
        # tenant.narinfo-filter]); `SELECT FROM narinfo LIMIT 1` could
        # pick a NULL-tenant path (hmac-positive / tenant-anon builds
        # → completion hook's filter_map drops → no path_tenants row)
        # which would 404 and false-fail this test.
        #
        # JOIN path_tenants on the team-test UUID. tenant-resolve case 1
        # built sec-tenant-known under id_team_test → completion hook
        # upserted (store_path_hash, team-test-uuid) → this JOIN finds it.
        store_path = psql(
            ${gatewayHost},
            f"SELECT n.store_path FROM narinfo n "
            f"JOIN path_tenants pt USING (store_path_hash) "
            f"WHERE pt.tenant_id = '{tenant_uuid}' LIMIT 1"
        )
        assert store_path.startswith("/nix/store/"), (
            f"team-test should own >=1 path via path_tenants "
            f"(tenant-resolve case 1 builds sec-tenant-known under "
            f"id_team_test → completion hook upserts): {store_path!r}"
        )
        test_hash = store_path.split("/")[-1][:32]
        ${gatewayHost}.log(f"cache-auth: probing hash {test_hash} from {store_path}")

        # Unauth → 401. curl -w '%{http_code}' -o /dev/null → just
        # the status code on stdout. -s: no progress bar noise.
        code = ${gatewayHost}.succeed(
            f"curl -s -o /dev/null -w '%{{http_code}}' "
            f"http://localhost:8080/{test_hash}.narinfo"
        ).strip()
        assert code == "401", (
            f"unauth narinfo expected 401, got {code} "
            f"(503=misconfiguration guard, 200=auth bypassed)"
        )

        # With Bearer → 200. Full end-to-end: token matched in
        # tenants.cache_token → middleware passes → narinfo handler
        # finds the hash → serves the narinfo text.
        code = ${gatewayHost}.succeed(
            f"curl -s -o /dev/null -w '%{{http_code}}' "
            f"-H 'Authorization: Bearer {token}' "
            f"http://localhost:8080/{test_hash}.narinfo"
        ).strip()
        assert code == "200", (
            f"Bearer-auth narinfo expected 200, got {code}"
        )

        # Wrong token → 401. Proves the token is actually being
        # checked (not just "any Authorization header passes").
        code = ${gatewayHost}.succeed(
            f"curl -s -o /dev/null -w '%{{http_code}}' "
            f"-H 'Authorization: Bearer wrong-token' "
            f"http://localhost:8080/{test_hash}.narinfo"
        ).strip()
        assert code == "401", (
            f"wrong-token narinfo expected 401, got {code}"
        )
        print(
            f"cache-auth PASS: unauth→401, Bearer→200, wrong-token→401 "
            f"(hash {test_hash})"
        )

    with subtest("cache-auth-tenant-filter: tenant A cannot narinfo tenant B's path"):
        # Second tenant with its own cache_token. team-test = tenant A,
        # tenant-b = tenant B. We pick a path that has NO path_tenants
        # row (the hmac-positive build ran under id_ed25519 = empty
        # comment = tenant_id NULL → completion hook's filter_map drops
        # the None → upsert skipped), then manually attribute it to B.
        token_b = "sec-cache-token-b"
        tenant_b_uuid = psql(
            ${gatewayHost},
            f"INSERT INTO tenants (tenant_name, cache_token) "
            f"VALUES ('tenant-b', '{token_b}') RETURNING tenant_id",
        )
        ${gatewayHost}.log(f"cache-auth-tenant-filter: tenant-b = {tenant_b_uuid}")

        # Pick a narinfo path NOT in team-test's path_tenants. The
        # NOT EXISTS anti-join excludes anything team-test already
        # owns; whatever's left is either NULL-tenant or someone
        # else's. LIMIT 1 takes any — we'll attribute it to tenant-b.
        other_path = psql(
            ${gatewayHost},
            f"SELECT n.store_path FROM narinfo n "
            f"WHERE NOT EXISTS ("
            f"  SELECT 1 FROM path_tenants pt "
            f"  WHERE pt.store_path_hash = n.store_path_hash "
            f"  AND pt.tenant_id = '{tenant_uuid}'"
            f") LIMIT 1"
        )
        assert other_path.startswith("/nix/store/"), (
            f"need >=1 path team-test does NOT own (hmac/anon builds "
            f"run under NULL tenant → no path_tenants row): {other_path!r}"
        )
        other_hash = other_path.split("/")[-1][:32]

        # Attribute to tenant-b. sha256(convert_to(...)) matches
        # scheduler's sha2::Sha256::digest keying (see lifecycle.nix
        # hash-encoding compat note at path_tenants proof).
        psql(
            ${gatewayHost},
            f"INSERT INTO path_tenants (store_path_hash, tenant_id) "
            f"VALUES (sha256(convert_to('{other_path}', 'UTF8')), '{tenant_b_uuid}')",
        )

        # ── Tenant A (team-test) → 404 ────────────────────────────────
        # 404 not 403: no existence oracle. team-test cannot
        # distinguish "tenant-b owns this" from "doesn't exist".
        code = ${gatewayHost}.succeed(
            f"curl -s -o /dev/null -w '%{{http_code}}' "
            f"-H 'Authorization: Bearer {token}' "
            f"http://localhost:8080/{other_hash}.narinfo"
        ).strip()
        assert code == "404", (
            f"team-test should NOT see tenant-b's path, got {code} "
            f"(200=filter not applied, 401=auth broken)"
        )

        # ── Tenant B → 200 (control) ──────────────────────────────────
        # Without this, a JOIN matching nothing (typo, wrong keying)
        # would pass the 404 above trivially. This proves the 404 is
        # the filter discriminating, not a universal miss.
        code = ${gatewayHost}.succeed(
            f"curl -s -o /dev/null -w '%{{http_code}}' "
            f"-H 'Authorization: Bearer {token_b}' "
            f"http://localhost:8080/{other_hash}.narinfo"
        ).strip()
        assert code == "200", (
            f"tenant-b owns the path → filter passes through, got {code}"
        )

        # ── Tenant A still sees its OWN path → 200 ────────────────────
        # test_hash from the cache-auth subtest above (team-test's
        # path). The filter gates the foreign path but not the owned
        # one — proves we didn't break visibility for legit owners.
        code = ${gatewayHost}.succeed(
            f"curl -s -o /dev/null -w '%{{http_code}}' "
            f"-H 'Authorization: Bearer {token}' "
            f"http://localhost:8080/{test_hash}.narinfo"
        ).strip()
        assert code == "200", (
            f"team-test should still see its own path, got {code}"
        )
        print(
            f"cache-auth-tenant-filter PASS: A→404 on B's path, "
            f"B→200 on own, A→200 on own (hashes {other_hash}, {test_hash})"
        )

    with subtest("cache-auth-nixcacheinfo: /nix-cache-info public"):
        # Nix clients probe /nix-cache-info BEFORE authenticating —
        # they can't know which token to present until they know it's
        # a Nix cache. The route is merged OUTSIDE the auth layer
        # (cache_server/mod.rs router()). Hard assert now that the
        # route split has landed: a regression here breaks
        # `nix build --substituters=…` at discovery.
        code = ${gatewayHost}.succeed(
            "curl -s -o /dev/null -w '%{http_code}' "
            "http://localhost:8080/nix-cache-info"
        ).strip()
        assert code == "200", (
            f"/nix-cache-info must be public (no auth), got {code}"
        )
        # Body sanity — not just any 200, the actual static metadata.
        body = ${gatewayHost}.succeed(
            "curl -s http://localhost:8080/nix-cache-info"
        )
        assert "StoreDir: /nix/store" in body, (
            f"/nix-cache-info body missing StoreDir: {body!r}"
        )
        print("cache-auth-nixcacheinfo PASS: /nix-cache-info 200 unauth")

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
    # Extending this VM fixture with the Secret mount is future work
    # (TODO(P0260): fixture extraServiceEnv for RIO_JWT__KEY_PATH +
    # a pkgs.writeText with a seed). The FALLBACK branch is the
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

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
