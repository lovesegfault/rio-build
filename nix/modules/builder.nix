{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.rio.worker;
  rioLib = import ./_common.nix { inherit lib config; };
in
{
  imports = [ ./common.nix ];

  options.services.rio.worker = {
    enable = lib.mkEnableOption "rio-builder build executor with FUSE store";

    workerId = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Worker ID (`RIO_EXECUTOR_ID`). Defaults to hostname if unset.
        Two workers with the same ID will steal each other's builds via
        heartbeat merging — ensure uniqueness across the cluster.
      '';
    };

    schedulerAddr = rioLib.mkGrpcAddrOption "scheduler" "Worker opens a bidirectional BuildExecution stream at startup.";
    storeAddr = rioLib.mkGrpcAddrOption "store" "FUSE fetches from here; executor uploads outputs via PutPath.";

    fuseMountPoint = lib.mkOption {
      type = lib.types.path;
      default = "/var/rio/fuse-store";
      description = ''
        FUSE mount point for the lazy-fetch store view (`RIO_FUSE_MOUNT_POINT`).

        Do NOT use `/nix/store`: the per-build sandbox gets its own mount
        namespace with the overlay bind-mounted at `/nix/store` (executor
        `pre_exec`), so the FUSE mount location is arbitrary and should not
        shadow the host store.
      '';
    };

    fuseCacheDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/rio/cache";
      description = "FUSE local cache directory (`RIO_FUSE_CACHE_DIR`).";
    };

    overlayBaseDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/rio/overlays";
      description = ''
        Per-build overlayfs base directory (`RIO_OVERLAY_BASE_DIR`).
        Must be on a different filesystem than the FUSE mount (kernel rejects
        overlay where FUSE lower + upper share st_dev). In VM tests, the
        rootfs ext4 is fine; do NOT point this at the FUSE mount's subtree.
      '';
    };

    metricsAddr = rioLib.mkMetricsOption 9093;
  };

  config = lib.mkIf cfg.enable {
    # FUSE + overlayfs kernel support.
    boot.kernelModules = [
      "fuse"
      "overlay"
    ];
    # fuse/mod.rs uses SessionACL::All (allow_other); requires /etc/fuse.conf
    # `user_allow_other`. This option sets that flag.
    programs.fuse.userAllowOther = true;

    systemd.services.rio-builder =
      rioLib.mkRioService {
        binary = "rio-builder";
        description = "rio-builder build executor with FUSE store";
        # Env var naming: the config loader strips `RIO_` then lowercases to
        # match the Config field; `__` nests. `RIO_STORE__ADDR` -> `store.addr`.
        environment = {
          RIO_SCHEDULER__ADDR = cfg.schedulerAddr;
          RIO_STORE__ADDR = cfg.storeAddr;
          RIO_FUSE_MOUNT_POINT = cfg.fuseMountPoint;
          RIO_FUSE_CACHE_DIR = cfg.fuseCacheDir;
          RIO_OVERLAY_BASE_DIR = cfg.overlayBaseDir;
          RIO_METRICS_ADDR = cfg.metricsAddr;
        }
        // lib.optionalAttrs (cfg.workerId != null) {
          RIO_EXECUTOR_ID = cfg.workerId;
        };
        serviceConfig = {
          # The worker runs as root (no User=), so CAP_SYS_ADMIN is already
          # available for FUSE mount, overlayfs, and the build sandbox's
          # mount namespace. We do NOT narrow CapabilityBoundingSet: the
          # sandbox child needs CAP_SETUID/SETGID (drop to the build
          # user), CAP_CHOWN (output ownership), CAP_SYS_CHROOT
          # (pivot_root/chroot), CAP_NET_ADMIN (loopback in the netns).
          # Allow opening /dev/fuse (device cgroup allowlist; DevicePolicy=auto
          # so pseudo-devices like /dev/null are always allowed).
          DeviceAllow = [ "/dev/fuse rw" ];

          # cgroup v2 per-build resource tracking. The worker creates a
          # sub-cgroup per build and moves the sandboxed build process
          # into it. memory.peak + polled cpu.stat give tree-wide peak
          # memory and CPU across every child the build forks.
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
          #   <drv-hash>/             ← per-build SIBLING (the sandboxed build's process tree)
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
          # Unmount FUSE on shutdown (best-effort; the fuser background session
          # holds it, but the kernel detaches on process exit anyway).
          ExecStopPost = "-${pkgs.util-linux}/bin/umount -l ${cfg.fuseMountPoint}";
        };
      }
      // {
        # The builder is one-shot and exits cleanly per build; systemd
        # respawns it (Restart=always below). Default StartLimitBurst=5
        # in 10s trips under fanout (50 builds → ~13 rapid restarts per
        # worker). Must be at the unit level — `StartLimitIntervalSec`
        # in [Service] is silently ignored since systemd 230.
        startLimitIntervalSec = 0;

        # fuse3 provides fusermount3, required by the fuser crate's
        # MountOption::AutoUnmount. mount/umount come from util-linux in
        # the systemd unit's default PATH.
        path = [
          pkgs.fuse3
        ];

        environment = {
          # Static /bin/sh exposed inside every build sandbox.
          RIO_SANDBOX_SHELL = "${pkgs.pkgsStatic.busybox}/bin/sh";
          # CA bundle mounted into network (fixed-output) sandboxes.
          RIO_CA_BUNDLE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
        };
      };

    # Ensure /var/rio/* directories exist (worker creates them too, but this
    # runs earlier and sets correct permissions).
    systemd.tmpfiles.rules = [
      "d /var/rio 0755 root root -"
      "d ${cfg.fuseCacheDir} 0755 root root -"
      "d ${cfg.overlayBaseDir} 0755 root root -"
      "d ${cfg.fuseMountPoint} 0755 root root -"
    ];
  };
}
