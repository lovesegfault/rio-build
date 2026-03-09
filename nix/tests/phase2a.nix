# Phase 2a milestone validation as a NixOS VM test.
#
# Milestone (docs/src/phases/phase2a.md:28):
#   `nix build --store ssh-ng://rio nixpkgs#hello` completes across 2+ workers.
#
# This test substitutes a tiny 5-node busybox fan-out DAG for `nixpkgs#hello`
# (seeding the full stdenv closure would dominate test time), but otherwise
# exercises the real distributed path: 4 VMs, real FUSE + overlayfs + mount
# namespaces, gateway speaking ssh-ng, scheduler dispatching across 2 workers,
# and Prometheus metrics validating distribution + FUSE fetch.
#
# Topology:
#   control   — PostgreSQL, rio-store, rio-scheduler, rio-gateway
#   worker1   — rio-worker (CAP_SYS_ADMIN, /dev/fuse)
#   worker2   — rio-worker (CAP_SYS_ADMIN, /dev/fuse)
#   client    — Nix client speaking ssh-ng to control
#
# Run interactively for debugging:
#   nix build .#checks.x86_64-linux.vm-phase2a.driverInteractive
#   ./result/bin/nixos-test-driver
#   >>> start_all(); control.shell_interact()
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

  # The 5-node test DAG. Passed to `nix-build` on the client via
  # `--arg busybox '<storePath>'`.
  testDrvFile = ./phase2a-derivation.nix;
in
pkgs.testers.runNixOSTest {
  name = "rio-phase2a";

  nodes = {
    control = common.mkControlNode {
      hostName = "control";
      # 9090-9092: Prometheus metrics ports (gateway/scheduler/store).
      extraFirewallPorts = [
        9090
        9091
        9092
      ];
    };

    worker1 = common.mkWorkerNode {
      hostName = "worker1";
      maxBuilds = 2;
    };
    worker2 = common.mkWorkerNode {
      hostName = "worker2";
      maxBuilds = 2;
    };

    client = common.mkClientNode {
      gatewayHost = "control";
      extraPackages = [ pkgs.curl ];
    };
  };

  testScript = ''
    start_all()

    # ── Phase 1: Bootstrap control plane ──────────────────────────────
    ${common.waitForControlPlane "control"}

    # ── Phase 2: SSH key exchange + gateway start ─────────────────────
    ${common.sshKeySetup "control"}

    # ── Phase 3: Workers register ─────────────────────────────────────
    # Workers restart-on-failure until scheduler is reachable.
    worker1.wait_for_unit("rio-worker.service")
    worker2.wait_for_unit("rio-worker.service")
    # Poll scheduler metrics until both workers show up.
    control.wait_until_succeeds(
        "curl -sf http://localhost:9091/metrics | "
        "grep -E 'rio_scheduler_workers_active 2'"
    )

    # ── Phase 4: Seed store via gateway ───────────────────────────────
    ${common.seedBusybox "control"}

    # ── Phase 5: Milestone build ──────────────────────────────────────
    # Build the 5-node fan-out DAG. `--no-link` to avoid pulling results
    # back. `nix-build` (not `nix build`) for simpler --arg passing
    # without flakes. On failure, worker + scheduler journals are dumped.
    ${common.mkBuildHelper {
      gatewayHost = "control";
      inherit testDrvFile;
    }}
    # capture_stderr=False so stdout is clean (just the output path).
    out = build([worker1, worker2], capture_stderr=False).strip()
    print(f"built output: {out}")

    # ── Phase 6: Verification ─────────────────────────────────────────
    # Build succeeded (exit code already checked by succeed()).
    #
    # Verify the build output CONTENT via ssh-ng. The root's
    # $out/stamp contains its own name followed by all 4 child
    # stamps (phase2a-derivation.nix:27-28). grep -c 'rio-leaf-' == 4
    # proves: (a) the output is actually retrievable via wopNarFromPath,
    # (b) the collector concatenated all 4 children (DAG walked
    # correctly), (c) the NAR bytes are intact (wrong content → wrong
    # grep count).
    leaf_count = client.succeed(
        f"nix store cat --store 'ssh-ng://control' {out}/stamp | "
        f"grep -c 'rio-leaf-'"
    ).strip()
    assert leaf_count == "4", (
        f"expected 4 'rio-leaf-' lines in {out}/stamp (one per child), "
        f"got {leaf_count}. DAG walk or NAR content corrupted?"
    )

    # Scheduler: at least one build reached terminal success.
    # Metric format: `rio_scheduler_builds_total{outcome="success"} N`
    control.succeed(
        "curl -sf http://localhost:9091/metrics | "
        "grep -E 'rio_scheduler_builds_total\\{outcome=\"success\"\\} [1-9]'"
    )

    # Distribution: both workers executed at least one derivation.
    # With 4 parallel leaves and max_builds=2 per worker, each worker should
    # get ≥1 leaf. Metric: `rio_worker_builds_total{outcome="success"} N`
    for w in [worker1, worker2]:
        w.succeed(
            "curl -sf http://localhost:9093/metrics | "
            "grep -E 'rio_worker_builds_total\\{outcome=\"success\"\\} [1-9]'"
        )

    # FUSE fetch: both workers pulled at least one path from rio-store
    # (busybox must be fetched before any build can run).
    for w in [worker1, worker2]:
        w.succeed(
            "curl -sf http://localhost:9093/metrics | "
            "grep -E 'rio_worker_fuse_cache_misses_total [1-9]'"
        )

    # Store: received the 5 build outputs via PutPath (plus the busybox seed).
    # We check ≥5 to be robust against retries / seed upload.
    control.succeed(
        "curl -sf http://localhost:9092/metrics | "
        "grep -E 'rio_store_put_path_total\\{result=\"created\"\\} ([5-9]|[1-9][0-9])'"
    )

    ${common.collectCoverage "control, worker1, worker2, client"}
  '';
}
