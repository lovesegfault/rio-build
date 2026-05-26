# Security scenario: HMAC + tenant resolution + gateway validation.
#
# Ports phase3b section B (HMAC) + G (gateway-validate), plus phase4
# section A (tenant resolution), onto the standalone fixture. Transport
# encryption is Cilium WireGuard (no app-level mTLS).
#
# gw.jwt.dual-mode — verify markers split across the two branches:
# the MINT branch is this scenario's jwt-dual-mode subtest (marker at
# default.nix:vm-security-standalone; the fixture runs withJwt, which
# the castore cutover requires anyway so the gateway's pushes are
# attributed to the tenant) — it pins that the attested JWT identity
# and the SubmitBuildRequest.tenant_name body fallback resolve to the
# same tenant for the same key. The JWT-less FALLBACK branch is
# attributed to the unit tests that exercise it directly, which carry
# their own r[verify] markers for the rule:
#   - rio-auth jwt_interceptor.rs::absent_header_passes_through
#     (absent header → pass-through, no Claims attached)
#   - rio-scheduler submit_tests.rs::test_submit_build_resolves_known_tenant
#     (claims-less SubmitBuild → tenant_name body → builds.tenant_id)
# plus the k3s prod-parity wiring (jwtEnabled=false) end-to-end.
#
# sec.boundary.grpc-hmac — verify marker at default.nix:vm-security-standalone
# HMAC-verifier proves the trust boundary: service-HMAC gates the
# gateway PutPath bypass; assignment-HMAC gates worker PutPath.
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
#     extraPackages = [ pkgs.grpcurl pkgs.grpc-health-probe pkgs.postgresql ];
#   };
#
# (hmac.key, service-hmac.key).
# The fixture wires RIO_HMAC_KEY_PATH + RIO_SERVICE_HMAC_KEY_PATH via
# extraServiceEnv; gateway PutPath bypass is via x-rio-service-token.
#
# ── privileged-hardening-e2e (k3s fixture, vm-security-nonpriv-k3s) ────
# Separate scenario function: proves MECHANISM of the privileged:false +
# base_runtime_spec /dev/fuse + hostUsers:false production path. The
# standalone scenario above proves auth/HMAC/tenant boundaries; this one
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
