# Per-subtest fragments for the scheduling scenario. Each file is
# `scope: with scope; ''...''` — the scenario file applies the
# shared let-bindings (drvs, fixture vars, helpers) as `scope`.
{
  fanout = import ./fanout.nix;
  fuse-direct = import ./fuse-direct.nix;
  overlay-readdir = import ./overlay-readdir.nix;
  sizeclass = import ./sizeclass.nix;
  chunks = import ./chunks.nix;
  reassign = import ./reassign.nix;
  cgroup = import ./cgroup.nix;
  max-silent-time = import ./max-silent-time.nix;
  setoptions-unreachable = import ./setoptions-unreachable.nix;
  cancel-timing = import ./cancel-timing.nix;
  load-50drv = import ./load-50drv.nix;
  sigint-graceful = import ./sigint-graceful.nix;
  warm-gate = import ./warm-gate.nix;
}
