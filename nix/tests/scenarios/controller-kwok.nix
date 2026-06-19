# Minimal-prelude lifecycle harness for the kwok-only fixture (issue #57
# 1d). Reuses the scenarios/lifecycle/*.nix fragment files (same `scope`
# shape) but replaces lifecycle.nix's k3s-pod-coupled prelude
# (pods/proxy metric scrapes, port-forward, JWT/HMAC mint via Secret)
# with systemd/localhost equivalents.
#
# Only the fragments named in default.nix's vm-lifecycle-{pool,autoscale}
# -kwok-only `subtests` lists are EVALUATED here — `lib.genAttrs` over
# `enabledFragments` instead of `mapAttrs` over the whole directory,
# because most lifecycle fragments interpolate `${cancelDrv}` /
# `${recoveryDrv}` / … at the Nix level and would fail eval on the
# stubbed scope. Adding a fragment to a kwok-only test means adding its
# name here AND supplying any scope vars it interpolates.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (pkgs) lib;
  inherit (fixture)
    ns
    nsStore
    nsBuilders
    nsFetchers
    ;
  drvs = import ../lib/derivations.nix { inherit pkgs; };

  ephemeralDrv1 = drvs.mkTrivial { marker = "kwok-ephemeral-1"; };
  ephemeralDrv2 = drvs.mkTrivial { marker = "kwok-ephemeral-2"; };

  prelude = ''
    ${common.mkBootstrap {
      inherit fixture;
      withSeed = true;
    }}

    # Scheduler metrics: rio-scheduler is a co-located systemd unit →
    # curl localhost:9091 directly (no pods/proxy, no port-forward).
    def sched_metric_wait(condition, timeout=60):
        k3s_server.wait_until_succeeds(
            f"curl -fsS http://localhost:9091/metrics | {condition}",
            timeout=timeout,
        )

    def wait_workers_zero(ctx):
        sched_metric_wait("grep -qx 'rio_scheduler_workers_active 0'", timeout=60)

    def assert_cel_rejects(name, spec_body, expected_msg, kind="Builder"):
        result = k3s_server.fail(
            "k3s kubectl apply --dry-run=server -f - 2>&1 <<'EOF'\n"
            "apiVersion: rio.build/v1alpha1\n"
            "kind: Pool\n"
            f"metadata:\n  name: {name}\n  namespace: ${nsBuilders}\n"
            f"spec:\n  kind: {kind}\n{spec_body}\n"
            "EOF"
        )
        assert expected_msg in result, (
            f"CEL should reject {name!r} with {expected_msg!r}, got: {result!r}"
        )
  '';

  # Same scope shape as lifecycle.nix so fragment files' `with scope;`
  # resolve. Only the keys actually interpolated by `enabledFragments`
  # need to be present (Nix is lazy on attrset access, but `with scope`
  # brings every key into scope for lookup — missing keys are fine
  # until referenced).
  scope = {
    inherit
      pkgs
      common
      ns
      nsStore
      nsBuilders
      nsFetchers
      ephemeralDrv1
      ephemeralDrv2
      ;
  };

  enabledFragments = [
    "pool-lifecycle"
    "ephemeral-spawn"
  ];
  allFragments = common.importDir ./lifecycle;
  fragments = lib.genAttrs enabledFragments (n: allFragments.${n} scope);

  mkTest = common.mkFragmentTest {
    scenario = "controller-kwok";
    inherit prelude fragments fixture;
    defaultTimeout = 300;
    chains = [ ];
  };
in
{
  inherit fragments mkTest;
}
