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
  # Coverage mode: rio-workspace is the instrumented build,
  # LLVM_PROFILE_FILE is set in all rio-* service environments, and
  # collectCoverage emits the profraw-collection testScript snippet.
  # false (default, vmTests) → all three are no-ops.
  coverage ? false,
}:
let
  inherit (pkgs) lib;

  # --- Coverage plumbing (all are no-ops when coverage=false) ---
  # LLVM_PROFILE_FILE template: %p = PID (handles service restarts —
  # phase3a/3b do `systemctl restart rio-*` multiple times), %m =
  # binary signature (each binary has a distinct coverage map; enables
  # safe on-line merging).
  #
  # DOUBLE-% ESCAPE: systemd's Environment= expands specifiers (%p =
  # unit prefix name, %m = machine ID, etc). Without escaping, systemd
  # replaces %p with e.g. "rio-gateway" before the binary sees it →
  # restarts overwrite the same file (no PID uniqueness). `%%` →
  # literal `%` → LLVM sees `%p-%m` and expands correctly.
  covEnv = lib.optionalAttrs coverage {
    LLVM_PROFILE_FILE = "/var/lib/rio/cov/rio-%%p-%%m.profraw";
  };
  covTmpfiles = lib.optional coverage "d /var/lib/rio/cov 0755 root root -";
  # Instrumented binaries are ~2× RSS; bump VM memory.
  covMemBump = if coverage then 256 else 0;
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
      # Merged into systemd.services.rio-{store,scheduler,gateway}.environment.
      # phase3b uses this to set RIO_TLS__* + RIO_HMAC_KEY_PATH without
      # extending the NixOS modules. NixOS attrsOf merge composes this
      # with each module's own `environment = {...}` block — figment reads
      # the union. Same env applied to all three services (TLS is the
      # same cert for the shared control VM; unknown vars are ignored).
      extraServiceEnv ? { },
    }:
    {
      imports = [
        rioModules.store
        rioModules.scheduler
        rioModules.gateway
        postgresqlConfig
      ];
      networking.hostName = hostName;

      # phase3b TLS/HMAC env injection + coverage env. Empty attrset
      # = no-op (NixOS module merge with {} is identity). When set,
      # the module system merges these keys with each module's own
      # `environment = {...}` — no risk of clobbering RIO_LISTEN_ADDR
      # etc. covEnv is {} when coverage=false.
      systemd.services = {
        rio-store.environment = extraServiceEnv // covEnv;
        rio-scheduler.environment = extraServiceEnv // covEnv;
        rio-gateway.environment = extraServiceEnv // covEnv;
      };

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

      systemd.tmpfiles.rules = gatewayTmpfiles ++ covTmpfiles;

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
        memorySize = memorySize + covMemBump;
        inherit diskSize;
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
      # Merged into systemd.services.rio-worker.environment. phase3b
      # uses this to set RIO_TLS__* (client cert for mTLS to scheduler/
      # store). Composed with the optional RIO_OTEL_ENDPOINT below via //.
      extraServiceEnv ? { },
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
      systemd.services.rio-worker.environment =
        lib.optionalAttrs (otelEndpoint != null) {
          RIO_OTEL_ENDPOINT = otelEndpoint;
        }
        // extraServiceEnv
        // covEnv;

      systemd.tmpfiles.rules = covTmpfiles;

      # curl for metric scraping.
      environment.systemPackages = [ pkgs.curl ];

      # 4 cores: tokio multi_thread runtime uses num_cpus worker threads.
      # FUSE callbacks doing Handle::block_on(gRPC) need spare worker
      # threads to drive the reactor; 4 cores gives headroom at
      # maxBuilds=2.
      virtualisation = {
        memorySize = 1024 + covMemBump;
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
  # k3s server node + rio-controller systemd service. Extracted
  # from phase3a.nix for reuse in phase3b.nix (Build CRD section).
  #
  # Parameters:
  #   controllerEnv: rio-controller systemd environment attrset
  #     (RIO_SCHEDULER_ADDR, RIO_STORE_ADDR, KUBECONFIG, etc).
  #     Must include at least KUBECONFIG.
  #   manifests: services.k3s.manifests (auto-deploy; k3s applies
  #     in filename order, use "00-" prefix for CRDs so they
  #     establish before dependent CRs).
  #   extraK3sImages: images to preload beyond airgap-images.
  #     [] if no containerized rio components (e.g., phase3b uses
  #     a native NixOS worker, no pod).
  #
  # Returns a NixOS module function ({ config, ... }: { ... }).
  # The caller provides hostName="k8s" via nodes.k8s attribute.
  mkK3sNode =
    {
      controllerEnv,
      manifests,
      extraK3sImages ? [ ],
    }:
    { config, ... }:
    {
      networking = {
        hostName = "k8s";
        # 6443 = k3s apiserver. 8472 = flannel VXLAN.
        firewall.allowedTCPPorts = [ 6443 ];
        firewall.allowedUDPPorts = [ 8472 ];
      };

      # k3s needs swap off (kubelet check). NixOS test VMs don't
      # enable swap by default, but make it explicit.
      swapDevices = [ ];

      # FUSE kernel module — /dev/fuse must exist on the HOST for
      # the hostPath CharDevice volume to mount.
      boot.kernelModules = [ "fuse" ];

      # cgroup v2 unified hierarchy (explicit — if it fell back
      # to hybrid, worker's own_cgroup() parser fails).
      boot.kernelParams = [ "systemd.unified_cgroup_hierarchy=1" ];

      services.k3s = {
        enable = true;
        role = "server";
        extraFlags = [
          # eth1 = test vlan (inter-VM). eth0 = slirp (mgmt) —
          # flannel doesn't work there (no broadcast).
          "--flannel-iface"
          "eth1"
          # Disable unneeded components: ingress, K8s metrics.
          "--disable"
          "traefik"
          "--disable"
          "metrics-server"
          # SAN: cross-VM clients (scheduler for Lease, phase3b
          # controller) reach https://k8s:6443; cert must include
          # `k8s` or TLS verification rejects.
          "--tls-san"
          "k8s"
        ];
        # Airgap: NixOS test VMs have no internet. Preload k3s's
        # own pod images + any caller-provided ones.
        images = [ config.services.k3s.package.airgap-images ] ++ extraK3sImages;
        inherit manifests;
      };

      systemd = {
        # containerd needs cgroup delegation to create pod cgroups.
        # Without this: pods stuck in ContainerCreating.
        services.k3s.serviceConfig.Delegate = "yes";

        # rio-controller as systemd (not pod). Simpler for VM tests
        # — no RBAC bootstrap ordering problem (k3s.yaml is
        # cluster-admin). After=k3s but k3s "starts" before
        # apiserver is ready; RestartSec handles the gap.
        services.rio-controller = {
          description = "rio-controller (K8s operator)";
          wantedBy = [ "multi-user.target" ];
          after = [ "k3s.service" ];
          requires = [ "k3s.service" ];
          # covEnv propagates LLVM_PROFILE_FILE — the controller's
          # builders.rs checks this env var and, if set, injects a
          # hostPath cov volume + env into the worker pod (phase3a).
          environment = controllerEnv // covEnv;
          serviceConfig = {
            ExecStart = "${rio-workspace}/bin/rio-controller";
            Restart = "on-failure";
            RestartSec = 5;
          };
        };

        tmpfiles.rules = covTmpfiles;
      };

      environment.systemPackages = [
        pkgs.curl
        pkgs.kubectl
      ];

      virtualisation = {
        memorySize = 4096 + covMemBump;
        cores = 8;
        diskSize = 8192;
      };
    };

  # ── Coverage profraw collection (appended to end of testScript) ─────
  #
  # When coverage=true, stops all rio services (SIGTERM → graceful
  # drain via shutdown_signal → atexit → LLVM profraw flush), tars
  # /var/lib/rio/cov, and copies to $out/coverage/<node>/.
  #
  # `pyNodeVars` is a Python expression evaluating to a list of Machine
  # instances — typically comma-separated node var names, e.g.,
  # "gateway, client" or "control, worker1, worker2, client".
  #
  # systemctl stop is synchronous (returns when unit inactive). || true
  # tolerates missing services (not every node runs every service).
  # For k8s nodes (phase3a), also deletes STS so the pod terminates
  # cleanly (pod PID 1 gets SIGTERM → worker's existing drain →
  # profraw flushed to hostPath mount).
  collectCoverage =
    pyNodeVars:
    if !coverage then
      ""
    else
      ''
        with subtest("collect coverage profraws"):
            for n in [${pyNodeVars}]:
                n.execute(
                    "systemctl stop rio-gateway rio-scheduler rio-store "
                    "rio-worker rio-controller 2>/dev/null || true"
                )
                # k3s worker pod (phase3a only): delete STS, wait for
                # pod termination. --wait blocks until pod is gone.
                n.execute(
                    "[ -f /etc/rancher/k3s/k3s.yaml ] && "
                    "k3s kubectl delete sts --all --wait=true --timeout=30s "
                    "2>/dev/null || true"
                )
                # Empty tarball if dir doesn't exist (e.g., client node
                # runs no rio services).
                n.execute(
                    "mkdir -p /var/lib/rio/cov && "
                    "tar czf /tmp/profraw.tar.gz -C /var/lib/rio/cov . "
                    "2>/dev/null || "
                    "tar czf /tmp/profraw.tar.gz --files-from=/dev/null"
                )
                n.copy_from_vm("/tmp/profraw.tar.gz", f"coverage/{n.name}")
      '';

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
