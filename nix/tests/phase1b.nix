# Phase 1b milestone validation: single-node end-to-end build via ssh-ng.
#
# Milestone (docs/src/phases/phase1b.md:32):
#   `nix build --store ssh-ng://localhost .#hello` completes
#   — full end-to-end single-node build.
#
# Topology (3 VMs):
#   control — PostgreSQL, rio-store, rio-scheduler, rio-gateway
#   worker  — rio-worker (1 instance, max_builds=1)
#   client  — Nix client speaking ssh-ng to control
#
# This is a STRICT subset of phase2a (which has 2 workers + a 5-node DAG).
# It validates the single-node build path that Phase 1b shipped:
#   - wopBuildPathsWithResults end-to-end
#   - gateway → scheduler → worker dispatch (no distribution asserted)
#   - worker FUSE + overlay + nix-daemon --stdio
#   - output upload via PutPath
#
# We use a single trivial derivation (no inputDrvs) instead of nixpkgs#hello
# to avoid seeding the stdenv closure.
#
# Run interactively:
#   nix build .#checks.x86_64-linux.vm-phase1b.driverInteractive
#   ./result/bin/nixos-test-driver
{
  pkgs,
  rio-workspace,
  rioModules,
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

  # Single trivial derivation — one leaf from the phase2a DAG.
  # Reuses the same builder pattern (busybox sh + mkdir + echo).
  testDrvFile = pkgs.writeText "phase1b-derivation.nix" ''
    { busybox }:
    derivation {
      name = "rio-phase1b-trivial";
      system = builtins.currentSystem;
      builder = "''${busybox}/bin/sh";
      args = [
        "-c"
        '''
          set -ex
          ''${busybox}/bin/busybox mkdir -p $out
          ''${busybox}/bin/busybox echo "phase1b milestone" > $out/stamp
        '''
      ];
    }
  '';
in
pkgs.testers.runNixOSTest {
  name = "rio-phase1b";

  nodes = {
    control = common.mkControlNode { hostName = "control"; };

    worker = common.mkWorkerNode {
      hostName = "worker";
      maxBuilds = 1;
    };

    client = common.mkClientNode { gatewayHost = "control"; };
  };

  testScript = ''
    start_all()

    # ── Bootstrap control plane ───────────────────────────────────────
    ${common.waitForControlPlane "control"}

    # ── SSH key exchange + gateway start ──────────────────────────────
    ${common.sshKeySetup "control"}

    # ── Worker registers ──────────────────────────────────────────────
    worker.wait_for_unit("rio-worker.service")
    control.wait_until_succeeds(
        "curl -sf http://localhost:9091/metrics | "
        "grep -E 'rio_scheduler_workers_active 1'"
    )

    # ── Seed store ────────────────────────────────────────────────────
    ${common.seedBusybox "control"}

    # ── Milestone: single-node build ──────────────────────────────────
    # One trivial derivation, one worker. Exercises the full build path
    # (gateway → scheduler → worker → nix-daemon --stdio → upload) without
    # distribution concerns. capture_stderr=False: we assert the output
    # path starts with /nix/store/, so stderr progress lines must not
    # be mixed into the captured stdout.
    ${common.mkBuildHelper {
      gatewayHost = "control";
      inherit testDrvFile;
    }}
    out = build([worker], capture_stderr=False).strip()

    print(f"build output: {out}")
    assert out.startswith("/nix/store/"), f"unexpected build output: {out!r}"

    # ── Verification ──────────────────────────────────────────────────
    # Worker executed exactly one build.
    worker.succeed(
        "curl -sf http://localhost:9093/metrics | "
        "grep -E 'rio_worker_builds_total\\{outcome=\"success\"\\} 1'"
    )

    # Store received the output via PutPath.
    control.succeed(
        "curl -sf http://localhost:9092/metrics | "
        "grep -E 'rio_store_put_path_total\\{result=\"created\"\\} [1-9]'"
    )

    # Output is queryable via ssh-ng (round-trips through rio-store).
    client.succeed(
        f"nix path-info --store 'ssh-ng://control' {out}"
    )

    ${common.collectCoverage "control, worker, client"}
  '';
}
