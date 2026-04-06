# Upstream substitution: fake binary cache → rio-store block-and-fetch.
#
# Validates the P0462/P0463 chain: tenant_upstreams → Substituter HTTP
# fetch → sig-verify → CAS ingest → narinfo row. Plus sig_mode handling
# and cross-tenant visibility.
#
# ── Dual approach: grpcurl + ssh-ng ────────────────────────────────────
# substitute-ssh-ng: the REAL protocol path — ssh-ng → gateway →
# wopQueryPathInfo → store. P0465 wired JWT propagation through
# opcodes_read.rs so the session JWT reaches the store's
# try_substitute_on_miss. Before P0465, only build.rs (SubmitBuild)
# attached the JWT; read-opcodes saw anonymous → substitution skipped.
#
# substitute-{cold-fetch,sig-mode-add,cross-tenant-gate}: grpcurl with
# self-signed JWTs for MULTI-TENANT scenarios (three tenants, three
# trust configs). ssh-ng would need per-tenant SSH keys + authorized_keys
# gymnastics; grpcurl is the sharper tool for store-side tenant-gate
# assertions. Same Substituter codepath, tighter test surface.
#
# ── Fake upstream mechanics ────────────────────────────────────────────
# Generated at VM RUNTIME on the client node using Nix's own tooling:
#   nix key generate-secret → test signing key
#   nix-store --add        → fixed-content store path
#   nix store sign         → sign with test key
#   nix copy --to file://  → valid narinfo + nar on disk
#   python -m http.server  → serve it
#
# Simpler than build-time NAR/narinfo fabrication (no manual fingerprint
# computation, no manual NAR encoding — Nix does it correctly). The path
# is content-addressed (fixed input → fixed hash) so it's deterministic
# across runs.
#
# store.substitute.{upstream,sig-mode,tenant-sig-visibility} — verify
# markers at default.nix:vm-substitute-standalone subtests entries
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) gatewayHost;

  jwtKeys = import ../lib/jwt-keys.nix;
  protoset = import ../lib/protoset.nix { inherit pkgs; };

  rioCli = "${common.rio-workspace}/bin/rio-cli";
  grpcurl = "${pkgs.grpcurl}/bin/grpcurl";

  # PyJWT for signing the test tenant token. Packaged so VM-closure
  # pull-in via store-path interpolation works. cryptography provides
  # the ed25519 backend PyJWT uses for EdDSA.
  pyWithJwt = pkgs.python3.withPackages (
    ps: with ps; [
      pyjwt
      cryptography
    ]
  );

  # Sign a JWT with the jwt-keys.nix test seed. Output on stdout so
  # testScript captures into a Python var. PyJWT's EdDSA wants a PEM
  # private key (not raw 32-byte seed), so we wrap via cryptography.
  signJwt = pkgs.writeScript "sign-jwt" ''
    #!${pyWithJwt}/bin/python3
    import sys, time, base64, jwt
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    seed = base64.b64decode("${jwtKeys.seedB64}")
    sk = Ed25519PrivateKey.from_private_bytes(seed)
    now = int(time.time())
    claims = {"sub": sys.argv[1], "iat": now, "exp": now + 3600, "jti": "vm-sub-test"}
    print(jwt.encode(claims, sk, algorithm="EdDSA"))
  '';
