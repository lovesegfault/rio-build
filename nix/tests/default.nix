# Wires fixtures × scenarios into the vmTests attrset.
#
# flake.nix imports this ONCE (instead of importing each phase*.nix
# individually). Returns `{ vm-<scenario>-<fixture> = <runNixOSTest>; }`.
# Coverage mode: same structure, `coverage=true` propagates to fixtures
# → covEnv in service envs → collectCoverage fires.
#
# During the migration, phase*.nix tests coexist here — commit 10 drops them.
{
  pkgs,
  rio-workspace,
  rioModules,
  dockerImages,
  nixhelm,
  system,
  crds,
  coverage ? false,
}:
let
  common = import ./common.nix {
    inherit
      pkgs
      rio-workspace
      rioModules
      coverage
      ;
  };

  standalone = import ./fixtures/standalone.nix {
    inherit
      pkgs
      rio-workspace
      rioModules
      coverage
      ;
  };

  # k3s-full not wired yet — commit 9 adds lifecycle/leader-election.
  # k3sFull = import ./fixtures/k3s-full.nix {
  #   inherit pkgs rio-workspace rioModules dockerImages nixhelm system coverage;
  # };

  protocol = import ./scenarios/protocol.nix;

  # ── Legacy phase tests (deleted in commit 10) ───────────────────────
  phaseArgs = {
    inherit
      pkgs
      rio-workspace
      coverage
      rioModules
      ;
  };
  k3sArgs = phaseArgs // {
    inherit
      dockerImages
      system
      crds
      nixhelm
      ;
  };
in
{
  # ── New scenario-based tests ────────────────────────────────────────
  vm-protocol-warm-standalone = protocol {
    inherit pkgs common;
    fixture = standalone {
      workers = {
        worker = {
          maxBuilds = 1;
        };
      };
    };
    cold = false;
  };

  vm-protocol-cold-standalone = protocol {
    inherit pkgs common;
    fixture = standalone {
      workers = {
        worker = {
          maxBuilds = 1;
        };
      };
    };
    cold = true;
  };

  # ── Legacy phase tests (migration period; deleted in commit 10) ─────
  vm-phase1a = import ./phase1a.nix phaseArgs;
  vm-phase1b = import ./phase1b.nix phaseArgs;
  vm-phase2a = import ./phase2a.nix phaseArgs;
  vm-phase2b = import ./phase2b.nix phaseArgs;
  vm-phase2c = import ./phase2c.nix phaseArgs;
  vm-phase3a = import ./phase3a.nix k3sArgs;
  vm-phase3b = import ./phase3b.nix k3sArgs;
  vm-phase4 = import ./phase4.nix k3sArgs;
}
