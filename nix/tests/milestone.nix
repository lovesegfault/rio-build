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
#   nix build .#checks.x86_64-linux.rio-milestone-vm.driverInteractive
#   ./result/bin/nixos-test-driver
#   >>> start_all(); control.shell_interact()
{
  pkgs,
  rio-workspace,
  rioModules,
}:
let
  inherit (pkgs) lib;

  # Static busybox: closure of exactly 1 path (no glibc, no runtime deps).
  # This is the sole input seed — FUSE fetches it on every worker, validating
  # the lazy-fetch path.
  inherit (pkgs.pkgsStatic) busybox;

  # closureInfo gives us store-paths + registration (narinfo) for seeding.
  # Even though pkgsStatic.busybox's closure should be {busybox} alone,
  # use closureInfo to be defensive against unexpected refs.
  busyboxClosure = pkgs.closureInfo { rootPaths = [ busybox ]; };

  # The 5-node test DAG. Passed to `nix-build` on the client via
  # `--arg busybox '<storePath>'`.
  testDrvFile = ./test-derivation.nix;

  # PostgreSQL connection URL — both store and scheduler run migrations on
  # startup (sqlx migrate, advisory-lock serialized), so no separate oneshot.
  databaseUrl = "postgres://postgres@localhost/rio";

  # Shared NixOS config for worker VMs.
  workerConfig = hostName: {
    imports = [ rioModules.worker ];
    networking.hostName = hostName;

    services.rio = {
      package = rio-workspace;
      logFormat = "pretty"; # human-readable in test logs
      worker = {
        enable = true;
        schedulerAddr = "control:9001";
        storeAddr = "control:9002";
        maxBuilds = 2;
      };
    };

    # curl for metric scraping.
    environment.systemPackages = [ pkgs.curl ];

    # 2 cores: tokio multi_thread runtime uses num_cpus worker threads.
    # With 1 vCPU, FUSE callbacks doing Handle::block_on(gRPC) can starve
    # when the single worker thread is busy driving other futures. 2 cores
    # also matches max_builds=2 for reasonable build parallelism.
    virtualisation = {
      memorySize = 1024;
      diskSize = 4096;
      cores = 2;
    };
  };
in
pkgs.testers.runNixOSTest {
  name = "rio-milestone-vm";

  nodes = {
    control = {
      imports = [
        rioModules.store
        rioModules.scheduler
        rioModules.gateway
      ];
      networking.hostName = "control";

      services.postgresql = {
        enable = true;
        enableTCPIP = true;
        # Trust auth inside the VM (no password). Covers local unix socket
        # AND the 10.0.0.0/8 test network (nixosTest's default vlan range).
        authentication = lib.mkForce ''
          local all all trust
          host  all all 127.0.0.1/32 trust
          host  all all ::1/128 trust
        '';
        initialScript = pkgs.writeText "rio-init.sql" ''
          CREATE DATABASE rio;
        '';
      };

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

      # Gateway starts after store + scheduler via After= in the module, but
      # load_authorized_keys() errors if the file is missing. Create an empty
      # file via tmpfiles so the gateway unit can start; testScript populates
      # it before the client connects.
      systemd.tmpfiles.rules = [
        "d /var/lib/rio 0755 root root -"
        "d /var/lib/rio/gateway 0755 root root -"
        "f /var/lib/rio/gateway/authorized_keys 0600 root root -"
      ];

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

      virtualisation.memorySize = 1024;
      virtualisation.diskSize = 4096;
    };

    worker1 = workerConfig "worker1";
    worker2 = workerConfig "worker2";

    client = {
      networking.hostName = "client";

      nix.settings.experimental-features = [
        "nix-command"
        "flakes"
      ];

      # Busybox + closure must be in the client's local store so `nix copy`
      # can read + upload them. Referencing them in the config pulls them in.
      environment.systemPackages = [
        busybox
        pkgs.curl
      ];
      # Force closureInfo into the VM's store (not otherwise a runtime dep).
      environment.etc."rio/busybox-closure".source = "${busyboxClosure}";

      # ssh-ng does not support the ?ssh-key= URL query param reliably across
      # Nix versions; use ~/.ssh/config instead.
      programs.ssh.extraConfig = ''
        Host control
          HostName control
          User root
          Port 2222
          IdentityFile /root/.ssh/id_ed25519
          StrictHostKeyChecking no
          UserKnownHostsFile /dev/null
      '';

      virtualisation.memorySize = 1024;
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
    # Generate client key, install on control, restart gateway to pick it up.
    client.succeed("mkdir -p /root/.ssh && ssh-keygen -t ed25519 -N ''' -f /root/.ssh/id_ed25519")
    pubkey = client.succeed("cat /root/.ssh/id_ed25519.pub").strip()
    control.succeed(f"echo '{pubkey}' > /var/lib/rio/gateway/authorized_keys")
    # Gateway may have started with the empty authorized_keys file (tmpfiles);
    # restart so load_authorized_keys() picks up the real key.
    control.succeed("systemctl restart rio-gateway.service")
    control.wait_for_unit("rio-gateway.service")
    control.wait_for_open_port(2222)

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

    # Scheduler: at least one build reached terminal succeeded.
    # Metric format: `rio_scheduler_builds_total{outcome="succeeded"} N`
    control.succeed(
        "curl -sf http://localhost:9091/metrics | "
        "grep -E 'rio_scheduler_builds_total\\{outcome=\"succeeded\"\\} [1-9]'"
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
