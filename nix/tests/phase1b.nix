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
#   nix build .#checks.x86_64-linux.rio-phase1b.driverInteractive
#   ./result/bin/nixos-test-driver
{
  pkgs,
  rio-workspace,
  rioModules,
}:
let
  inherit (pkgs) lib;

  inherit (pkgs.pkgsStatic) busybox;
  busyboxClosure = pkgs.closureInfo { rootPaths = [ busybox ]; };

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

  databaseUrl = "postgres://postgres@localhost/rio";
in
pkgs.testers.runNixOSTest {
  name = "rio-phase1b";

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

      systemd.tmpfiles.rules = [
        "d /var/lib/rio 0755 root root -"
        "d /var/lib/rio/gateway 0755 root root -"
        "f /var/lib/rio/gateway/authorized_keys 0600 root root -"
      ];

      environment.systemPackages = [ pkgs.curl ];

      networking.firewall.allowedTCPPorts = [
        2222
        9001
        9002
      ];

      virtualisation = {
        memorySize = 1024;
        diskSize = 4096;
        cores = 4;
      };
    };

    worker = {
      imports = [ rioModules.worker ];
      networking.hostName = "worker";

      services.rio = {
        package = rio-workspace;
        logFormat = "pretty";
        worker = {
          enable = true;
          schedulerAddr = "control:9001";
          storeAddr = "control:9002";
          maxBuilds = 1;
        };
      };

      environment.systemPackages = [ pkgs.curl ];

      virtualisation = {
        memorySize = 1024;
        diskSize = 4096;
        cores = 4;
        # See phase2a.nix for why writableStore=false on worker VMs.
        writableStore = false;
      };

      fileSystems."/nix/var" = {
        fsType = "tmpfs";
        neededForBoot = true;
      };
    };

    client = {
      networking.hostName = "client";

      nix.settings.experimental-features = [
        "nix-command"
        "flakes"
      ];

      environment.systemPackages = [ busybox ];
      environment.etc."rio/busybox-closure".source = "${busyboxClosure}";

      programs.ssh.extraConfig = ''
        Host control
          HostName control
          User root
          Port 2222
          IdentityFile /root/.ssh/id_ed25519
          StrictHostKeyChecking no
          UserKnownHostsFile /dev/null
      '';

      virtualisation = {
        memorySize = 1024;
        cores = 4;
      };
    };
  };

  testScript = ''
    start_all()

    # ── Bootstrap control plane ───────────────────────────────────────
    control.wait_for_unit("postgresql.service")
    control.wait_for_unit("rio-store.service")
    control.wait_for_open_port(9002)
    control.wait_for_unit("rio-scheduler.service")
    control.wait_for_open_port(9001)

    # ── SSH key exchange + gateway start ──────────────────────────────
    client.succeed("mkdir -p /root/.ssh && ssh-keygen -t ed25519 -N ''' -f /root/.ssh/id_ed25519")
    pubkey = client.succeed("cat /root/.ssh/id_ed25519.pub").strip()
    control.succeed(f"echo '{pubkey}' > /var/lib/rio/gateway/authorized_keys")
    control.succeed("systemctl restart rio-gateway.service")
    control.wait_for_unit("rio-gateway.service")
    control.wait_for_open_port(2222)

    # ── Worker registers ──────────────────────────────────────────────
    worker.wait_for_unit("rio-worker.service")
    control.wait_until_succeeds(
        "curl -sf http://localhost:9091/metrics | "
        "grep -E 'rio_scheduler_workers_active 1'"
    )

    # ── Seed store ────────────────────────────────────────────────────
    client.succeed("ls ${busybox}")
    client.succeed(
        "nix copy --no-check-sigs --to 'ssh-ng://control' "
        "$(cat ${busyboxClosure}/store-paths)"
    )

    # ── Milestone: single-node build ──────────────────────────────────
    # One trivial derivation, one worker. Exercises the full build path
    # (gateway → scheduler → worker → nix-daemon --stdio → upload) without
    # distribution concerns.
    try:
        out = client.succeed(
            "nix-build --no-out-link "
            "--store 'ssh-ng://control' "
            "--arg busybox '(builtins.storePath ${busybox})' "
            "${testDrvFile}"
        ).strip()
    except Exception:
        worker.execute("journalctl -u rio-worker --no-pager -n 200 >&2")
        control.execute("journalctl -u rio-scheduler --no-pager -n 100 >&2")
        control.execute("journalctl -u rio-gateway --no-pager -n 100 >&2")
        raise

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
  '';
}
