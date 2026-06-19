# vm-k3s-batch-a — one k3s boot, three subtest groups (issue #57 1e).
#
# Collapses vm-cli-k3s + vm-lifecycle-core-k3s + vm-lifecycle-gc-k3s
# onto a single `k3sFull { jwtEnabled; singleNode; }` boot. Each of
# those paid ~4min of k3s bring-up for ≤3min of subtests; this pays one
# bring-up and runs the three groups SEQUENTIALLY via lib/driver.py
# run_batch (NOT concurrently — Machine.execute() is thread-unsafe; see
# the lib/driver.py header). The boot-amortization is the saving; ~12min
# of bring-up → ~4min, subtests unchanged.
#
# Group order is load-bearing:
#   1. cli            — `cli builds` asserts the empty-state path
#                       (`.total_count == 0`); MUST run before any
#                       SubmitBuild. cli creates+deletes its own
#                       `cli-smoke-tenant` and does only `gc --dry-run`,
#                       so leaves no state lifecycle-core depends on.
#   2. lifecycle-core — submits builds (cancel-cgroup-kill, build-
#                       timeout). Non-destructive to the store.
#   3. lifecycle-gc   — TriggerGC + backdate+sweep is store-global; runs
#                       LAST so it can't collect a path lifecycle-core
#                       (or a future concurrent group) is mid-building.
#
# Fixture: lifecycle's prelude is the bootstrap (jwtEnabled is required
# for its tenant-authz path; cli's AdminService is HMAC-gated, not
# JWT-gated, so jwtEnabled is a strict superset). cli.nix's `body`
# expects only kubectlHelpers (pf_open / leader_pod) from the bootstrap
# — lifecycle's prelude provides that and more.
#
# vm-security-nonpriv-k3s is NOT folded in: it needs the
# vmtest-full-nonpriv.yaml overlay (default x86-64 pool privileged:
# false), which is a different fixture shape than lifecycle's
# privileged default. Folding it would mean lifecycle-core's
# cancel-cgroup-kill / build-timeout run on the nonpriv FUSE/cgroup
# path for the first time — a behaviour change orthogonal to the
# boot-amortization goal. Left as its own singleNode test.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (pkgs) lib;

  lifecycleMod = import ./lifecycle.nix { inherit pkgs common fixture; };
  cliMod = import ./cli.nix { inherit pkgs common fixture; };

  cat = names: lib.concatMapStrings (s: lifecycleMod.fragments.${s} + "\n") names;
in
common.mkBatchTest {
  scenario = "k3s-batch-a";
  inherit fixture;
  inherit (lifecycleMod) prelude;
  # ~240s bring-up + cli ~60s + core ~360s + gc ~300s ≈ 960s; 1500
  # leaves slack for builder-disk variance under coverage. Each group
  # also carries its own per-group budget (run_batch records a TIMEOUT
  # failure if exceeded; globalTimeout is the hard backstop).
  globalTimeout = 1500;
  groups = [
    {
      name = "cli";
      timeout = 300;
      inherit (cliMod) body;
    }
    {
      name = "lifecycle-core";
      timeout = 600;
      body = cat [
        "jwt-mount-present"
        "health-shared"
        "cancel-cgroup-kill"
        "build-timeout"
        "pool-lifecycle"
      ];
    }
    {
      name = "lifecycle-gc";
      timeout = 600;
      body = cat [
        "gc-dry-run"
        "gc-sweep"
        "refs-end-to-end"
      ];
    }
  ];
}
