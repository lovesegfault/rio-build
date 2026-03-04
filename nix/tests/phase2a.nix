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
#   nix build .#checks.x86_64-linux.rio-phase2a.driverInteractive
#   ./result/bin/nixos-test-driver
#   >>> start_all(); control.shell_interact()
{
  pkgs,
  rio-workspace,
  rioModules,
}:
let
  common = import ./common.nix { inherit pkgs rio-workspace rioModules; };
  inherit (common) busybox busyboxClosure databaseUrl;

  # The 5-node test DAG. Passed to `nix-build` on the client via
  # `--arg busybox '<storePath>'`.
  testDrvFile = ./phase2a-derivation.nix;
in
pkgs.testers.runNixOSTest {
  name = "rio-phase2a";

  nodes = {
    control = {
      imports = [
        rioModules.store
        rioModules.scheduler
        rioModules.gateway
        common.postgresqlConfig
      ];
      networking.hostName = "control";

      services.rio = {
        package = rio-workspace;
        logFormat = "pretty";
        store = {
          enable = true;
          inherit databaseUrl;
        };
        scheduler = {
          enable = true;
          storeAddr = "localhost:9002";
          inherit databaseUrl;
        };
        gateway = {
          enable = true;
          schedulerAddr = "localhost:9001";
          storeAddr = "localhost:9002";
          authorizedKeysPath = "/var/lib/rio/gateway/authorized_keys";
        };
      };

      systemd.tmpfiles.rules = common.gatewayTmpfiles;

      environment.systemPackages = [ pkgs.curl ];

      # Open ports for cross-VM gRPC + SSH.
      networking.firewall.allowedTCPPorts = [
        2222
        9001
        9002
        9090
        9091
        9092
      ];

      virtualisation = {
        memorySize = 1024;
        diskSize = 4096;
        cores = 4;
      };
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
    control.wait_for_unit("postgresql.service")
    control.wait_for_unit("rio-store.service")
    control.wait_for_open_port(9002)
    control.wait_for_unit("rio-scheduler.service")
    control.wait_for_open_port(9001)

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
    # Sanity: busybox is in the client's local store.
    client.succeed("ls ${busybox}")
    # Upload busybox closure through the gateway (exercises wopAddToStoreNar /
    # wopAddMultipleToStore). `--no-check-sigs` because the client's local
    # store paths aren't signed.
    client.succeed(
        "nix copy --no-check-sigs --to 'ssh-ng://control' "
        "$(cat ${busyboxClosure}/store-paths)"
    )

    # ── Phase 5: Milestone build ──────────────────────────────────────
    # Build the 5-node fan-out DAG. `--no-link` to avoid pulling results back.
    # `nix-build` (not `nix build`) for simpler --arg passing without flakes.
    # On failure, dump worker journals before raising (testScript catches the
    # exception from `succeed()` and re-raises after printing).
    try:
        client.succeed(
            "nix-build --no-out-link "
            "--store 'ssh-ng://control' "
            "--arg busybox '(builtins.storePath ${busybox})' "
            "${testDrvFile}"
        )
    except Exception:
        for w in [worker1, worker2]:
            w.execute("journalctl -u rio-worker --no-pager -n 200 >&2")
        control.execute("journalctl -u rio-scheduler --no-pager -n 100 >&2")
        raise

    # ── Phase 6: Verification ─────────────────────────────────────────
    # Build succeeded (exit code already checked by succeed()).

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
  '';
}
