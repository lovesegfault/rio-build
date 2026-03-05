# Shared helpers for the per-phase VM tests.
#
# Duplication extracted from phase{1a,1b,2a,2b,2c}.nix:
#   - Control node config (5× near-identical → mkControlNode)
#   - PostgreSQL config block (5× identical → postgresqlConfig)
#   - SSH key setup Python (5× → sshKeySetup)
#   - Worker node config (3× in 2a/2b/2c → mkWorkerNode)
#   - Client node config (5× → mkClientNode)
#   - Control-plane wait testScript (5× → waitForControlPlane)
#   - Seed-busybox testScript (5× → seedBusybox)
#   - Build + journal-dump-on-failure (4× → mkBuildHelper)
#   - let-bindings (busybox, busyboxClosure, databaseUrl)
#
# Usage:
#   let
#     common = import ./common.nix { inherit pkgs rio-workspace rioModules; };
#   in pkgs.testers.runNixOSTest {
#     nodes.control = common.mkControlNode { hostName = "control"; };
#     nodes.worker1 = common.mkWorkerNode { hostName = "worker1"; maxBuilds = 2; };
#     nodes.client  = common.mkClientNode { gatewayHost = "control"; };
#     testScript = ''
#       start_all()
#       ${common.waitForControlPlane "control"}
#       ${common.sshKeySetup "control"}
#       ${common.seedBusybox "control"}
#       ${common.mkBuildHelper { gatewayHost = "control"; inherit testDrvFile; }}
#       build([worker1])
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

  # ── Control node config (5× near-identical) ─────────────────────────
  #
  # Runs PostgreSQL + rio-store + rio-scheduler + rio-gateway on one VM.
  # All 5 phase tests share this topology for the control plane; only
  # per-phase knobs (memory, firewall, scheduler extras) vary.
  #
  # Use as a full node definition for simple cases:
  #   control = common.mkControlNode { hostName = "control"; };
  #
  # Or as an import when layering extras (phase2b adds Tempo on top —
  # NixOS module merging handles systemd.services, systemPackages etc.):
  #   control = {
  #     imports = [ (common.mkControlNode { hostName = "control"; ... }) ];
  #     systemd.services.tempo = { ... };
  #   };
  mkControlNode =
    {
      hostName,
      memorySize ? 1024,
      diskSize ? 4096,
      # Merged into services.rio.scheduler via // — phase2c passes
      # extraConfig + tickIntervalSecs for size-class TOML routing.
      extraSchedulerConfig ? { },
      # Merged into services.rio.store via // — phase2c (post-A3)
      # passes extraConfig for [chunk_backend] TOML.
      extraStoreConfig ? { },
      # Appended to the base set [ 2222 9001 9002 ]. phase2a/2c open
      # metrics ports; phase2b also opens Tempo's OTLP + query ports.
      extraFirewallPorts ? [ ],
      # Appended to the base [ pkgs.curl ] — every build-capable test
      # scrapes metrics via curl on the control node. phase1a doesn't
      # need curl but getting it is harmless.
      extraPackages ? [ ],
    }:
    {
      imports = [
        rioModules.store
        rioModules.scheduler
        rioModules.gateway
        postgresqlConfig
      ];
      networking.hostName = hostName;

      services.rio = {
        package = rio-workspace;
        logFormat = "pretty"; # human-readable in VM test logs
        store = {
          enable = true;
          inherit databaseUrl;
        }
        // extraStoreConfig;
        scheduler = {
          enable = true;
          storeAddr = "localhost:9002";
          inherit databaseUrl;
        }
        // extraSchedulerConfig;
        gateway = {
          enable = true;
          schedulerAddr = "localhost:9001";
          storeAddr = "localhost:9002";
          authorizedKeysPath = "/var/lib/rio/gateway/authorized_keys";
        };
      };

      systemd.tmpfiles.rules = gatewayTmpfiles;

      environment.systemPackages = [ pkgs.curl ] ++ extraPackages;

      # 2222 = gateway SSH (client), 9001 = scheduler gRPC (workers),
      # 9002 = store gRPC (workers). phase1a has no workers so 9001/9002
      # are technically unused cross-VM there, but opening them is a no-op.
      networking.firewall.allowedTCPPorts = [
        2222
        9001
        9002
      ]
      ++ extraFirewallPorts;

      virtualisation = {
        cores = 4;
        inherit memorySize diskSize;
      };
    };

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

  # ── Control-plane wait (5× identical) ───────────────────────────────
  # Blocks until postgres + rio-store + rio-scheduler are all ready.
  # Gateway startup is handled separately by sshKeySetup (restart after
  # populating authorized_keys). `node` is the Python variable name for
  # the control node (phase1a: `gateway`, all others: `control`).
  waitForControlPlane = node: ''
    ${node}.wait_for_unit("postgresql.service")
    ${node}.wait_for_unit("rio-store.service")
    ${node}.wait_for_open_port(9002)
    ${node}.wait_for_unit("rio-scheduler.service")
    ${node}.wait_for_open_port(9001)
  '';

  # ── Seed busybox closure (5× identical modulo ssh target) ───────────
  # Uploads the static-busybox closure via `nix copy` over ssh-ng.
  # Exercises wopAddToStoreNar / wopAddMultipleToStore. `--no-check-sigs`
  # because the client's local store paths aren't signed. `gatewayHost`
  # is the SSH target hostname (phase1a: "gateway", others: "control").
  seedBusybox = gatewayHost: ''
    client.succeed("ls ${busybox}")
    client.succeed(
        "nix copy --no-check-sigs --to 'ssh-ng://${gatewayHost}' "
        "$(cat ${busyboxClosure}/store-paths)"
    )
  '';

  # ── Build helper with journal dump on failure (4× near-identical) ───
  #
  # Generates a Python `def build(workers, attr="", capture_stderr=True)`
  # that runs nix-build against the gateway and dumps worker +
  # control-plane journals on failure before re-raising.
  #
  #   - `workers` is passed at the Python level so each test can supply
  #     its own worker node list (phase1b: [worker], phase2a: [worker1,
  #     worker2], phase2c: [wsmall1, wsmall2, wlarge], etc.).
  #   - `attr` selects a derivation attr via `-A` (phase2c uses this).
  #   - `capture_stderr=False` for tests asserting on the build's stdout
  #     (phase1b checks the output path starts with /nix/store/; 2>&1
  #     would mix nix-build progress lines into the captured output).
  #
  # `gatewayHost` doubles as the SSH target hostname AND the Python
  # variable name for the control node — these match in all current
  # tests (both are "control"; phase1a doesn't build so N/A there).
  # `testDrvFile` is baked at Nix-eval time (per-phase; can be either
  # a `./foo.nix` path literal or a `pkgs.writeText` derivation).
  mkBuildHelper =
    { gatewayHost, testDrvFile }:
    ''
      def build(workers, attr="", capture_stderr=True):
          cmd = (
              "nix-build --no-out-link "
              "--store 'ssh-ng://${gatewayHost}' "
              "--arg busybox '(builtins.storePath ${busybox})' "
              "${testDrvFile}"
          )
          if attr:
              cmd += f" -A {attr}"
          if capture_stderr:
              cmd += " 2>&1"
          try:
              return client.succeed(cmd)
          except Exception:
              for w in workers:
                  w.execute("journalctl -u rio-worker --no-pager -n 200 >&2")
              ${gatewayHost}.execute("journalctl -u rio-scheduler -u rio-gateway --no-pager -n 200 >&2")
              raise
    '';
}
