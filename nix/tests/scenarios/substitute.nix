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

  # 4-path closure for the substitute-progress-e2e subtest. Chain:
  # root → mid → {leaf-a, leaf-b}. Building `root` locally on client
  # produces 4 input-addressed store paths (busybox is static, closure
  # of 1); copying that closure to /srv/cache and submitting the SAME
  # expr via ssh-ng makes the scheduler see all 4 outputs as
  # substitutable → walk_substitute_closure walks the references →
  # actCopyPath + resProgress events on the wire.
  #
  # Function `{ busybox }:` so Nix tracks busybox as a build input
  # (a literal string path has no context → "No such file" in sandbox).
  # Same `--arg busybox '(builtins.storePath ${common.busybox})'`
  # pattern as substitute-scale.nix.
  progressClosure = pkgs.writeText "progress-closure.nix" ''
    { busybox }:
    let
      sh = "''${busybox}/bin/sh";
      bb = "''${busybox}/bin/busybox";
      mk = name: deps: derivation {
        inherit name;
        system = builtins.currentSystem;
        builder = sh;
        args = [
          "-c"
          "''${bb} mkdir -p $out && ''${bb} echo ''${name}-v1 ''${toString deps} > $out/x"
        ];
      };
      leafA = mk "rio-progress-leaf-a" [ ];
      leafB = mk "rio-progress-leaf-b" [ ];
      mid = mk "rio-progress-mid" [ leafA leafB ];
    in mk "rio-progress-root" [ mid ]
  '';
  bbArg = "--arg busybox '(builtins.storePath ${common.busybox})'";

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

  # ~60s boot + cache setup ~10s + grpcurl round-trips + ssh-ng build
  # for substitute-progress-e2e (~40s). No worker builds, no k3s.
  globalTimeout = 480 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.mkBootstrap {
      inherit fixture;
      withSsh = false;
    }}

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
    # RIO_SERVICE_HMAC_KEY_PATH: withHmac=true makes StoreAdminService
    # require x-rio-service-token; rio-cli mints it from this key.
    def cli(args):
        return ${gatewayHost}.succeed(
            "${common.covShellEnv}"
            "RIO_STORE_ADDR=localhost:9002 "
            "RIO_SERVICE_HMAC_KEY_PATH=${fixture.hmacKeys}/service-hmac.key "
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
    # -plaintext: services run plaintext gRPC (Cilium WireGuard handles
    # transport). protoset: rio servers don't register tonic-reflection.
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
        # Label is `tenant` (UUID, bounded), NOT `upstream` (tenant-
        # supplied URL → unbounded cardinality). assert_metric_ge does
        # exact label-string match.
        assert_metric_ge(
            ${gatewayHost}, 9092,
            "rio_store_substitute_total", 1.0,
            labels='{result="hit",tenant="' + tid_a + '"}',
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

        # ── Gate covers ALL tenant-facing read RPCs, not just
        #    QueryPathInfo. Pre-fix: 5/6 leaked. ───────────────────────
        # FindMissingPaths: gate-failed → reported missing (else
        # scheduler launders into path_tenants).
        rc, out = ${gatewayHost}.execute(
            f"${grpcurl} -plaintext -max-time 30 "
            f"-protoset ${protoset}/rio.protoset "
            f"-H 'x-rio-tenant-token: {jwt_c}' "
            f'-d \'{{"store_paths":["{sub_path}"]}}\' '
            "localhost:9002 rio.store.StoreService/FindMissingPaths 2>&1"
        )
        assert rc == 0, f"FindMissingPaths rpc failed:\n{out}"
        assert sub_path in out, (
            f"FindMissingPaths: gate-failed path must be in "
            f"missing_paths for tenant C:\n{out}"
        )
        # BatchGetManifest: end-user tenant token → PermissionDenied.
        rc, out = ${gatewayHost}.execute(
            f"${grpcurl} -plaintext -max-time 30 "
            f"-protoset ${protoset}/rio.protoset "
            f"-H 'x-rio-tenant-token: {jwt_c}' "
            f'-d \'{{"store_paths":["{sub_path}"]}}\' '
            "localhost:9002 rio.store.StoreService/BatchGetManifest 2>&1"
        )
        assert rc != 0 and "PermissionDenied" in out, (
            f"BatchGetManifest: end-user tenant token should be "
            f"rejected:\n{out}"
        )
        # GetPath: same gate as QueryPathInfo → NotFound.
        rc, out = ${gatewayHost}.execute(
            f"${grpcurl} -plaintext -max-time 30 "
            f"-protoset ${protoset}/rio.protoset "
            f"-H 'x-rio-tenant-token: {jwt_c}' "
            f'-d \'{{"store_path":"{sub_path}"}}\' '
            "localhost:9002 rio.store.StoreService/GetPath 2>&1"
        )
        assert rc != 0 and ("NotFound" in out or "not found" in out.lower()), (
            f"GetPath: tenant C should get NotFound:\n{out}"
        )
        print("cross-tenant gate PASS: all read RPCs gated for C")

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
    # built-path-cross-tenant-gate — I-217: BUILT path tenant isolation
    # ══════════════════════════════════════════════════════════════════
    with subtest("built-path-cross-tenant-gate: B can't see A's BUILT path (I-217)"):
        # The subtest above proves the gate for SUBSTITUTED paths (zero
        # path_tenants rows → sig-verify). I-217: once a path has a
        # path_tenants row (built locally), it MUST be visible only to
        # owning tenants — NOT to "anyone, since it's built" (the pre-
        # I-217 leak), and NOT via sig-trust fallback (built-by-another
        # takes precedence over sig-verify).
        #
        # Seed a path_tenants row for sub_path under tenant A. In a full
        # k3s fixture this happens via scheduler upsert_path_tenants on
        # build-completion; the standalone fixture has no scheduler, so
        # we INSERT directly. The store-side gate (what I-217 fixes) is
        # the same either way.
        psql(
            ${gatewayHost},
            "INSERT INTO path_tenants (store_path_hash, tenant_id) "
            f"VALUES (sha256('{sub_path}'::bytea), '{tid_a}')",
        )

        # Fresh tenant D: no upstream, trusts nothing. The pre-I-217
        # any_built bypass would have made sub_path visible to D anyway.
        tid_d = mk_tenant("sub-tenant-d")
        jwt_d = jwt_for(tid_d)

        # A owns it → visible.
        rc, out = query_path_info(jwt_a, sub_path)
        assert rc == 0, (
            f"owner A must see its own built path; gate over-broad:\n{out}"
        )

        # D doesn't own it, trusts no sig → HIDDEN.
        rc, out = query_path_info(jwt_d, sub_path)
        assert rc != 0 and ("NotFound" in out or "not found" in out.lower()), (
            f"I-217: D (not owner, no trust) must get NotFound for A's "
            f"BUILT path. Pre-fix any_built bypass leaked this:\n{out}"
        )

        # B/C trust test-cache-1 AND have it as upstream. The gate hides
        # A's row from them (built-by-another), but QueryPathInfo then
        # tries try_substitute_on_miss → B/C's upstream HAS sub_path →
        # they get it via THEIR OWN substitution. Correct isolation: B
        # substitutes independently, not via A's build leak. The I-217
        # regression is D's case above (no upstream → no fallback).
        rc, out = query_path_info(jwt_b, sub_path)
        assert rc == 0, (
            f"B (has upstream test-cache-1) gets sub_path via own "
            f"substitution fallback (gate hides A's row, B substitutes "
            f"independently):\n{out}"
        )

        # Anonymous → unfiltered (spec carve-out, unchanged by I-217).
        rc, out = ${gatewayHost}.execute(
            f"${grpcurl} -plaintext -max-time 30 "
            f"-protoset ${protoset}/rio.protoset "
            f'-d \'{{"store_path":"{sub_path}"}}\' '
            "localhost:9002 rio.store.StoreService/QueryPathInfo 2>&1"
        )
        assert rc == 0, (
            f"anonymous (no x-rio-tenant-token) → unfiltered per "
            f"r[store.tenant.narinfo-filter]:\n{out}"
        )
        print("built-path cross-tenant gate PASS: A sees, B/C/D blocked")

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

    with subtest(
        "substitute-progress-e2e: actCopyPath stop-parity, resProgress done≤expected monotone"
    ):
        import json

        # Build the 4-path chain locally on client → root + mid +
        # 2 leaves (+ busybox already present). Input-addressed: same
        # busybox path → same drvs → same output paths the gateway will
        # compute on submit.
        # --impure: builtins.currentSystem + builtins.storePath are
        # impure under flakes-mode eval. Same --arg busybox pattern as
        # substitute-scale.nix (literal string path has no context →
        # "No such file" in sandbox).
        root_path = client.succeed(
            "nix build --impure --print-out-paths "
            "${bbArg} -f ${progressClosure}"
        ).strip()
        client.succeed(f"nix store sign --key-file /tmp/sub/sec --recursive {root_path}")
        client.succeed(
            "nix copy --no-check-sigs "
            f"--to 'file:///srv/cache?compression=none' {root_path}"
        )
        # Precondition: rio-store is cold for the root (no narinfo row).
        before = psql(
            ${gatewayHost},
            f"SELECT count(*) FROM narinfo WHERE store_path = '{root_path}'",
        )
        assert before == "0", (
            f"precondition FAIL: {root_path} already in rio-store; "
            f"progress events would be skipped (cache-hit lane)"
        )

        # Submit via ssh-ng with internal-json on stderr → cap.json. The
        # tee preserves the capture even if nom/nix exits oddly. The
        # build SUCCEEDS without any worker — all 4 drvs' outputs are
        # in upstream → walk_substitute_closure → Cached.
        client.succeed(
            "nix build --impure --no-link "
            "${bbArg} -f ${progressClosure} "
            "  --store 'ssh-ng://${gatewayHost}' --eval-store auto "
            "  --log-format internal-json -v "
            "  2> >(tee /tmp/cap.json >&2)"
        )
        cap = client.succeed("cat /tmp/cap.json")
        events = []
        for line in cap.splitlines():
            line = line.strip()
            # nix internal-json prefixes each line with "@nix "
            if line.startswith("@nix "):
                line = line[5:]
            if line.startswith("{"):
                try:
                    events.append(json.loads(line))
                except json.JSONDecodeError:
                    pass
        assert len(events) > 0, f"no JSON events captured; cap.json:\n{cap[:2000]}"

        # actCopyPath = type 100. resProgress = result type 105.
        copy_starts = {
            e["id"] for e in events if e.get("action") == "start" and e.get("type") == 100
        }
        all_stops = {e["id"] for e in events if e.get("action") == "stop"}
        leaked = copy_starts - all_stops
        assert not leaked, (
            f"actCopyPath stop-parity: {len(leaked)} start(s) without stop: {leaked}\n"
            f"starts={copy_starts} stops={all_stops}"
        )
        assert len(copy_starts) >= 1, (
            "expected ≥1 actCopyPath start (one per Substituting drv); "
            "got 0 — did walk_substitute_closure run? events:\n"
            + "\n".join(str(e) for e in events[:50])
        )

        # resProgress invariants per-aid: done≤expected, done monotone.
        last_done: dict[int, int] = {}
        n_progress = 0
        for e in events:
            if e.get("action") != "result" or e.get("type") != 105:
                continue
            n_progress += 1
            aid, fields = e["id"], e["fields"]
            done, expected = fields[0], fields[1]
            assert done <= expected, (
                f"resProgress >100%: aid={aid} done={done} expected={expected}\n"
                f"full event: {e}"
            )
            prev = last_done.get(aid, -1)
            assert done >= prev, (
                f"resProgress went backward: aid={aid} {prev} → {done}\n"
                f"full event: {e}"
            )
            last_done[aid] = done
        print(
            f"substitute-progress-e2e PASS: "
            f"{len(copy_starts)} actCopyPath start/stop matched, "
            f"{n_progress} resProgress events all done≤expected and monotone"
        )

    client.execute("systemctl stop test-cache 2>/dev/null || true")

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
