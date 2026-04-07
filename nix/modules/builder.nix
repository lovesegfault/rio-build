{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.rio.worker;
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

    schedulerAddr = lib.mkOption {
      type = lib.types.str;
      description = ''
        rio-scheduler gRPC address as `host:port` (NOT a URL — main.rs prepends `http://`).
        Worker opens a bidirectional BuildExecution stream at startup.
      '';
    };

    storeAddr = lib.mkOption {
      type = lib.types.str;
      description = ''
        rio-store gRPC address as `host:port` (NOT a URL — main.rs prepends `http://`).
        FUSE fetches from here; executor uploads outputs via PutPath.
      '';
    };

    fuseMountPoint = lib.mkOption {
      type = lib.types.path;
      default = "/var/rio/fuse-store";
      description = ''
        FUSE mount point for the lazy-fetch store view (`RIO_FUSE_MOUNT_POINT`).

        Do NOT use `/nix/store`: the per-build nix-daemon gets its own mount
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

    metricsAddr = lib.mkOption {
      type = lib.types.str;
      default = "[::]:9093";
      description = "Prometheus metrics listen address (`RIO_METRICS_ADDR`).";
    };

    sizeClass = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = ''
        Size-class this worker is deployed as (`RIO_SIZE_CLASS`). Matches
        a name in the scheduler's `size_classes` config. Empty = wildcard
        (accepts any-class work). Scheduler routes builds by estimated
        duration; this declares which bucket this worker serves.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # Worker spawns `nix-daemon --stdio` per build; need nix binary + nixbld users.
    nix.enable = lib.mkDefault true;
    nix.settings.sandbox = lib.mkDefault true;

    # FUSE + overlayfs kernel support.
    boot.kernelModules = [
      "fuse"
      "overlay"
    ];
    # fuse/mod.rs uses SessionACL::All (allow_other); requires /etc/fuse.conf
    # `user_allow_other`. This option sets that flag.
    programs.fuse.userAllowOther = true;

    systemd.services.rio-builder = {
      description = "rio-builder build executor with FUSE store";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      # nix-daemon --stdio must be on PATH. fuse3 provides fusermount3,
      # required by the fuser crate's MountOption::AutoUnmount.
      path = [
        config.nix.package
        pkgs.fuse3
      ];

      # Env var naming: figment strips `RIO_` then lowercases to match
      # the Config field. `RIO_STORE_ADDR` -> `store_addr`, etc.
      environment = {
        RIO_SCHEDULER_ADDR = cfg.schedulerAddr;
        RIO_STORE_ADDR = cfg.storeAddr;
        RIO_FUSE_MOUNT_POINT = cfg.fuseMountPoint;
        RIO_FUSE_CACHE_DIR = cfg.fuseCacheDir;
        RIO_OVERLAY_BASE_DIR = cfg.overlayBaseDir;
        RIO_METRICS_ADDR = cfg.metricsAddr;
        RIO_LOG_FORMAT = config.services.rio.logFormat;
      }
      // lib.optionalAttrs (cfg.sizeClass != "") {
        RIO_SIZE_CLASS = cfg.sizeClass;
      }
      // lib.optionalAttrs (cfg.workerId != null) {
        RIO_EXECUTOR_ID = cfg.workerId;
      };

      serviceConfig = {
        ExecStart = "${config.services.rio.package}/bin/rio-builder";
        # The worker runs as root (no User=), so CAP_SYS_ADMIN is already
        # available for FUSE mount, overlayfs, and CLONE_NEWNS in pre_exec.
        # We do NOT narrow CapabilityBoundingSet: the spawned `nix-daemon
        # --stdio` child inherits the bounding set, and its sandbox setup
        # needs CAP_SETUID/SETGID (nixbld users), CAP_CHOWN (output ownership),
        # CAP_SYS_CHROOT (sandbox chroot), CAP_MKNOD (/dev nodes), etc.
        # Allow opening /dev/fuse (device cgroup allowlist; DevicePolicy=auto
        # so pseudo-devices like /dev/null are always allowed).
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
        # Default StartLimitBurst (5 in 10s) is too tight for startup
        # races: worker starts before cross-VM scheduler → connect
        # refused → exit → restart. A few of these in quick succession
        # hits the limit and systemd gives up. 0 = unlimited restarts.
        StartLimitIntervalSec = "0";
        # Unmount FUSE on shutdown (best-effort; the fuser background session
        # holds it, but the kernel detaches on process exit anyway).
        ExecStopPost = "-${pkgs.util-linux}/bin/umount -l ${cfg.fuseMountPoint}";
      };
    };

    # Ensure /var/rio/* directories exist (worker creates them too, but this
    # runs earlier and sets correct permissions). Also create the bind-mount
    # TARGETS that executor pre_exec mounts onto: `/nix/var/nix/db` is created
    # by nix-daemon on first local use, but the worker never runs local
    # nix-daemon (only the namespaced --stdio child), so that path may not exist.
    systemd.tmpfiles.rules = [
      "d /var/rio 0755 root root -"
      "d ${cfg.fuseCacheDir} 0755 root root -"
      "d ${cfg.overlayBaseDir} 0755 root root -"
      "d ${cfg.fuseMountPoint} 0755 root root -"
      # Bind-mount targets for executor pre_exec (spawn_daemon_in_namespace).
      # /nix/store and /etc/nix are created by NixOS activation, but
      # /nix/var/nix/db is lazy-created by the local nix-daemon. tmpfiles `d`
      # does NOT create parents, so list the full chain explicitly.
      "d /nix/var 0755 root root -"
      "d /nix/var/nix 0755 root root -"
      "d /nix/var/nix/db 0755 root root -"
    ];
  };
}