in
pkgs.testers.runNixOSTest {
  name = "rio-substitute";
  skipTypeCheck = true;

  # ~60s boot + cache setup ~10s + three grpcurl round-trips. No builds
  # through the scheduler, no k3s.
  globalTimeout = 420 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

    ${common.kvmCheck}
    start_all()
    ${fixture.waitReady}

    # ══════════════════════════════════════════════════════════════════
    # Fake upstream: generate narinfo+nar on client, serve via http
    # ══════════════════════════════════════════════════════════════════
    # Fixed content → fixed store path hash (deterministic across runs).
    # nix copy --to file:// writes a valid binary-cache layout:
    #   <hash>.narinfo + nar/<filehash>.nar.xz. compression=none avoids
    # the xz dependency on the store side (Substituter decompresses
    # based on narinfo's Compression: line; none is simplest).
    client.succeed("mkdir -p /srv/cache /tmp/sub")
    client.succeed("echo rio-substitute-fixture-v1 > /tmp/sub/payload")
    sub_path = client.succeed("nix-store --add /tmp/sub/payload").strip()
    assert sub_path.startswith("/nix/store/"), f"unexpected: {sub_path!r}"
    hash_part = sub_path.removeprefix("/nix/store/").split("-", 1)[0]
    print(f"substitute: test path {sub_path} (hash_part={hash_part})")

    # Test signing key. `nix key generate-secret` writes `name:base64seed`
    # to stdout (nix secret-key format, same as rio's Signer::parse).
    client.succeed(
        "nix key generate-secret --key-name test-cache-1 > /tmp/sub/sec && "
        "nix key convert-secret-to-public < /tmp/sub/sec > /tmp/sub/pub"
    )
    test_pubkey = client.succeed("cat /tmp/sub/pub").strip()
    assert test_pubkey.startswith("test-cache-1:"), f"bad pubkey: {test_pubkey!r}"

    # Sign + copy to file cache. --no-check-sigs because the source
    # (client's local store) isn't signed; nix copy verifies sigs by
    # default on the READ side.
    client.succeed(f"nix store sign --key-file /tmp/sub/sec {sub_path}")
    client.succeed(
        f"nix copy --no-check-sigs "
        f"--to 'file:///srv/cache?compression=none' {sub_path}"
    )
    # Sanity: narinfo exists and carries the test sig.
    narinfo = client.succeed(f"cat /srv/cache/{hash_part}.narinfo")
    assert "Sig: test-cache-1:" in narinfo, (
        f"narinfo missing test-cache-1 sig:\n{narinfo}"
    )

    # http.server on :8080. Firewall open via extraClientModules. The
    # store (on control) fetches http://client:8080/{hash}.narinfo.
    # systemd-run detaches cleanly — `... & echo $!` leaves the test
    # driver's pipe open (succeed() reads until EOF → hangs forever).
    # Same pattern as fetcher-split.nix.
    client.succeed(
        "systemd-run --unit=test-cache "
        "${pkgs.python3}/bin/python3 -m http.server 8080 "
        "--bind 0.0.0.0 --directory /srv/cache"
    )
    client.wait_for_open_port(8080)
    # Positive control: control can reach the upstream. If this fails,
    # firewall/routing is broken and every subsequent substitute-miss
    # is VACUOUS (NetPol-style proves-nothing guard).
    ${gatewayHost}.succeed(
        f"curl -sf http://client:8080/{hash_part}.narinfo | "
        "grep -q 'Sig: test-cache-1:'"
    )

    # ══════════════════════════════════════════════════════════════════
    # Tenant + JWT setup
    # ══════════════════════════════════════════════════════════════════
    # Three tenants for the cross-tenant gate subtest, plus one for the
    # ssh-ng end-to-end path. Direct psql() (from common.assertions) —
    # rio-cli create-tenant goes through the scheduler's AdminService
    # which we don't need here (pure store-side test).
    def mk_tenant(name):
        return psql(
            ${gatewayHost},
            f"INSERT INTO tenants (tenant_name) VALUES ('{name}') "
            "RETURNING tenant_id",
        )
    tid_a = mk_tenant("sub-tenant-a")  # trusts test-cache-1, sig_mode=keep
    tid_b = mk_tenant("sub-tenant-b")  # trusts test-cache-1 (cross-tenant)
    tid_c = mk_tenant("sub-tenant-c")  # trusts WRONG key → gated out
    tid_ssh = mk_tenant("sub-tenant-ssh")  # ssh-ng end-to-end path
    print(f"substitute: tenants A={tid_a} B={tid_b} C={tid_c} ssh={tid_ssh}")

    # Sign per-tenant JWTs. The store's jwt_interceptor verifies
    # against the pubkey at RIO_JWT__KEY_PATH (set via extraServiceEnv
    # in default.nix's fixture wiring).
    def jwt_for(tid):
        return ${gatewayHost}.succeed(f"${signJwt} {tid}").strip()
    jwt_a = jwt_for(tid_a)
    jwt_c = jwt_for(tid_c)

    # ══════════════════════════════════════════════════════════════════
    # rio-cli upstream add — StoreAdminService CRUD
    # ══════════════════════════════════════════════════════════════════
    # covShellEnv sets LLVM_PROFILE_FILE in coverage mode so rio-cli's
    # profraws land where collectCoverage picks them up.
    def cli(args):
        return ${gatewayHost}.succeed(
            "${common.covShellEnv}"
            "RIO_STORE_ADDR=localhost:9002 "
            f"${rioCli} {args} 2>&1"
        )

    # Tenant A: trust test-cache-1, sig_mode=keep (upstream sig only).
    out = cli(
        f"upstream add --tenant {tid_a} "
        "--url http://client:8080 --priority 50 "
        f"--trusted-key '{test_pubkey}' --sig-mode keep"
    )
    print(f"upstream add (A, keep):\n{out}")
    assert "added upstream http://client:8080" in out, out

    # Tenant B: same key, different URL string (visibility gate only
    # checks trusted_keys UNION, not URL match).
    cli(
        f"upstream add --tenant {tid_b} "
        "--url http://client:8080 --priority 50 "
        f"--trusted-key '{test_pubkey}' --sig-mode keep"
    )

    # Tenant C: trusts a DIFFERENT key. Substituted-by-A paths should
    # be invisible to C (sig doesn't verify against C's trusted_keys).
    # AddUpstream validates pubkey shape (32-byte ed25519) — generate
    # a real-but-unrelated key, not a filler string.
    client.succeed(
        "nix key generate-secret --key-name wrong-key > /tmp/sub/wrong-sec && "
        "nix key convert-secret-to-public < /tmp/sub/wrong-sec > /tmp/sub/wrong-pub"
    )
    wrong_pubkey = client.succeed("cat /tmp/sub/wrong-pub").strip()
    cli(
        f"upstream add --tenant {tid_c} "
        "--url http://client:8080 --priority 50 "
        f"--trusted-key '{wrong_pubkey}' --sig-mode keep"
    )

    # List round-trip (exercises ListUpstreams).
    out = cli(f"upstream list --tenant {tid_a}")
    assert "http://client:8080" in out and "keep" in out, (
        f"upstream list missing entry:\n{out}"
    )

    # ══════════════════════════════════════════════════════════════════
    # grpcurl helper — QueryPathInfo with JWT header
    # ══════════════════════════════════════════════════════════════════
    # -plaintext: standalone fixture doesn't run mTLS on store (no
    # withPki). protoset: rio servers don't register tonic-reflection.
    def query_path_info(token, path):
        return ${gatewayHost}.execute(
            f"${grpcurl} -plaintext -max-time 30 "
            f"-protoset ${protoset}/rio.protoset "
            f"-H 'x-rio-tenant-token: {token}' "
            f'-d \'{{"store_path":"{path}"}}\' '
            "localhost:9002 rio.store.StoreService/QueryPathInfo 2>&1"
        )

    # ══════════════════════════════════════════════════════════════════
    # substitute-cold-fetch — miss → upstream fetch → ingest
    # ══════════════════════════════════════════════════════════════════
    with subtest("substitute-cold-fetch: miss → fetch → ingest → narinfo row"):
        # Precondition: store is cold (no narinfo row for this path).
        before = psql(
            ${gatewayHost},
            f"SELECT count(*) FROM narinfo WHERE store_path = '{sub_path}'",
        )
        assert before == "0", (
            f"precondition FAIL: narinfo already has {before} row(s) for "
            f"{sub_path} — store not cold. Substitute-hit below is VACUOUS."
        )

        # QueryPathInfo as tenant A. Miss → try_substitute_on_miss →
        # HTTP GET narinfo → verify sig → GET nar → CAS ingest →
        # narinfo INSERT → return PathInfo.
        rc, out = query_path_info(jwt_a, sub_path)
        assert rc == 0, f"QueryPathInfo failed (rc={rc}):\n{out}"
        # grpcurl JSON output → storePath field. QueryPathInfo returns
        # PathInfo directly (not wrapped in .info).
        resp = json.loads(out)
        assert resp.get("storePath") == sub_path, (
            f"expected storePath={sub_path!r} in response:\n{out}"
        )
        print(f"substitute-cold-fetch PASS: ingested {sub_path}")

        # Ingest proof: narinfo row exists with upstream's sig.
        sigs = psql(
            ${gatewayHost},
            f"SELECT signatures FROM narinfo WHERE store_path = '{sub_path}'",
        )
        assert "test-cache-1:" in sigs, (
            f"narinfo.signatures missing upstream sig:\n{sigs}"
        )

        # Metric proof: rio_store_substitute_total{result="hit",...} ≥ 1.
        # Store metrics on :9092 (scheduler is :9091, builder :9093).
        # upstream label added to the metric — assert_metric_ge does
        # exact label-string match, so include it.
        assert_metric_ge(
            ${gatewayHost}, 9092,
            "rio_store_substitute_total", 1.0,
            labels='{result="hit",upstream="http://client:8080"}',
        )

    # ══════════════════════════════════════════════════════════════════
    # substitute-sig-mode-add — upstream sig + rio sig
    # ══════════════════════════════════════════════════════════════════
    with subtest("substitute-sig-mode-add: keep upstream + add rio sig"):
        # Second path with distinct content → distinct hash_part.
        client.succeed("echo rio-substitute-fixture-v2-add > /tmp/sub/payload2")
        sub_path2 = client.succeed("nix-store --add /tmp/sub/payload2").strip()
        client.succeed(f"nix store sign --key-file /tmp/sub/sec {sub_path2}")
        client.succeed(
            f"nix copy --no-check-sigs "
            f"--to 'file:///srv/cache?compression=none' {sub_path2}"
        )
        # http.server serves the new file automatically (directory listing).

        # New tenant with sig_mode=add. The store's Substituter holds a
        # TenantSigner iff signingKeyFile is configured — the fixture
        # wires it via extraStoreConfig in default.nix.
        tid_add = mk_tenant("sub-tenant-add")
        cli(
            f"upstream add --tenant {tid_add} "
            "--url http://client:8080 --priority 50 "
            f"--trusted-key '{test_pubkey}' --sig-mode add"
        )
        jwt_add = jwt_for(tid_add)

        rc, out = query_path_info(jwt_add, sub_path2)
        assert rc == 0, f"QueryPathInfo (sig_mode=add) failed:\n{out}"

        sigs = psql(
            ${gatewayHost},
            f"SELECT signatures FROM narinfo WHERE store_path = '{sub_path2}'",
        )
        # sig_mode=add → BOTH upstream AND rio sigs. The rio sig's key
        # name is the store's signingKeyFile name (rio-vm-test-1, set
        # via extraStoreConfig in default.nix).
        assert "test-cache-1:" in sigs, f"missing upstream sig: {sigs}"
        assert "rio-vm-test-1:" in sigs, (
            f"sig_mode=add should append rio sig (signingKeyFile name "
            f"rio-vm-test-1): {sigs}"
        )

    # ══════════════════════════════════════════════════════════════════
    # substitute-cross-tenant-gate — sig-visibility across tenants
    # ══════════════════════════════════════════════════════════════════
    with subtest("substitute-cross-tenant-gate: C can't see A's substituted path"):
        # sub_path was substituted under tenant A (sig: test-cache-1).
        # Zero path_tenants rows (substitution doesn't populate it —
        # only build-completion does). C trusts ONLY 'wrong-key' →
        # sig_visibility_gate fails → NotFound.
        rc, out = query_path_info(jwt_c, sub_path)
        assert rc != 0, (
            f"tenant C (untrusted key) should get NotFound for "
            f"A-substituted {sub_path}, got rc=0:\n{out}"
        )
        assert "NotFound" in out or "not found" in out.lower(), (
            f"expected NotFound status:\n{out}"
        )
        print(f"cross-tenant gate PASS: C blocked from {sub_path}")

        # Positive control: B DOES trust test-cache-1 → visible.
        # Same path, different tenant context — if this also fails,
        # the gate is over-broad (blocks everyone, proves nothing).
        jwt_b = jwt_for(tid_b)
        rc, out = query_path_info(jwt_b, sub_path)
        assert rc == 0, (
            f"tenant B (trusts test-cache-1) should see A-substituted "
            f"path — gate over-broad if blocked:\n{out}"
        )

        # Dynamic re-trust: add test-cache-1 to C's upstream → C sees it.
        # Proves the gate re-reads tenant_trusted_keys per-request (no
        # stale cache).
        cli(
            f"upstream add --tenant {tid_c} "
            "--url http://client-alt:8080 --priority 60 "
            f"--trusted-key '{test_pubkey}' --sig-mode keep"
        )
        rc, out = query_path_info(jwt_c, sub_path)
        assert rc == 0, (
            f"after adding test-cache-1 to C's trusted_keys, path "
            f"should be visible:\n{out}"
        )

    # ══════════════════════════════════════════════════════════════════
    # substitute-ssh-ng — gateway JWT propagation end-to-end
    # ══════════════════════════════════════════════════════════════════
    with subtest("substitute-ssh-ng: gateway propagates JWT through wopQueryPathInfo"):
        # Fresh path so the ssh-ng request is a cold miss → triggers
        # try_substitute_on_miss at the store. Reusing sub_path would
        # hit the warm narinfo row from substitute-cold-fetch above,
        # bypassing the substituter entirely.
        client.succeed("echo rio-substitute-fixture-v3-sshng > /tmp/sub/payload3")
        sub_path3 = client.succeed("nix-store --add /tmp/sub/payload3").strip()
        client.succeed(f"nix store sign --key-file /tmp/sub/sec {sub_path3}")
        client.succeed(
            f"nix copy --no-check-sigs "
            f"--to 'file:///srv/cache?compression=none' {sub_path3}"
        )

        # Tenant-ssh trusts the test cache. No sig_mode complications;
        # this subtest proves JWT PROPAGATION, not sig handling.
        cli(
            f"upstream add --tenant {tid_ssh} "
            "--url http://client:8080 --priority 50 "
            f"--trusted-key '{test_pubkey}' --sig-mode keep"
        )

        # SSH key with tenant NAME in the comment. Gateway extracts
        # the comment → scheduler.resolve_tenant → UUID → mint JWT.
        # -C 'sub-tenant-ssh' (not the UUID — gateway is PG-free,
        # scheduler owns name→UUID resolution per r[sched.tenant.resolve]).
        #
        # Key MUST be at /root/.ssh/id_ed25519 — mkClientNode's
        # ssh_config hardcodes that IdentityFile (common.nix:464) and
        # the ?ssh-key= querystring is unreliable across Nix versions
        # (common.nix:457). authorized_keys write uses > (overwrite, not
        # append) because tmpfiles seeds the placeholder without a
        # trailing newline — >> would glue our key to it.
        client.succeed(
            "mkdir -p /root/.ssh && "
            "ssh-keygen -t ed25519 -N ''' -C 'sub-tenant-ssh' "
            "-f /root/.ssh/id_ed25519"
        )
        sub_pubkey = client.succeed("cat /root/.ssh/id_ed25519.pub").strip()
        ${gatewayHost}.succeed(
            f"echo '{sub_pubkey}' > /var/lib/rio/gateway/authorized_keys"
        )
        ${gatewayHost}.succeed("systemctl restart rio-gateway.service")
        ${gatewayHost}.wait_for_unit("rio-gateway.service")
        ${gatewayHost}.wait_for_open_port(2222)

        # Precondition: store is cold for this path (no narinfo row).
        # Without this guard, a prior-test ingest makes the ssh-ng
        # success VACUOUS — it'd be a warm hit, not a substitute.
        before = psql(
            ${gatewayHost},
            f"SELECT count(*) FROM narinfo WHERE store_path = '{sub_path3}'",
        )
        assert before == "0", (
            f"precondition FAIL: narinfo already has {before} row(s) for "
            f"{sub_path3} — store not cold. ssh-ng hit below is VACUOUS."
        )

        # THE ACTUAL TEST: nix path-info via ssh-ng → gateway
        # wopQueryPathInfo → store QueryPathInfo WITH x-rio-tenant-token
        # → try_substitute_on_miss → upstream fetch → PathInfo returned.
        #
        # Before P0465: gateway didn't attach JWT → store saw anonymous
        # → substitute short-circuited → NotFound → nix path-info failed.
        # After P0465: JWT propagated → tenant-scoped substitute fires.
        #
        # No ?ssh-key= querystring — ssh_config's IdentityFile handles it.
        out = client.succeed(
            f"nix path-info --store 'ssh-ng://${gatewayHost}' {sub_path3} 2>&1"
        )
        assert sub_path3 in out, (
            f"ssh-ng path-info should return the substituted path. "
            f"If this fails with 'not valid', the gateway's JWT "
            f"propagation through opcodes_read.rs is broken — store "
            f"saw anonymous request, try_substitute_on_miss skipped.\n"
            f"output:\n{out}"
        )
        print(f"substitute-ssh-ng PASS: {sub_path3} visible via ssh-ng")

        # Ingest proof: narinfo row now exists (was 0, now 1) —
        # confirms the ssh-ng request triggered the substitute, not
        # just a cache-hit or error-masked-as-success.
        after = psql(
            ${gatewayHost},
            f"SELECT count(*) FROM narinfo WHERE store_path = '{sub_path3}'",
        )
        assert after == "1", (
            f"expected narinfo row after ssh-ng substitute, got {after} — "
            f"did try_substitute_on_miss actually fire?"
        )

    client.execute("systemctl stop test-cache 2>/dev/null || true")

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
