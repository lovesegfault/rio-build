# Per-subtest fragments for the lifecycle scenario. Each file is
# `scope: with scope; ''...''` — the scenario file applies the
# shared let-bindings (drvs, fixture vars, helpers) as `scope`.
{
  jwt-mount-present = import ./jwt-mount-present.nix;
  health-shared = import ./health-shared.nix;
  cancel-cgroup-kill = import ./cancel-cgroup-kill.nix;
  build-timeout = import ./build-timeout.nix;
  recovery = import ./recovery.nix;
  gc-dry-run = import ./gc-dry-run.nix;
  gc-sweep = import ./gc-sweep.nix;
  refs-end-to-end = import ./refs-end-to-end.nix;
  ephemeral-pool = import ./ephemeral-pool.nix;
  manifest-pool = import ./manifest-pool.nix;
  bps-lifecycle = import ./bps-lifecycle.nix;
  store-rollout = import ./store-rollout.nix;
  bootstrap-job-ran = import ./bootstrap-job-ran.nix;
  bootstrap-tenant = import ./bootstrap-tenant.nix;
}
