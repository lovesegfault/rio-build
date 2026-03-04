# Shared helpers for the per-phase VM tests.
#
# ~250 LOC was duplicated across phase{1a,1b,2a,2b,2c}.nix:
#   - PostgreSQL config block (5× identical)
#   - SSH key setup Python (5× near-identical, differs only in host var)
#   - workerConfig function (3× near-identical in 2a/2b/2c)
#   - client node config (5× near-identical)
#   - let-bindings (busybox, busyboxClosure, databaseUrl)
#
# Usage:
#   let
#     common = import ./common.nix { inherit pkgs rio-workspace rioModules; };
#   in pkgs.testers.runNixOSTest {
#     nodes.control = { imports = [ common.controlNode ]; ... };
#     nodes.worker1 = common.mkWorkerNode { hostName = "worker1"; };
#     testScript = ''
#       ${common.sshKeySetup "control"}
#       ...
#     '';
#   }
{
  pkgs,
  rio-workspace,
  rioModules,
}:
let
  inherit (pkgs) lib;
in
rec {
  # ── Shared let-bindings ─────────────────────────────────────────────

  # Static busybox: closure of exactly 1 path (no glibc, no runtime deps).
  # The sole input seed for all VM tests — FUSE fetches it on every worker,
  # validating the lazy-fetch path.
  inherit (pkgs.pkgsStatic) busybox;

  # closureInfo gives us store-paths + registration (narinfo) for seeding.
  # Even though pkgsStatic.busybox's closure should be {busybox} alone,
  # use closureInfo to be defensive against unexpected refs.
  busyboxClosure = pkgs.closureInfo { rootPaths = [ busybox ]; };

  # PostgreSQL connection URL — both store and scheduler run migrations on
  # startup (sqlx migrate, advisory-lock serialized), so no separate oneshot.
  databaseUrl = "postgres://postgres@localhost/rio";

  # ── PostgreSQL config (5× identical across all tests) ───────────────

  postgresqlConfig = {
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
  };

  # ── Gateway tmpfiles (5× identical) ─────────────────────────────────
  # Gateway starts after store + scheduler via After= in the module, but
  # load_authorized_keys() errors if the file is missing. Create an empty
  # file via tmpfiles so the gateway unit can start; testScript populates
  # it before the client connects.
  gatewayTmpfiles = [
    "d /var/lib/rio 0755 root root -"
    "d /var/lib/rio/gateway 0755 root root -"
    "f /var/lib/rio/gateway/authorized_keys 0600 root root -"
  ];

  # ── Worker node config (3× near-identical in 2a/2b/2c) ──────────────
  #
  # Parameterized by:
  #   - hostName: VM hostname (also used as worker_id)
  #   - maxBuilds: concurrent build slots (phase2a=2, phase2b=1, phase2c=2)
  #   - sizeClass: optional size-class tag (phase2c only)
  #   - otelEndpoint: optional OTLP endpoint (phase2b only; worker spans
  #     not strictly needed for the milestone but make the trace tree
  #     look like the observability.md spec diagram)
  #
  # The writableStore=false + /nix/var tmpfs pattern is load-bearing —
  # see the inline rationale. The 4-core virtualisation setting is also
  # intentional (tokio multi_thread runtime uses num_cpus worker threads;
  # FUSE callbacks doing Handle::block_on(gRPC) need spare worker threads
  # to drive the reactor).
  mkWorkerNode =
    {
      hostName,
      maxBuilds,
      sizeClass ? null,
      otelEndpoint ? null,
    }:
    {
      imports = [ rioModules.worker ];
      networking.hostName = hostName;

      services.rio = {
        package = rio-workspace;
        logFormat = "pretty"; # human-readable in test logs
        worker = {
          enable = true;
          schedulerAddr = "control:9001";
          storeAddr = "control:9002";
          inherit maxBuilds;
        }
        // lib.optionalAttrs (sizeClass != null) { inherit sizeClass; };
      };

      # OTel endpoint for the worker (phase2b). Worker spans aren't
      # strictly needed for the milestone (gateway→scheduler is the
      # critical trace hop), but having them in Tempo makes the trace
      # tree match the observability.md spec diagram.
      systemd.services.rio-worker.environment = lib.optionalAttrs (otelEndpoint != null) {
        RIO_OTEL_ENDPOINT = otelEndpoint;
      };

      # curl for metric scraping.
      environment.systemPackages = [ pkgs.curl ];

      # 4 cores: tokio multi_thread runtime uses num_cpus worker threads.
      # FUSE callbacks doing Handle::block_on(gRPC) need spare worker
      # threads to drive the reactor; 4 cores gives headroom at
      # maxBuilds=2.
      virtualisation = {
        memorySize = 1024;
        diskSize = 4096;
        cores = 4;
        # Worker VMs must NOT have a writable /nix/store. With
        # writableStore=true (the NixOS-test default), /nix/store is
        # itself an overlayfs (tmpfs upper on 9p lower). Our per-build
        # overlay uses /nix/store as a lower; overlay-on-overlay breaks
        # copy-up → nix-daemon creates chroot dirs, builder writes $out,
        # but the parent can't see it → OutputRejected. With
        # writableStore=false, /nix/store is the plain 9p mount.
        writableStore = false;
        # But the host nix-daemon (NixOS module) still wants to create
        # /nix/var/nix/gcroots etc. — point it at a tmpfs via /nix/var.
        # (Our per-build daemon uses the overlay's synth DB via
        # bind-mount, so this only affects the host daemon's state, not
        # builds.)
      };

      # With writableStore=false, /nix/var is on the RO 9p mount too.
      # Mount tmpfs there so host nix-daemon + our synth-DB bind target
      # have writable paths. The per-build overlay handles /nix/store
      # writes.
      fileSystems."/nix/var" = {
        fsType = "tmpfs";
        neededForBoot = true;
      };
    };

  # ── Client node config (5× near-identical) ──────────────────────────
  #
  # Parameterized by `gatewayHost`: the SSH target hostname (phase1a
  # uses "gateway", all others use "control"). `extraPackages` lets
  # phase2a/2b/2c add curl.
  mkClientNode =
    {
      gatewayHost,
      extraPackages ? [ ],
    }:
    {
      networking.hostName = "client";

      nix.settings.experimental-features = [
        "nix-command"
        "flakes"
      ];

      # Busybox + closure must be in the client's local store so
      # `nix copy` can read + upload them. Referencing them in the
      # config pulls them in.
      environment.systemPackages = [ busybox ] ++ extraPackages;
      # Force closureInfo into the VM's store (not otherwise a runtime dep).
      environment.etc."rio/busybox-closure".source = "${busyboxClosure}";

      # ssh-ng does not support the ?ssh-key= URL query param reliably
      # across Nix versions; use ~/.ssh/config instead.
      programs.ssh.extraConfig = ''
        Host ${gatewayHost}
          HostName ${gatewayHost}
          User root
          Port 2222
          IdentityFile /root/.ssh/id_ed25519
          StrictHostKeyChecking no
          UserKnownHostsFile /dev/null
      '';

      virtualisation.memorySize = 1024;
      virtualisation.cores = 4;
    };

  # ── SSH key setup testScript snippet ────────────────────────────────
  #
  # Generate client key, install on the gateway host, restart gateway so
  # load_authorized_keys() picks up the real key. The gateway may have
  # started with the empty authorized_keys file (tmpfiles); restart
  # is required.
  #
  # Interpolate as `${common.sshKeySetup "control"}` in testScript.
  # The `gatewayHost` arg is the Python variable name for the gateway
  # node (phase1a: `gateway`, all others: `control`).
  sshKeySetup = gatewayHost: ''
    client.succeed("mkdir -p /root/.ssh && ssh-keygen -t ed25519 -N ''' -f /root/.ssh/id_ed25519")
    pubkey = client.succeed("cat /root/.ssh/id_ed25519.pub").strip()
    ${gatewayHost}.succeed(f"echo '{pubkey}' > /var/lib/rio/gateway/authorized_keys")
    ${gatewayHost}.succeed("systemctl restart rio-gateway.service")
    ${gatewayHost}.wait_for_unit("rio-gateway.service")
    ${gatewayHost}.wait_for_open_port(2222)
  '';
}
