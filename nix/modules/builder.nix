{
  config,
  lib,
  ...
}:
let
  cfg = config.services.rio.worker;
  rioLib = import ./_common.nix { inherit lib config; };
in
{
  imports = [
    ./common.nix
    # FUSE_PASSTHROUGH-capable kernel (>= 6.9) + fuse/overlay modules —
    # the same module the EKS AMI imports, so VM-test worker nodes run
    # the kernel shape the castore-FUSE passthrough path needs.
    ../nixos-node/kernel.nix
  ];

  options.services.rio.worker = {
    enable = lib.mkEnableOption "rio-builder build executor with a per-build castore-FUSE store";

    workerId = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Worker ID (`RIO_EXECUTOR_ID`). Defaults to hostname if unset.
        Two workers with the same ID will steal each other's builds via
        heartbeat merging — ensure uniqueness across the cluster.
      '';
    };

    schedulerAddr = rioLib.mkGrpcAddrOption "scheduler" "Builder pulls its assignment and reports its outcome here (PullAssignment/ReportOutcome unaries).";
    storeAddr = rioLib.mkGrpcAddrOption "store" "Castore-FUSE open() fetches blobs/chunks from here; executor uploads outputs via PutPathChunked.";

    overlayBaseDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/rio/overlays";
      description = ''
        Per-build overlayfs base directory (`RIO_OVERLAY_BASE_DIR`).
        Must be on a different filesystem than the castore-FUSE lower
        (kernel rejects overlay where FUSE lower + upper share st_dev).
        In VM tests, the rootfs ext4 is fine.
      '';
    };

    metricsAddr = rioLib.mkMetricsOption 9093;

    # rio-mountd: the privileged per-node broker the builder dials once
    # per build (Mount{build_id} → /dev/fuse fd over SCM_RIGHTS). On
    # EKS this is the helm DaemonSet; on the standalone module path it
    # is a host systemd service with the same CLI surface.
    mountd = {
      allowedGid = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 0;
        description = ''
          Group that owns the rio-mountd UDS (`--allowed-gid`; socket
          created mode 0660 root:<gid>). The socket file permission is
          mountd's only access control, so it must match the
          rio-builder service's primary group; the module runs the
          builder as root, so the default is gid 0. The k8s deployment
          uses 990 (the executor pods' runAsGroup) instead.
        '';
      };

      stagingQuotaBytes = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 0;
        description = ''
          Kernel-enforced per-build staging quota in bytes
          (`--staging-quota-bytes`). Requires /var/rio/staging to live
          on an XFS filesystem mounted with `prjquota`; on any other
          filesystem a non-zero value fails every Mount. The module
          default is 0 (quota disabled) because the standalone path
          keeps /var/rio on the root filesystem; production (EKS AMI +
          helm DaemonSet) always sets a non-zero quota on the prjquota
          XFS the AMI provisions (instance-store stripe via
          rio-kubelet-mount, or the dedicated EBS volume via
          rio-ebs-mount on EBS-only nodes).
        '';
      };

      metricsAddr = rioLib.mkMetricsOption 9095;
    };
  };

  config = lib.mkIf cfg.enable {
    # Worker spawns `nix-daemon --stdio` per build; need nix binary + nixbld users.
    nix.enable = lib.mkDefault true;
    nix.settings.sandbox = lib.mkDefault true;

    # ── rio-mountd: privileged castore-FUSE broker ───────────────────
    # Mirrors the flags the vm-mountd scenario and the helm DaemonSet
    # pass: socket + the four /var/rio dirs + quota + gid gate +
    # metrics. Runs as root (CAP_SYS_ADMIN for mount(2) and the
    # FUSE_DEV_IOC_BACKING_OPEN ioctl).
    systemd = {
      services.rio-mountd = rioLib.mkRioService {
        binary = "rio-mountd";
        description = "rio-mountd privileged castore-FUSE broker";
        # Coverage-mode VM tests merge LLVM_PROFILE_FILE in via
        # nix/tests/common.nix mkWorkerNode (same as rio-builder); the
        # module itself stays coverage-agnostic.
        environment = { };
        serviceConfig = {
          ExecStart = lib.concatStringsSep " " [
            "${config.services.rio.package}/bin/rio-mountd"
            "--socket /run/rio-mountd.sock"
            "--castore-dir /var/rio/castore"
            "--staging-dir /var/rio/staging"
            "--cache-dir /var/rio/cache"
            "--chunks-dir /var/rio/chunks"
            "--staging-quota-bytes ${toString cfg.mountd.stagingQuotaBytes}"
            "--allowed-gid ${toString cfg.mountd.allowedGid}"
            "--metrics-addr ${cfg.mountd.metricsAddr}"
          ];
          Restart = "always";
          RestartSec = "1s";
        };
      };

      services.rio-builder =
        rioLib.mkRioService {
          binary = "rio-builder";
          description = "rio-builder build executor with a per-build castore-FUSE store";
          # Builds need the broker's socket; ordering avoids a connect-
          # refused churn at boot (Restart=always covers the residual
          # race anyway — the socket is only dialed at build time).
          extraAfter = [ "rio-mountd.service" ];
          # Env var naming: the config loader strips `RIO_` then lowercases to
          # match the Config field; `__` nests. `RIO_STORE__ADDR` -> `store.addr`.
          # Castore knobs (mountd_socket, castore/staging/cache/chunks dirs,
          # stream_threshold, …) are left at the binary defaults, which match
          # the rio-mountd flags above.
          environment = {
            RIO_SCHEDULER__ADDR = cfg.schedulerAddr;
            RIO_STORE__ADDR = cfg.storeAddr;
            RIO_OVERLAY_BASE_DIR = cfg.overlayBaseDir;
            RIO_METRICS_ADDR = cfg.metricsAddr;
          }
          // lib.optionalAttrs (cfg.workerId != null) {
            RIO_EXECUTOR_ID = cfg.workerId;
          };
          serviceConfig = {
            # The worker runs as root (no User=), so CAP_SYS_ADMIN is already
            # available for the per-build overlay mount, fusectl, and
            # CLONE_NEWNS in pre_exec. The /dev/fuse fd itself comes from
            # rio-mountd over SCM_RIGHTS, but keep the device allowed so the
            # device cgroup never gets in the way of FUSE plumbing.
            # We do NOT narrow CapabilityBoundingSet: the spawned `nix-daemon
            # --stdio` child inherits the bounding set, and its sandbox setup
            # needs CAP_SETUID/SETGID (nixbld users), CAP_CHOWN (output ownership),
            # CAP_SYS_CHROOT (sandbox chroot), CAP_MKNOD (/dev nodes), etc.
            DeviceAllow = [ "/dev/fuse rw" ];

            # cgroup v2 per-build resource tracking. The worker creates a
            # sub-cgroup per build and moves the spawned nix-daemon into
            # it. memory.peak + polled cpu.stat give tree-wide peak memory
            # and CPU. Per-PID VmHWM would only capture nix-daemon's own
            # RSS (~10MB) — the builder is a fork()ed child whose footprint
            # never appears there.
            #
            # Delegate=yes: grants the service ownership of its cgroup
            # subtree. Without this, cgroup.subtree_control writes fail
            # EACCES and the worker DIES AT STARTUP (cgroup v2 is a hard
            # requirement — no broken-metrics fallback).
            #
            # DelegateSubgroup=builds: systemd v254+. cgroup v2 forbids a
            # cgroup having BOTH processes AND sub-cgroups with enabled
            # controllers (the "no internal processes" rule). This makes
            # systemd run the worker in a `builds/` SUB-cgroup of the
            # service cgroup, leaving the service cgroup EMPTY.
            #
            # The worker's delegated_root() reads /proc/self/cgroup (which
            # points to `.../builds/`) and returns the PARENT
            # (`.../rio-builder.service/`). Per-build cgroups are created
            # there as SIBLINGS of `builds/` — the service cgroup is
            # empty, so enabling +memory +cpu on it succeeds; `builds/`
            # has the worker process but no controller-enabled children
            # (per-build cgroups are not under it). No rule violation.
            #
            # /sys/fs/cgroup/system.slice/rio-builder.service/:
            #   cgroup.subtree_control  ← worker writes "+memory +cpu" (EMPTY cgroup: no EBUSY)
            #   builds/                 ← DelegateSubgroup; worker PID lives here
            #   <drv-hash>/             ← per-build SIBLING (nix-daemon PID → forks builder)
            #     memory.peak           ← tree-wide peak, read at build end
            #     cpu.stat              ← tree-wide cumulative, polled 1Hz
            Delegate = "yes";
            DelegateSubgroup = "builds";
            # The builder is one-shot: exits cleanly after completing a
            # build. systemd respawns it for the next assignment — same
            # role the k8s controller plays with Jobs. Also covers startup
            # races (scheduler not ready → connect refused → exit).
            Restart = "always";
            RestartSec = "1s";
          };
        }
        // {
          # The builder is one-shot and exits cleanly per build; systemd
          # respawns it (Restart=always below). Default StartLimitBurst=5
          # in 10s trips under fanout (50 builds → ~13 rapid restarts per
          # worker). Must be at the unit level — `StartLimitIntervalSec`
          # in [Service] is silently ignored since systemd 230.
          startLimitIntervalSec = 0;

          # nix-daemon --stdio must be on PATH.
          path = [ config.nix.package ];
        };

      # Ensure /var/rio/* directories exist (rio-mountd creates its four
      # dirs too, but this runs earlier and sets correct permissions).
      # Also create the bind-mount TARGETS that executor pre_exec mounts
      # onto: `/nix/var/nix/db` is created by nix-daemon on first local
      # use, but the worker never runs local nix-daemon (only the
      # namespaced --stdio child), so that path may not exist.
      tmpfiles.rules = [
        "d /var/rio 0755 root root -"
        "d /var/rio/castore 0755 root root -"
        "d /var/rio/staging 0755 root root -"
        "d /var/rio/cache 0755 root root -"
        "d /var/rio/chunks 0755 root root -"
        "d ${cfg.overlayBaseDir} 0755 root root -"
        # Bind-mount targets for executor pre_exec (spawn_daemon_in_namespace).
        # /nix/store and /etc/nix are created by NixOS activation, but
        # /nix/var/nix/db is lazy-created by the local nix-daemon. tmpfiles `d`
        # does NOT create parents, so list the full chain explicitly.
        "d /nix/var 0755 root root -"
        "d /nix/var/nix 0755 root root -"
        "d /nix/var/nix/db 0755 root root -"
      ];
    };
  };
}
