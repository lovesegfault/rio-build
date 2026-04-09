# Security scenario: mTLS + HMAC + tenant resolution + gateway validation.
#
# Ports phase3b sections T (mTLS) + B (HMAC) + G (gateway-validate), plus
# phase4 section A (tenant resolution), onto the standalone fixture.
#
# gw.jwt.dual-mode — verify marker at default.nix:vm-security-standalone
# jwt-dual-mode subtest: proves both branches of the PERMANENT
# dual-mode are reachable. SSH-comment branch (signing_key=None —
# the fixture's default) → tenant identity via
# SubmitBuildRequest.tenant_name. The JWT-issue branch is proven
# compile-side by server.rs:resolve_and_mint + jwt_issuance_tests;
# this VM subtest pins the FALLBACK branch under a real
# gateway+scheduler+PG end-to-end.
#
# sec.boundary.grpc-hmac — verify marker at default.nix:vm-security-standalone
# mTLS-reject/-accept + HMAC-verifier prove both halves of the trust
# boundary: TLS terminates at the gRPC port, HMAC gates PutPath.
#
# store.tenant.narinfo-filter — verify marker at default.nix:vm-security-standalone
# cache-auth-tenant-filter subtest: tenant A → 404 on tenant B's path,
# tenant B → 200 on own. The 200 control guards against JOIN-matches-
# nothing (the 404 alone proves nothing if the filter always misses).
#
# gw.reject.nochroot — verify marker at default.nix:vm-security-standalone
# gateway-validate subtest: nix-build a .drv with __noChroot=true via
# ssh-ng://. Gateway rejects with "sandbox escape" pre-SubmitBuild;
# builds row count unchanged proves scheduler never saw it. Exercises
# the validate_dag path (translate.rs:301) — client uploads the .drv to
# the store via wopAddToStoreNar, then wopBuildPathsWithResults triggers
# BFS → drv_cache populated → validate_dag fires on the env entry.
#
# gw.rate.per-tenant — verify marker at default.nix:vm-security-standalone
# rate-limit subtest: configure per_minute=2 burst=3 via systemd
# drop-in, fire 4 rapid builds from the same tenant SSH key → 4th
# gets STDERR_ERROR with "rate limit" body. builds row count unchanged
# on the 4th proves the scheduler never saw it (same pre-SubmitBuild
# gate as gateway-validate).
#
# store.gc.tenant-quota-enforce — verify marker at default.nix:vm-security-standalone
# quota-exceeded subtest: UPDATE tenants SET gc_max_store_bytes=1 →
# attempt build → STDERR_ERROR "over store quota" before SubmitBuild.
# builds row count unchanged proves the scheduler never saw it.
# Positive-control second build with limit raised proves the gate is
# a check not a hard-off switch.
#
# Caller (default.nix) constructs the fixture with:
#   fixture = standalone {
#     workers = { worker = { }; };
#     withPki = true;
#     extraPackages = [ pkgs.grpcurl pkgs.grpc-health-probe pkgs.postgresql ];
#   };
#
# withPki=true → fixture.pki is a store path to lib/pki.nix output
# (ca.crt, server.crt/key, client.crt/key, gateway.crt/key, hmac.key).
# The fixture wires RIO_TLS__* + RIO_HMAC_KEY_PATH via extraServiceEnv;
# gateway gets CN=rio-gateway cert (store PutPath HMAC bypass).
#
# ── privileged-hardening-e2e (k3s fixture, vm-security-nonpriv-k3s) ────
# Separate scenario function: proves MECHANISM of the privileged:false +
# base_runtime_spec /dev/fuse + hostUsers:false production path. The
# standalone scenario above proves auth/mTLS/tenant boundaries; this one
# proves the worker pod security posture is actually FUNCTIONAL (not just
# rendered correctly by the controller). Uses k3sFull fixture with the
# vmtest-full-nonpriv.yaml overlay.
{
  pkgs,
  common,
}:
let
  drvs = import ../lib/derivations.nix { inherit pkgs; };
in
{
  standalone = import ./security/standalone.nix { inherit pkgs common drvs; };
  privileged-hardening-e2e = import ./security/privileged-hardening-e2e.nix {
    inherit pkgs common drvs;
  };
}
